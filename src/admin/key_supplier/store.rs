use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, types::Type};

const MAX_MESSAGE_CHARS: usize = 2000;

/// `row_to_event` 的列顺序。所有单行查询共用，避免手抄漏列。
const EVENT_COLUMNS: &str = "id,supplier_id,event_id,event_type,purchase_order_id,message,quantity,\
received_at,status,attempts,last_error,purchased,imported,duplicate_count,\
webhook_duplicate_count,failed_count,read_at,processing_started_at,supplier_batch_id,\
total_debit,unit_price,supplier_order_id,replayed,\
pool_usable,pool_deficit,pool_requested,retry_after,purchase_count";

/// 历史行（单供货商时代）回填的供货商标识，与配置迁移出来的条目 id 一致。
pub const LEGACY_SUPPLIER_ID: &str = "default";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS supplier_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    supplier_id TEXT NOT NULL DEFAULT 'default',
    event_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    purchase_order_id TEXT,
    message TEXT,
    quantity INTEGER NOT NULL,
    received_at TEXT NOT NULL,
    status TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    purchased INTEGER NOT NULL DEFAULT 0,
    imported INTEGER NOT NULL DEFAULT 0,
    duplicate_count INTEGER NOT NULL DEFAULT 0,
    webhook_duplicate_count INTEGER NOT NULL DEFAULT 0,
    failed_count INTEGER NOT NULL DEFAULT 0,
    read_at TEXT,
    processing_started_at TEXT,
    supplier_batch_id TEXT,
    total_debit INTEGER,
    unit_price REAL,
    supplier_order_id TEXT,
    replayed INTEGER NOT NULL DEFAULT 0,
    pool_usable INTEGER,
    pool_deficit INTEGER,
    pool_requested INTEGER,
    retry_after TEXT,
    purchase_count INTEGER
);
"#;

const INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_supplier_events_queue ON supplier_events(status, id);
CREATE INDEX IF NOT EXISTS idx_supplier_events_read ON supplier_events(read_at);
CREATE INDEX IF NOT EXISTS idx_supplier_events_supplier ON supplier_events(supplier_id, id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_supplier_events_supplier_event_unique
    ON supplier_events(supplier_id, event_id);
"#;

/// 多供货商改造前建的唯一索引（只锁 `event_id`）。重建后必须删掉，
/// 否则两家供货商推同名 event id 会被误判成重复而丢单。
const LEGACY_EVENT_ID_INDEX: &str = "idx_supplier_events_event_id_unique";

const MIGRATION_COLUMNS: &[(&str, &str)] = &[
    ("purchase_order_id", "TEXT"),
    ("message", "TEXT"),
    ("quantity", "INTEGER NOT NULL DEFAULT 0"),
    ("attempts", "INTEGER NOT NULL DEFAULT 0"),
    ("last_error", "TEXT"),
    ("purchased", "INTEGER NOT NULL DEFAULT 0"),
    ("imported", "INTEGER NOT NULL DEFAULT 0"),
    ("duplicate_count", "INTEGER NOT NULL DEFAULT 0"),
    ("webhook_duplicate_count", "INTEGER NOT NULL DEFAULT 0"),
    ("failed_count", "INTEGER NOT NULL DEFAULT 0"),
    ("read_at", "TEXT"),
    ("processing_started_at", "TEXT"),
    ("supplier_id", "TEXT NOT NULL DEFAULT 'default'"),
    // 供货商侧开号批次号（kiroapp-io 的 order_id），可空：其它协议没有批次概念。
    ("supplier_batch_id", "TEXT"),
    // 本单实际扣费（供货商积分）。阶梯定价下这是唯一权威数字，不能用单价 × 数量反推。
    // 没有它就无法做跨供货商预算封顶，也算不出「每存活小时成本」。可空：历史行没有，
    // 且 kiro-rs 协议不返回扣费。
    ("total_debit", "INTEGER"),
    // 本单均价 = total_debit / purchased。对方直接返回，落库省一次除法与精度纠缠。
    ("unit_price", "REAL"),
    // 供货商侧订单号（采购**响应**里的 order_id）。与 `supplier_batch_id`（推送里的
    // 批次号）不是一回事：这个用来跟对方的订单历史对账，查「钱花了有没有拿到货」。
    ("supplier_order_id", "TEXT"),
    // 幂等重放标记。为真说明上一次其实已经成交，只是响应没回到我们手上——
    // 那一次对应的事件很可能停在 failed，可据此自动对账出「假失败」。
    ("replayed", "INTEGER NOT NULL DEFAULT 0"),
    // 以下三列是全局号池闸的水位快照，回答「为什么买了这么多」和「为什么没买」。
    // 只在号池闸启用时写入，其余情况（含历史行）为 NULL。
    //
    // 触发时全局可用的采购凭据数。
    ("pool_usable", "INTEGER"),
    // 触发时的缺口 = 目标存量 - pool_usable，下界 0。
    ("pool_deficit", "INTEGER"),
    // 经单家上下限与库存夹逼后实际请求的数量。与 `pool_deficit` 的差额说明是被
    // 哪一道夹逼砍掉的。
    ("pool_requested", "INTEGER"),
    // 最早可再次领取的时间（RFC3339）。瞬时上游故障把事件压回队列时写入，
    // 到点前 `claim_next` 不会捡它。为空 = 立即可领。
    ("retry_after", "TEXT"),
    // 上一轮实际发出去的采购数量。重放必须原样重发：`purchase_order_id` 由
    // `event_id` 派生，同一订单号换数量会让幂等协议返 409（原单已成交、钱扣了、
    // key 没到手），那恰好是重试要避免的结果。
    ("purchase_count", "INTEGER"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupplierEventStatus {
    Received,
    Processing,
    Succeeded,
    Skipped,
    Failed,
}

impl SupplierEventStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Processing => "processing",
            Self::Succeeded => "succeeded",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }

    fn from_db(value: &str, column: usize) -> rusqlite::Result<Self> {
        match value {
            "received" => Ok(Self::Received),
            "processing" => Ok(Self::Processing),
            "succeeded" => Ok(Self::Succeeded),
            "skipped" => Ok(Self::Skipped),
            "failed" => Ok(Self::Failed),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                column,
                Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown supplier event status: {other}"),
                )),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingSupplierEvent {
    pub supplier_id: String,
    pub event_id: String,
    pub event_type: String,
    pub purchase_order_id: Option<String>,
    /// 供货商侧的开号批次号，采购时回传以定向拉取该批次产出。仅 `kiroapp-io` 有值。
    pub supplier_batch_id: Option<String>,
    pub message: Option<String>,
    pub quantity: i64,
}

// 不派生 `Eq`：`unit_price` 是 f64。金额本来就不该拿来做等价判定，
// 需要比较的场景（测试断言）用 `PartialEq` 就够。
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSupplierEvent {
    pub id: i64,
    pub supplier_id: String,
    pub event_id: String,
    pub event_type: String,
    pub purchase_order_id: Option<String>,
    pub supplier_batch_id: Option<String>,
    pub message: Option<String>,
    pub quantity: i64,
    pub received_at: String,
    pub status: SupplierEventStatus,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub purchased_count: i64,
    pub imported_count: i64,
    pub duplicate_count: i64,
    pub webhook_duplicate_count: i64,
    pub failed_count: i64,
    pub read_at: Option<String>,
    pub processing_started_at: Option<String>,
    /// 本单实际扣费（供货商积分）。`None` = 该协议不返回扣费，或这条事件没花钱。
    pub total_debit: Option<i64>,
    /// 本单均价 = `total_debit / purchased_count`。
    pub unit_price: Option<f64>,
    /// 供货商侧订单号，用来和对方订单历史对账。
    pub supplier_order_id: Option<String>,
    /// 本单命中了对方的幂等重放（说明上一次其实已成交）。
    pub replayed: bool,
    /// 触发时全局可用的采购凭据数。`None` = 号池闸未启用。
    pub pool_usable: Option<i64>,
    /// 触发时的缺口（目标存量 - `pool_usable`，下界 0）。
    pub pool_deficit: Option<i64>,
    /// 经夹逼后实际请求的数量。与 `pool_deficit` 的差额说明被哪道夹逼砍掉了。
    pub pool_requested: Option<i64>,
    /// 最早可再次领取的时间。`Some` 说明这条事件正在等一次自动重试。
    pub retry_after: Option<String>,
    /// 上一轮实际发出去的采购数量。`Some` 说明本次必须原样重放这个数量。
    pub purchase_count: Option<i64>,
}

/// 一次处理的结果。`Default` 是「什么都没发生」，构造时只填关心的字段。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessSummary {
    pub purchased_count: i64,
    pub imported_count: i64,
    pub duplicate_count: i64,
    pub failed_count: i64,
    pub message: Option<String>,
    /// 实际扣费。`None` 表示「不知道/没花钱」，落库时不覆盖已有值。
    pub total_debit: Option<i64>,
    pub unit_price: Option<f64>,
    pub supplier_order_id: Option<String>,
    /// 命中幂等重放。只会从 false 翻成 true，不会被后续写回抹掉。
    pub replayed: bool,
    /// 号池闸的水位快照。三者必须在成功、跳过、失败三条路径上都落库——
    /// 「为什么没买」正是它们要回答的问题，只在成功时记录等于没记录。
    pub pool_usable: Option<i64>,
    pub pool_deficit: Option<i64>,
    pub pool_requested: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SupplierEventPage {
    pub items: Vec<StoredSupplierEvent>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InsertOutcome {
    Inserted(StoredSupplierEvent),
    Duplicate(StoredSupplierEvent),
}

pub struct SupplierEventStore {
    conn: Mutex<Connection>,
}

impl SupplierEventStore {
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            }
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        initialize_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        initialize_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 退出前把 WAL 截断。库本体只有几百 KB，WAL 却能涨到几 MB——同样是
    /// PASSIVE 自动检查点只复用不缩文件、硬退出又从不截断造成的。
    ///
    /// 拿不到写锁就放弃：卡住退出比留着一个大 WAL 严重得多。
    pub fn checkpoint_truncate(&self) -> rusqlite::Result<()> {
        // 锁中毒说明某个持有者 panic 过。此时数据可能不一致，但截断 WAL 本身无害，
        // 而且这是退出路径——放弃比传播 panic 好。
        let Ok(conn) = self.conn.lock() else {
            return Ok(());
        };
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
    }

    /// 落库一条 webhook 事件。`(supplier_id, event_id)` 唯一，重复推送只累加
    /// `webhook_duplicate_count`，绝不产生第二条待处理事件——这是「不重复购买」的第一道闸。
    pub fn insert_event(&self, event: IncomingSupplierEvent) -> rusqlite::Result<InsertOutcome> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let received_at = Utc::now().to_rfc3339();
        let message = event
            .message
            .map(|value| truncate_chars(&value, MAX_MESSAGE_CHARS));
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO supplier_events
             (supplier_id,event_id,event_type,purchase_order_id,message,quantity,received_at,status,
              supplier_batch_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,'received',?8)",
            params![
                event.supplier_id,
                event.event_id,
                event.event_type,
                event.purchase_order_id,
                message,
                event.quantity,
                received_at,
                event.supplier_batch_id
            ],
        )?;
        if inserted == 0 {
            tx.execute(
                "UPDATE supplier_events SET webhook_duplicate_count=webhook_duplicate_count+1
                 WHERE supplier_id=?1 AND event_id=?2",
                params![event.supplier_id, event.event_id],
            )?;
        }
        tx.commit()?;
        let stored = Self::query_by_event_id(&conn, &event.supplier_id, &event.event_id)?
            .expect("inserted or duplicate event must be queryable");
        if inserted == 1 {
            Ok(InsertOutcome::Inserted(stored))
        } else {
            Ok(InsertOutcome::Duplicate(stored))
        }
    }

    pub fn claim_next(&self) -> rusqlite::Result<Option<StoredSupplierEvent>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // `retry_after` 未到点的事件跳过而不是阻塞队列：它后面那些新到货的通知
        // 还得抢货，不能被一条正在等退避的事件挡住。
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM supplier_events
                 WHERE status='received' AND (retry_after IS NULL OR retry_after <= ?1)
                 ORDER BY id ASC LIMIT 1",
                params![Utc::now().to_rfc3339()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            tx.commit()?;
            return Ok(None);
        };
        let stored = Self::claim_in_transaction(&tx, id)?;
        tx.commit()?;
        Ok(stored)
    }

    pub fn claim_by_event_id(
        &self,
        supplier_id: &str,
        event_id: &str,
    ) -> rusqlite::Result<Option<StoredSupplierEvent>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM supplier_events
                 WHERE supplier_id=?1 AND event_id=?2 AND status='received'
                   AND retry_after IS NULL",
                params![supplier_id, event_id],
                |row| row.get(0),
            )
            .optional()?;
        let stored = match id {
            Some(id) => Self::claim_in_transaction(&tx, id)?,
            None => None,
        };
        tx.commit()?;
        Ok(stored)
    }

    pub fn complete(&self, id: i64, summary: ProcessSummary) -> rusqlite::Result<()> {
        self.transition_processing(
            id,
            "succeeded",
            ProcessSummary {
                message: summary
                    .message
                    .map(|value| truncate_chars(&value, MAX_MESSAGE_CHARS)),
                ..summary
            },
        )
    }

    pub fn skip(&self, id: i64, message: Option<&str>) -> rusqlite::Result<()> {
        self.skip_with_summary(id, message, ProcessSummary::default())
    }

    /// 跳过并附带一份 summary。号池闸用它把水位快照写进跳过的事件——
    /// 「为什么没买」正是那三个数要回答的问题，只在成功时记录等于没记录。
    ///
    /// `summary` 里的计数字段会被清零：跳过意味着一个都没买、没导入、没失败。
    /// 只有金额与水位这些「解释性」字段被保留，且走 `COALESCE` 只写不抹。
    pub fn skip_with_summary(
        &self,
        id: i64,
        message: Option<&str>,
        summary: ProcessSummary,
    ) -> rusqlite::Result<()> {
        self.transition_processing(
            id,
            "skipped",
            ProcessSummary {
                purchased_count: 0,
                imported_count: 0,
                duplicate_count: 0,
                failed_count: 0,
                message: message.map(|value| truncate_chars(value, MAX_MESSAGE_CHARS)),
                ..summary
            },
        )
    }

    pub fn fail(&self, id: i64, error: &str) -> rusqlite::Result<()> {
        self.fail_with_summary(
            id,
            ProcessSummary {
                failed_count: 1,
                ..Default::default()
            },
            error,
        )
    }

    pub fn fail_with_summary(
        &self,
        id: i64,
        summary: ProcessSummary,
        error: &str,
    ) -> rusqlite::Result<()> {
        self.transition_processing(
            id,
            "failed",
            ProcessSummary {
                message: Some(truncate_chars(error, 300)),
                ..summary
            },
        )
    }

    /// 人工重试。也接受正在等自动重试的事件（`received` + `retry_after`）：
    /// 人明确要求现在就试，不该还让他等退避走完。
    ///
    /// 不清 `purchase_count`：钉住的数量正是为了原样重放。数量变了幂等协议会返 409，
    /// 那单钱可能已经扣了、key 却取不回来。
    pub fn retry(&self, id: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE supplier_events
             SET status='received', processing_started_at=NULL, last_error=NULL, retry_after=NULL
             WHERE id=?1
               AND (status IN ('failed','skipped')
                    OR (status='received' AND retry_after IS NOT NULL))",
            params![id],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(rusqlite::Error::QueryReturnedNoRows)
        }
    }

    /// 把事件压回队列，`retry_after` 到点后再领，并钉住上一轮发出去的采购数量。
    ///
    /// 只用于**瞬时**上游故障（5xx / 网络 / 429）。在这之前这类故障直接进 `failed`，
    /// 而 `failed` 是终态——`claim_next` 只捡 `received`——所以供货商抖动几秒钟就等于
    /// 永久丢一条到货通知，只能靠人去点重试。
    pub fn defer(
        &self,
        id: i64,
        retry_after: DateTime<Utc>,
        purchase_count: Option<u32>,
        error: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE supplier_events
             SET status='received', processing_started_at=NULL, retry_after=?2,
                 purchase_count=COALESCE(?3, purchase_count), last_error=?4
             WHERE id=?1 AND status='processing'",
            params![
                id,
                retry_after.to_rfc3339(),
                purchase_count,
                truncate_chars(error, 300)
            ],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(rusqlite::Error::QueryReturnedNoRows)
        }
    }

    pub fn recover_stale_processing(&self, cutoff: DateTime<Utc>) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE supplier_events SET status='received', processing_started_at=NULL
             WHERE status='processing' AND processing_started_at IS NOT NULL AND processing_started_at < ?1",
            params![cutoff.to_rfc3339()],
        )
    }

    /// 分页读事件。`supplier_id=None` 表示不按供货商过滤（跨供货商总览）。
    pub fn list(
        &self,
        limit: usize,
        before: Option<i64>,
        supplier_id: Option<&str>,
    ) -> rusqlite::Result<SupplierEventPage> {
        let conn = self.conn.lock().unwrap();
        let limit = limit.clamp(1, 200) as i64;
        // 用共享的 EVENT_COLUMNS 而不是手抄一份：列顺序和 `row_to_event` 绑死，
        // 手抄的那份漏列就会在运行时报 InvalidColumnIndex。
        let mut stmt = conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS}
             FROM supplier_events
             WHERE (?1 IS NULL OR id < ?1) AND (?2 IS NULL OR supplier_id = ?2)
             ORDER BY id DESC LIMIT ?3",
        ))?;
        let rows = stmt.query_map(params![before, supplier_id, limit], Self::row_to_event)?;
        Ok(SupplierEventPage {
            items: rows.collect::<rusqlite::Result<_>>()?,
        })
    }

    pub fn unread_count(&self, supplier_id: Option<&str>) -> rusqlite::Result<i64> {
        self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM supplier_events
             WHERE read_at IS NULL AND (?1 IS NULL OR supplier_id = ?1)",
            params![supplier_id],
            |row| row.get(0),
        )
    }

    pub fn mark_read(&self, ids: &[i64]) -> rusqlite::Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut changed = 0;
        let now = Utc::now().to_rfc3339();
        for id in ids {
            changed += tx.execute(
                "UPDATE supplier_events SET read_at=?1 WHERE id=?2 AND read_at IS NULL",
                params![now, id],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn mark_all_read(&self, supplier_id: Option<&str>) -> rusqlite::Result<usize> {
        self.conn.lock().unwrap().execute(
            "UPDATE supplier_events SET read_at=?1
             WHERE read_at IS NULL AND (?2 IS NULL OR supplier_id = ?2)",
            params![Utc::now().to_rfc3339(), supplier_id],
        )
    }

    fn transition_processing(
        &self,
        id: i64,
        status: &str,
        summary: ProcessSummary,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        // 金额三列用 COALESCE、`replayed` 用 CASE：一律「只写不抹」。采购成功但导入失败时
        // 走的是 `fail_with_summary`，钱的字段必须活下来——否则预算累计会把这单算成 0。
        let changed = conn.execute(
            "UPDATE supplier_events SET status=?1, message=COALESCE(?2,message), purchased=?3, imported=?4,
             duplicate_count=?5, failed_count=?6,
             total_debit=COALESCE(?7,total_debit), unit_price=COALESCE(?8,unit_price),
             supplier_order_id=COALESCE(?9,supplier_order_id),
             replayed=CASE WHEN ?10=1 THEN 1 ELSE replayed END,
             pool_usable=COALESCE(?11,pool_usable), pool_deficit=COALESCE(?12,pool_deficit),
             pool_requested=COALESCE(?13,pool_requested),
             last_error=CASE WHEN ?1='failed' THEN ?2 ELSE last_error END, processing_started_at=NULL
             WHERE id=?14 AND status='processing'",
            params![
                status,
                summary.message,
                summary.purchased_count,
                summary.imported_count,
                summary.duplicate_count,
                summary.failed_count,
                summary.total_debit,
                summary.unit_price,
                summary.supplier_order_id,
                i64::from(summary.replayed),
                summary.pool_usable,
                summary.pool_deficit,
                summary.pool_requested,
                id
            ],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(rusqlite::Error::QueryReturnedNoRows)
        }
    }

    fn query_by_event_id(
        conn: &Connection,
        supplier_id: &str,
        event_id: &str,
    ) -> rusqlite::Result<Option<StoredSupplierEvent>> {
        conn.query_row(
            &format!(
                "SELECT {EVENT_COLUMNS} FROM supplier_events WHERE supplier_id=?1 AND event_id=?2"
            ),
            params![supplier_id, event_id],
            Self::row_to_event,
        )
        .optional()
    }

    fn query_by_id(
        conn: &rusqlite::Transaction<'_>,
        id: i64,
    ) -> rusqlite::Result<Option<StoredSupplierEvent>> {
        conn.query_row(
            &format!("SELECT {EVENT_COLUMNS} FROM supplier_events WHERE id=?1"),
            params![id],
            Self::row_to_event,
        )
        .optional()
    }

    fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSupplierEvent> {
        Ok(StoredSupplierEvent {
            id: row.get(0)?,
            supplier_id: row.get(1)?,
            event_id: row.get(2)?,
            event_type: row.get(3)?,
            purchase_order_id: row.get(4)?,
            message: row.get(5)?,
            quantity: row.get(6)?,
            received_at: row.get(7)?,
            status: SupplierEventStatus::from_db(&row.get::<_, String>(8)?, 8)?,
            attempts: row.get(9)?,
            last_error: row.get(10)?,
            purchased_count: row.get(11)?,
            imported_count: row.get(12)?,
            duplicate_count: row.get(13)?,
            webhook_duplicate_count: row.get(14)?,
            failed_count: row.get(15)?,
            read_at: row.get(16)?,
            processing_started_at: row.get(17)?,
            supplier_batch_id: row.get(18)?,
            total_debit: row.get(19)?,
            unit_price: row.get(20)?,
            supplier_order_id: row.get(21)?,
            replayed: row.get::<_, i64>(22)? != 0,
            pool_usable: row.get(23)?,
            pool_deficit: row.get(24)?,
            pool_requested: row.get(25)?,
            retry_after: row.get(26)?,
            purchase_count: row.get(27)?,
        })
    }

    fn claim_in_transaction(
        tx: &rusqlite::Transaction<'_>,
        id: i64,
    ) -> rusqlite::Result<Option<StoredSupplierEvent>> {
        let now = Utc::now().to_rfc3339();
        let changed = tx.execute(
            "UPDATE supplier_events SET status='processing', attempts=attempts+1, processing_started_at=?1, retry_after=NULL WHERE id=?2 AND status='received'",
            params![now, id],
        )?;
        if changed == 1 {
            Ok(Some(
                Self::query_by_id(tx, id)?.expect("claimed event must exist"),
            ))
        } else {
            Ok(None)
        }
    }
}

/// 多供货商改造前的表把 `event_id` 声明成列级 `UNIQUE`，SQLite 无法直接 drop，
/// 会让两家供货商推同名 event id 时误判重复丢单，因此整表重建。
///
/// 重建在 `MIGRATION_COLUMNS` 之后执行，所以**每加一列都要同时改这里的三处**
/// （CREATE 的列定义、INSERT 的列名、SELECT 的列名），否则新列会在重建时被丢掉。
/// `open_rebuilds_the_table_so_two_suppliers_can_share_an_event_id` 守着这一点。
const REBUILD_TABLE: &str = r#"
CREATE TABLE supplier_events_migrated (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    supplier_id TEXT NOT NULL DEFAULT 'default',
    event_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    purchase_order_id TEXT,
    message TEXT,
    quantity INTEGER NOT NULL,
    received_at TEXT NOT NULL,
    status TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    purchased INTEGER NOT NULL DEFAULT 0,
    imported INTEGER NOT NULL DEFAULT 0,
    duplicate_count INTEGER NOT NULL DEFAULT 0,
    webhook_duplicate_count INTEGER NOT NULL DEFAULT 0,
    failed_count INTEGER NOT NULL DEFAULT 0,
    read_at TEXT,
    processing_started_at TEXT,
    supplier_batch_id TEXT,
    total_debit INTEGER,
    unit_price REAL,
    supplier_order_id TEXT,
    replayed INTEGER NOT NULL DEFAULT 0,
    pool_usable INTEGER,
    pool_deficit INTEGER,
    pool_requested INTEGER,
    retry_after TEXT,
    purchase_count INTEGER
);
INSERT INTO supplier_events_migrated
    (id,supplier_id,event_id,event_type,purchase_order_id,message,quantity,received_at,status,
     attempts,last_error,purchased,imported,duplicate_count,webhook_duplicate_count,failed_count,
     read_at,processing_started_at,supplier_batch_id,
     total_debit,unit_price,supplier_order_id,replayed,
     pool_usable,pool_deficit,pool_requested,retry_after,purchase_count)
SELECT id,supplier_id,event_id,event_type,purchase_order_id,message,quantity,received_at,status,
       attempts,last_error,purchased,imported,duplicate_count,webhook_duplicate_count,failed_count,
       read_at,processing_started_at,supplier_batch_id,
       total_debit,unit_price,supplier_order_id,replayed,
       pool_usable,pool_deficit,pool_requested,retry_after,purchase_count
FROM supplier_events;
DROP TABLE supplier_events;
ALTER TABLE supplier_events_migrated RENAME TO supplier_events;
"#;

fn initialize_schema(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(SCHEMA)?;
    let columns: std::collections::HashSet<String> = tx
        .prepare("PRAGMA table_info(supplier_events)")?
        .query_map([], |row| row.get(1))?
        .collect::<rusqlite::Result<_>>()?;

    for (name, definition) in MIGRATION_COLUMNS {
        if !columns.contains(*name) {
            tx.execute(
                &format!("ALTER TABLE supplier_events ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    if has_column_level_unique(&tx)? {
        tx.execute_batch(REBUILD_TABLE)?;
    }
    tx.execute(&format!("DROP INDEX IF EXISTS {LEGACY_EVENT_ID_INDEX}"), [])?;
    tx.execute_batch(INDEXES)?;
    tx.commit()
}

/// 列级 `UNIQUE` 会让 SQLite 建一个 `sqlite_autoindex_*` 隐式索引；表的其它约束
/// （INTEGER PRIMARY KEY AUTOINCREMENT 是 rowid 别名）不会。以此判断是否需要重建。
fn has_column_level_unique(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
         WHERE type='index' AND tbl_name='supplier_events' AND name LIKE 'sqlite_autoindex_%')",
        [],
        |row| row.get(0),
    )
}

fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use rusqlite::Connection;

    fn event(id: &str) -> IncomingSupplierEvent {
        IncomingSupplierEvent {
            supplier_id: LEGACY_SUPPLIER_ID.to_string(),
            event_id: id.to_string(),
            event_type: "purchase.requested".to_string(),
            purchase_order_id: Some("po-1".to_string()),
            supplier_batch_id: None,
            message: Some("hello".to_string()),
            quantity: 2,
        }
    }

    #[test]
    fn open_creates_schema_and_parent_directory() {
        let root = std::env::temp_dir().join(format!("kiro-supplier-{}", std::process::id()));
        let path = root.join("nested/events.db");
        let _ = std::fs::remove_dir_all(&root);
        let store = SupplierEventStore::open(&path).unwrap();
        assert_eq!(store.unread_count(None).unwrap(), 0);
        let conn = Connection::open(path).unwrap();
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(supplier_events)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(columns.contains(&"processing_started_at".to_string()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn open_migrates_minimal_legacy_supplier_events_table() {
        let path = std::env::temp_dir().join(format!(
            "kiro-supplier-legacy-{}-{}.db",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE supplier_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    event_id TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    received_at TEXT NOT NULL,
                    status TEXT NOT NULL
                );
                INSERT INTO supplier_events (event_id, event_type, received_at, status)
                VALUES ('legacy-1', 'purchase.requested', '2026-01-01T00:00:00Z', 'received');",
            )
            .unwrap();
        }

        let store = SupplierEventStore::open(&path).unwrap();
        let page = store.list(10, None, None).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].event_id, "legacy-1");
        assert_eq!(page.items[0].quantity, 0);
        assert_eq!(page.items[0].attempts, 0);
        assert_eq!(page.items[0].purchased_count, 0);
        assert_eq!(page.items[0].imported_count, 0);
        assert_eq!(store.claim_next().unwrap().unwrap().event_id, "legacy-1");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn open_deduplicates_legacy_event_ids_and_enforces_uniqueness() {
        let path = std::env::temp_dir().join(format!(
            "kiro-supplier-duplicate-{}-{}.db",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE supplier_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    event_id TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    received_at TEXT NOT NULL,
                    status TEXT NOT NULL
                );
                INSERT INTO supplier_events (event_id, event_type, received_at, status)
                VALUES
                    ('duplicate-1', 'purchase.requested', '2026-01-01T00:00:00Z', 'received'),
                    ('duplicate-1', 'purchase.requested', '2026-01-02T00:00:00Z', 'received');",
            )
            .unwrap();
        }

        assert!(SupplierEventStore::open(&path).is_err());
        let conn = Connection::open(&path).unwrap();
        let rows: Vec<(i64, String)> = conn
            .prepare("SELECT id,event_id FROM supplier_events ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (1, "duplicate-1".to_string()),
                (2, "duplicate-1".to_string())
            ]
        );
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(supplier_events)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!columns.contains(&"purchase_order_id".to_string()));
        assert!(!columns.contains(&"processing_started_at".to_string()));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn messages_are_truncated_without_splitting_unicode() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        let message = format!("{}{}", "中".repeat(MAX_MESSAGE_CHARS), "😀");
        let inserted = store
            .insert_event(IncomingSupplierEvent {
                message: Some(message.clone()),
                ..event("message-insert")
            })
            .unwrap();
        let inserted = match inserted {
            InsertOutcome::Inserted(value) => value,
            InsertOutcome::Duplicate(_) => panic!("event must be inserted"),
        };
        assert_eq!(
            inserted.message.as_ref().unwrap().chars().count(),
            MAX_MESSAGE_CHARS
        );
        assert!(!inserted.message.as_ref().unwrap().ends_with('\u{fffd}'));

        let claimed = store.claim_next().unwrap().unwrap();
        let complete_message = format!("{}😀", "界".repeat(MAX_MESSAGE_CHARS));
        store
            .complete(
                claimed.id,
                ProcessSummary {
                    message: Some(complete_message),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            store.list(1, None, None).unwrap().items[0]
                .message
                .as_ref()
                .unwrap()
                .chars()
                .count(),
            MAX_MESSAGE_CHARS
        );

        store
            .insert_event(IncomingSupplierEvent {
                event_id: "message-skip".to_string(),
                ..event("message-skip")
            })
            .unwrap();
        let skipped = store.claim_next().unwrap().unwrap();
        let skip_message = format!("{}🚀", "好".repeat(MAX_MESSAGE_CHARS));
        store.skip(skipped.id, Some(&skip_message)).unwrap();
        assert_eq!(
            store.list(1, None, None).unwrap().items[0]
                .message
                .as_ref()
                .unwrap()
                .chars()
                .count(),
            MAX_MESSAGE_CHARS
        );
    }

    /// 迁移探针：对着**真实旧库的副本**跑一遍 `open()`，确认不丢行、新列可用。
    ///
    /// 默认 `#[ignore]`，合成 fixture 覆盖不到「线上那份库到底什么形状」时手动跑：
    /// ```text
    /// cp key_supplier.db /tmp/probe.db
    /// PROBE_DB=/tmp/probe.db cargo test -- --ignored probe_migrates
    /// ```
    /// 它会往库里写一行探针数据，所以**只能对副本跑**。
    #[test]
    #[ignore = "manual probe against a real DB copy"]
    fn probe_migrates_a_real_legacy_database() {
        let path = std::env::var("PROBE_DB").expect("set PROBE_DB to a copy of the real db");
        let before: i64 = Connection::open(&path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM supplier_events", [], |row| row.get(0))
            .unwrap();

        let store = SupplierEventStore::open(&path).unwrap();
        let items = store.list(200, None, None).unwrap().items;

        assert_eq!(items.len() as i64, before, "迁移不能丢行");
        for item in &items {
            assert_eq!(item.supplier_id, LEGACY_SUPPLIER_ID);
            assert!(item.supplier_batch_id.is_none());
        }
        store
            .insert_event(IncomingSupplierEvent {
                supplier_id: "io".to_string(),
                event_id: "probe-batched".to_string(),
                event_type: "new_keys_available".to_string(),
                purchase_order_id: Some("0123456789abcdef0123456789abcdef".to_string()),
                supplier_batch_id: Some("batch-probe".to_string()),
                message: None,
                quantity: 1,
            })
            .unwrap();
        println!("rows before={before} after={} (+1 probe)", items.len());
    }

    #[test]
    fn open_rebuilds_the_table_so_two_suppliers_can_share_an_event_id() {
        let path = std::env::temp_dir().join(format!(
            "kiro-supplier-rebuild-{}-{}.db",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let _ = std::fs::remove_file(&path);
        {
            // 多供货商改造前的真实表形状：event_id 是列级 UNIQUE。
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE supplier_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    event_id TEXT UNIQUE NOT NULL,
                    event_type TEXT NOT NULL,
                    purchase_order_id TEXT,
                    message TEXT,
                    quantity INTEGER NOT NULL,
                    received_at TEXT NOT NULL,
                    status TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    purchased INTEGER NOT NULL DEFAULT 0,
                    imported INTEGER NOT NULL DEFAULT 0,
                    duplicate_count INTEGER NOT NULL DEFAULT 0,
                    webhook_duplicate_count INTEGER NOT NULL DEFAULT 0,
                    failed_count INTEGER NOT NULL DEFAULT 0,
                    read_at TEXT,
                    processing_started_at TEXT
                );
                CREATE UNIQUE INDEX idx_supplier_events_event_id_unique ON supplier_events(event_id);
                INSERT INTO supplier_events
                    (event_id,event_type,quantity,received_at,status,imported)
                VALUES ('shared-1','new_keys_available',3,'2026-01-01T00:00:00Z','succeeded',3);",
            )
            .unwrap();
        }

        let store = SupplierEventStore::open(&path).unwrap();

        // 历史行保留并回填 default，计数不丢。
        let items = store.list(10, None, None).unwrap().items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].supplier_id, LEGACY_SUPPLIER_ID);
        assert_eq!(items[0].imported_count, 3);
        assert_eq!(items[0].status, SupplierEventStatus::Succeeded);

        // 另一家供货商用同一个 event_id 必须能落库（旧的列级 UNIQUE 已被拆掉）。
        let other = store
            .insert_event(IncomingSupplierEvent {
                supplier_id: "kiroapp".to_string(),
                event_id: "shared-1".to_string(),
                event_type: "new_keys_available".to_string(),
                purchase_order_id: None,
                supplier_batch_id: None,
                message: None,
                quantity: 1,
            })
            .unwrap();
        assert!(matches!(other, InsertOutcome::Inserted(_)));
        assert_eq!(store.list(10, None, None).unwrap().items.len(), 2);
        // 同一家重复推还是判重。
        assert!(matches!(
            store
                .insert_event(IncomingSupplierEvent {
                    supplier_id: "kiroapp".to_string(),
                    event_id: "shared-1".to_string(),
                    event_type: "new_keys_available".to_string(),
                    purchase_order_id: None,
                    supplier_batch_id: None,
                    message: None,
                    quantity: 1,
                })
                .unwrap(),
            InsertOutcome::Duplicate(_)
        ));

        // 遗留的单列唯一索引必须消失，否则跨供货商同 id 又会被误判重复。
        let conn = Connection::open(&path).unwrap();
        let legacy_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                params![LEGACY_EVENT_ID_INDEX],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_index, 0);
        drop(conn);

        // 重复 open 幂等（不会反复重建）。
        drop(store);
        let reopened = SupplierEventStore::open(&path).unwrap();
        assert_eq!(reopened.list(10, None, None).unwrap().items.len(), 2);

        // 重建后 supplier_batch_id 必须还在并可写：重建走的是自己那份列清单，
        // 漏掉新列的话 ALTER 加上的列会在重建时被丢掉，插入才报错就太晚了。
        reopened
            .insert_event(IncomingSupplierEvent {
                supplier_id: "io".to_string(),
                event_id: "batched-1".to_string(),
                event_type: "new_keys_available".to_string(),
                purchase_order_id: Some("0123456789abcdef0123456789abcdef".to_string()),
                supplier_batch_id: Some("batch-io".to_string()),
                message: None,
                quantity: 2,
            })
            .unwrap();
        let stored = reopened.list(10, None, Some("io")).unwrap().items;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].supplier_batch_id.as_deref(), Some("batch-io"));
        // 历史行的新列是 NULL，不是空串——别让它看起来像「有个空批次」。
        let legacy = reopened
            .list(10, None, Some(LEGACY_SUPPLIER_ID))
            .unwrap()
            .items;
        assert!(legacy[0].supplier_batch_id.is_none());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn insert_deduplicates_event_id() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        assert!(matches!(
            store.insert_event(event("a")).unwrap(),
            InsertOutcome::Inserted(_)
        ));
        assert!(matches!(
            store.insert_event(event("a")).unwrap(),
            InsertOutcome::Duplicate(_)
        ));
        let items = store.list(10, None, None).unwrap().items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].webhook_duplicate_count, 1);
    }

    #[test]
    fn claim_is_atomic_and_oldest_first() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        store.insert_event(event("a")).unwrap();
        store.insert_event(event("b")).unwrap();
        let first = store.claim_next().unwrap().unwrap();
        let second = store.claim_next().unwrap().unwrap();
        assert_eq!(first.event_id, "a");
        assert_eq!(second.event_id, "b");
        assert_eq!(first.status, SupplierEventStatus::Processing);
        assert_eq!(first.attempts, 1);
        assert!(store.claim_next().unwrap().is_none());
    }

    #[test]
    fn complete_persists_all_processing_counts_atomically() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        store.insert_event(event("counts")).unwrap();
        let claimed = store.claim_next().unwrap().unwrap();

        store
            .complete(
                claimed.id,
                ProcessSummary {
                    purchased_count: 3,
                    imported_count: 1,
                    duplicate_count: 1,
                    failed_count: 1,
                    // 金额与对账字段必须和计数一起原子落库：只落一半的话，
                    // 预算累计和对方订单历史就永远对不上。
                    total_debit: Some(190),
                    unit_price: Some(38.0),
                    supplier_order_id: Some("0d9f".to_owned()),
                    replayed: true,
                    // 号池水位快照与计数同批落库，缺一个就解释不了「为什么买了这么多」。
                    pool_usable: Some(1),
                    pool_deficit: Some(4),
                    pool_requested: Some(3),
                    message: None,
                },
            )
            .unwrap();

        let stored = store.list(1, None, None).unwrap().items.remove(0);
        assert_eq!(stored.purchased_count, 3);
        assert_eq!(stored.imported_count, 1);
        assert_eq!(stored.duplicate_count, 1);
        assert_eq!(stored.failed_count, 1);
        assert_eq!(stored.total_debit, Some(190));
        assert_eq!(stored.unit_price, Some(38.0));
        assert_eq!(stored.supplier_order_id.as_deref(), Some("0d9f"));
        assert!(stored.replayed);
        assert_eq!(stored.pool_usable, Some(1));
        assert_eq!(stored.pool_deficit, Some(4));
        assert_eq!(stored.pool_requested, Some(3));
    }

    #[test]
    fn skip_and_fail_persist_the_pool_snapshot() {
        // 「为什么没买」正是水位三个数要回答的问题。只在成功路径记录等于没记录，
        // 所以 skipped 与 failed 两条路径必须同样落库。
        let store = SupplierEventStore::open_in_memory().unwrap();

        store.insert_event(event("skipped-with-snapshot")).unwrap();
        let claimed = store.claim_next().unwrap().unwrap();
        store
            .skip_with_summary(
                claimed.id,
                Some("号池已达目标存量"),
                ProcessSummary {
                    pool_usable: Some(3),
                    pool_deficit: Some(0),
                    pool_requested: Some(0),
                    // 跳过时计数字段应被清零，即使调用方误传了非零值。
                    purchased_count: 7,
                    ..Default::default()
                },
            )
            .unwrap();

        let skipped = store.list(1, None, None).unwrap().items.remove(0);
        assert_eq!(skipped.status, SupplierEventStatus::Skipped);
        assert_eq!(skipped.message.as_deref(), Some("号池已达目标存量"));
        assert_eq!(skipped.pool_usable, Some(3));
        assert_eq!(skipped.pool_deficit, Some(0));
        assert_eq!(skipped.pool_requested, Some(0));
        assert_eq!(skipped.purchased_count, 0, "跳过不该记成买到了");

        store.insert_event(event("failed-with-snapshot")).unwrap();
        let claimed = store.claim_next().unwrap().unwrap();
        store
            .fail_with_summary(
                claimed.id,
                ProcessSummary {
                    purchased_count: 2,
                    failed_count: 2,
                    // 采购成功但导入失败：钱花了，水位与金额都必须活下来。
                    total_debit: Some(60),
                    pool_usable: Some(0),
                    pool_deficit: Some(3),
                    pool_requested: Some(2),
                    ..Default::default()
                },
                "导入失败",
            )
            .unwrap();

        let failed = store.list(1, None, None).unwrap().items.remove(0);
        assert_eq!(failed.status, SupplierEventStatus::Failed);
        assert_eq!(failed.total_debit, Some(60));
        assert_eq!(failed.pool_usable, Some(0));
        assert_eq!(failed.pool_deficit, Some(3));
        assert_eq!(failed.pool_requested, Some(2));
    }

    #[test]
    fn claim_by_event_id_only_claims_received_event() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        store.insert_event(event("a")).unwrap();

        let claimed = store
            .claim_by_event_id(LEGACY_SUPPLIER_ID, "a")
            .unwrap()
            .unwrap();
        assert_eq!(claimed.status, SupplierEventStatus::Processing);
        assert_eq!(claimed.attempts, 1);
        assert!(
            store
                .claim_by_event_id(LEGACY_SUPPLIER_ID, "a")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn state_transitions_retry_and_stale_recovery_are_restricted() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        store.insert_event(event("a")).unwrap();
        let claimed = store.claim_next().unwrap().unwrap();
        store
            .complete(
                claimed.id,
                ProcessSummary {
                    purchased_count: 1,
                    imported_count: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(store.retry(claimed.id).is_err());
        store.insert_event(event("b")).unwrap();
        let failed = store.claim_next().unwrap().unwrap();
        store.fail(failed.id, &"错误".repeat(400)).unwrap();
        store.retry(failed.id).unwrap();
        assert_eq!(store.claim_next().unwrap().unwrap().event_id, "b");
        store.insert_event(event("c")).unwrap();
        let stale = store.claim_next().unwrap().unwrap();
        let recovered = store
            .recover_stale_processing(Utc::now() + Duration::seconds(1))
            .unwrap();
        assert_eq!(recovered, 2);
        assert_eq!(store.claim_next().unwrap().unwrap().event_id, "b");
        assert_eq!(store.claim_next().unwrap().unwrap().event_id, "c");
        assert_eq!(stale.attempts, 1);
    }

    #[test]
    fn deferred_event_waits_for_retry_after_without_blocking_the_queue() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        store.insert_event(event("a")).unwrap();
        store.insert_event(event("b")).unwrap();

        let first = store.claim_next().unwrap().unwrap();
        assert_eq!(first.event_id, "a");
        store
            .defer(
                first.id,
                Utc::now() + Duration::seconds(60),
                Some(3),
                "supplier HTTP 500",
            )
            .unwrap();

        // 回到 received 而不是 failed，数量被钉住，错误仍然可见。
        let deferred = store.list(999, None, None).unwrap();
        let deferred = deferred
            .items
            .iter()
            .find(|item| item.event_id == "a")
            .unwrap();
        assert_eq!(deferred.status, SupplierEventStatus::Received);
        assert_eq!(deferred.purchase_count, Some(3));
        assert!(deferred.retry_after.is_some());
        assert_eq!(deferred.last_error.as_deref(), Some("supplier HTTP 500"));

        // 未到点不该被领走，但也不该挡住后面新到货的事件——抢货是拼延迟的。
        assert_eq!(store.claim_next().unwrap().unwrap().event_id, "b");

        // 人工重试无视退避：人明确要求现在就试。钉住的数量必须留着，
        // 换了数量幂等协议会返 409（原单已成交、钱扣了、key 没到手）。
        store.retry(first.id).unwrap();
        let claimed = store.claim_next().unwrap().unwrap();
        assert_eq!(claimed.event_id, "a");
        assert_eq!(claimed.purchase_count, Some(3));
        assert_eq!(claimed.attempts, 2);
        // 领取时清空退避标记，否则下一轮 stale 回收后会把旧时间再算一次。
        assert!(claimed.retry_after.is_none());
        assert!(claimed.last_error.is_none());
    }

    #[test]
    fn defer_only_applies_to_in_flight_events_and_expired_waits_are_claimable() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        store.insert_event(event("a")).unwrap();

        // 没在处理中的事件不能被压回队列。
        let id = store.claim_next().unwrap().unwrap().id;
        store.complete(id, ProcessSummary::default()).unwrap();
        assert!(
            store
                .defer(id, Utc::now() + Duration::seconds(1), None, "boom")
                .is_err()
        );

        store.insert_event(event("b")).unwrap();
        let claimed = store.claim_next().unwrap().unwrap();
        store
            .defer(claimed.id, Utc::now() - Duration::seconds(1), None, "boom")
            .unwrap();

        // 到点即可再领，且不带数量时不钉任何东西。
        let again = store.claim_next().unwrap().unwrap();
        assert_eq!(again.event_id, "b");
        assert_eq!(again.attempts, 2);
        assert_eq!(again.purchase_count, None);
    }

    #[test]
    fn list_paginates_and_tracks_unread() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        for id in ["a", "b", "c"] {
            store.insert_event(event(id)).unwrap();
        }
        let page = store.list(999, None, None).unwrap();
        assert_eq!(page.items.len(), 3);
        assert_eq!(page.items[0].event_id, "c");
        assert_eq!(
            store
                .list(2, Some(page.items[0].id), None)
                .unwrap()
                .items
                .len(),
            2
        );
        assert_eq!(store.unread_count(None).unwrap(), 3);
        store.mark_read(&[page.items[0].id]).unwrap();
        assert_eq!(store.unread_count(None).unwrap(), 2);
        store.mark_all_read(None).unwrap();
        assert_eq!(store.unread_count(None).unwrap(), 0);
    }

    #[test]
    fn unknown_status_is_rejected_strictly() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        store.insert_event(event("a")).unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE supplier_events SET status='future'", [])
            .unwrap();
        assert!(store.list(1, None, None).is_err());
    }
}
