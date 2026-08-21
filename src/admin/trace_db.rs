//! 请求链路追踪（Trace）持久化
//!
//! 记录每次 `/v1/messages` 请求的完整重试链路，用于排查"中断"类问题：
//! - 一个外部请求 = 1 条 [`TraceRecord`] 汇总 + N 条 [`TraceAttempt`] 子记录
//! - 每跳记录命中凭据、HTTP 状态码、失败分类、上游错误体片段、耗时
//!
//! 存储：SQLite（`traces.db`），WAL 模式。前端查询直接走 SQL（索引 + WHERE + LIMIT），
//! 不维护内存缓冲。后台任务定期清理超过保留天数的记录（保留天数与启用开关运行时可改）。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, types::Type};
use serde::{Deserialize, Serialize};

use crate::anthropic::compaction_diagnostics::CompactionTraceData;

use super::client_keys::ClientResponseMode;

/// trace 记录默认保留天数
const DEFAULT_RETENTION_DAYS: u64 = 7;
/// 上游错误体片段最大长度（字节）
const ERROR_SNIPPET_MAX: usize = 2048;
/// 查询默认返回条数
pub const DEFAULT_QUERY_LIMIT: usize = 200;

/// 单次上游尝试的结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceAttempt {
    /// 第几次尝试（0-based）
    pub attempt: u32,
    /// 命中的上游凭据 id；0 表示未取到凭据
    pub credential_id: u64,
    /// 端点名（ide / cli）
    pub endpoint: String,
    /// 上游 HTTP 状态码；None 表示网络层失败（请求未发出/无响应）
    pub http_status: Option<u16>,
    /// 失败分类，见 [`Outcome`]
    pub outcome: String,
    /// 上游错误体片段（截断到 [`ERROR_SNIPPET_MAX`]）
    pub error_snippet: Option<String>,
    /// 本跳耗时（毫秒）
    pub duration_ms: u64,
}

/// 调用方使用的入口 Key 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TraceKeySource {
    /// 管理员API密钥。
    MasterApiKey,
    /// Admin UI 中创建并分发的客户端 Key。
    ClientKey,
}

impl TraceKeySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MasterApiKey => "masterApiKey",
            Self::ClientKey => "clientKey",
        }
    }

    fn from_db(value: &str, column: usize) -> rusqlite::Result<Self> {
        match value {
            "masterApiKey" => Ok(Self::MasterApiKey),
            "clientKey" => Ok(Self::ClientKey),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                column,
                Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("未知 trace key_source: {other}"),
                )),
            )),
        }
    }
}

/// 一个外部请求的完整链路
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceRecord {
    /// 链路 id（uuid v4），前端 key
    pub trace_id: String,
    /// 请求开始时间（RFC3339）
    pub ts: String,
    /// 客户端 Key id；0 表示 master apiKey
    pub key_id: u64,
    /// 入口 Key 类型，区分管理员API密钥与创建的客户端 Key。
    pub key_source: TraceKeySource,
    /// 鉴权时捕获的回复模式，不随 Key 后续编辑变化。
    #[serde(default)]
    pub response_mode: ClientResponseMode,
    /// 模型名
    pub model: String,
    /// 是否流式
    pub is_stream: bool,
    /// 最终状态：success / error / interrupted
    pub final_status: String,
    /// 最终命中（成功）或最后尝试的凭据 id
    pub final_credential_id: u64,
    /// 失败分类（顶层，便于筛选）
    pub error_type: Option<String>,
    /// 给用户的简明错误信息
    pub error_message: Option<String>,
    /// 总尝试次数
    pub total_attempts: u32,
    /// 端到端耗时（毫秒）
    pub duration_ms: u64,
    /// 流式中断时已发送的字节数（区分完整失败 vs 半截中断）
    pub interrupted_after_bytes: Option<u64>,
    /// 输入 token（Anthropic 口径）
    #[serde(default)]
    pub input_tokens: u64,
    /// 输出 token
    #[serde(default)]
    pub output_tokens: u64,
    /// 缓存创建 token
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// 缓存读取 token
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// 费用（上游 meteringEvent 累计的 credits）
    #[serde(default)]
    pub credits: f64,
    /// 首 Token 延迟（毫秒，仅流式有值；非流式为 None）
    #[serde(default)]
    pub first_token_ms: Option<u64>,
    /// Kiro 上游首个原始 body chunk 延迟（毫秒，仅流式有值）。
    #[serde(default)]
    pub upstream_first_byte_ms: Option<u64>,
    /// 本次请求实际下发的思考档位（low/medium/high/xhigh/max）；未启用/不支持时为 None。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// 是否声明了 1M 扩展上下文（客户端带 `anthropic-beta: context-1m-...` 头）。
    #[serde(default)]
    pub context_1m: bool,
    /// 客户端是否请求了推理（thinking 启用 或 显式 output_config.effort）；
    /// 与 reasoning_effort 独立：请求了推理但未解析出具体档位时仍为 true。
    #[serde(default)]
    pub thinking: bool,
    /// 是否对精确的空 user 请求应用了最小兼容文本。
    #[serde(default)]
    pub empty_user_compat_applied: bool,
    /// 失败请求对应的持久化错误快照 ID。
    #[serde(default)]
    pub snapshot_id: Option<String>,
    /// 自动压缩诊断安全快照；不含正文、工具参数、请求头或凭证。
    #[serde(default)]
    pub compaction: Option<CompactionTraceData>,
    /// 每跳明细
    pub attempts: Vec<TraceAttempt>,
}

/// 利润报表关联所需的最小 trace 视图；不读取 attempts，避免报表查询放大 SQLite I/O。
#[derive(Debug, Clone, PartialEq)]
pub struct ProfitTraceRecord {
    pub trace_id: String,
    pub key_id: u64,
    pub model: String,
    pub credits: f64,
    pub final_status: String,
}

/// 失败分类（attempt.outcome / record.error_type 取值）
pub mod outcome {
    pub const SUCCESS: &str = "success";
    pub const QUOTA_EXHAUSTED: &str = "quota_exhausted";
    pub const ACCOUNT_THROTTLED: &str = "account_throttled";
    pub const AUTH_FAILED: &str = "auth_failed";
    pub const TRANSIENT: &str = "transient";
    pub const NETWORK_ERROR: &str = "network_error";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const UNKNOWN: &str = "unknown";
    /// 仅用作 record.error_type：流式响应已开始但上游中途断开
    pub const STREAM_INTERRUPTED: &str = "stream_interrupted";
    /// 上游流读取出错（已下发部分内容后断开）
    pub const STREAM_READ_ERROR: &str = "stream_read_error";
    /// 上游流空闲超时，服务端主动收尾
    pub const STREAM_IDLE_TIMEOUT: &str = "stream_idle_timeout";
    /// 客户端主动断开连接
    pub const CLIENT_DISCONNECTED: &str = "client_disconnected";
    /// 上游 200 但一个助手内容都没给（重试后仍然如此）
    pub const UPSTREAM_EMPTY_RESPONSE: &str = "upstream_empty_response";
    /// 号池耗尽：一个可用凭据都没有，请求根本没发到上游。
    ///
    /// 单列一类是为了让它命中快照白名单。此前它一路落成 `unknown`，于是每个请求
    /// 都被完整存档——线上 27.8 万条、2.7 GB，占了整个快照库的 83%，而请求体对
    /// 「没号可用」这个结论没有任何诊断价值。
    pub const NO_AVAILABLE_CREDENTIALS: &str = "no_available_credentials";
    /// 配置的账号里没有一个提供客户端请求的模型。同样是请求未出站的确定性终态。
    pub const MODEL_NOT_AVAILABLE: &str = "model_not_available";
}

/// 把上游错误体截断到安全长度（按字符边界，避免切碎 UTF-8）
pub fn truncate_snippet(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= ERROR_SNIPPET_MAX {
        return Some(trimmed.to_string());
    }
    let mut end = ERROR_SNIPPET_MAX;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…(truncated)", &trimmed[..end]))
}

/// 链路上报接收端：provider 在重试循环里每跳调用 [`Self::on_attempt`]
pub enum TraceDiagnosticEvent<'a> {
    UpstreamRequest {
        attempt: u32,
        credential_id: u64,
        endpoint: &'a str,
        body: &'a str,
    },
    UpstreamResponse {
        attempt: u32,
        credential_id: u64,
        endpoint: &'a str,
        status: u16,
        body: &'a str,
    },
    NetworkError {
        attempt: u32,
        credential_id: u64,
        endpoint: &'a str,
        message: &'a str,
    },
}

pub trait TraceSink: Send + Sync {
    fn on_attempt(&self, attempt: TraceAttempt);
    fn on_diagnostic(&self, _event: TraceDiagnosticEvent<'_>) {}
}

/// 查询过滤条件
#[derive(Debug, Default, Clone)]
pub struct TraceQuery {
    /// final_status 精确匹配（success/error/interrupted）
    pub status: Option<String>,
    /// error_type 精确匹配
    pub error_type: Option<String>,
    /// 最终凭据 id
    pub credential_id: Option<u64>,
    /// 客户端 Key id（0 = master apiKey）
    pub key_id: Option<u64>,
    /// 该凭据在某一跳失败过（attempt 级，跨 trace 最终状态）。
    /// 用于"凭据失败详情"：即便整条 trace 最终成功，只要该凭据某跳失败也会命中。
    pub failed_attempt_credential_id: Option<u64>,
    /// 模型名
    pub model: Option<String>,
    /// 仅返回非 success
    pub only_failed: bool,
    /// 按账号分组筛选：只返回最终凭据属于这些 id 的 trace。
    /// 由 handler 层在查询前根据 group 参数转换为凭据 id 白名单填入。
    pub credential_ids: Option<Vec<u64>>,
    /// 自动压缩诊断原因精确匹配。
    pub compaction_diagnosis: Option<String>,
    /// SHA-256 会话 hash 精确匹配。
    pub session_hash: Option<String>,
    /// 仅返回达到上下文/字节压力或诊断非 normal 的记录。
    pub high_pressure_only: bool,
    /// 返回条数上限
    pub limit: usize,
    /// 偏移量（分页用）
    pub offset: usize,
}

/// 落库队列容量。
///
/// 队列满时**丢弃新记录而不是阻塞请求**：trace 是可观测性数据，任何情况下都不该
/// 让它拖慢真实流量。丢弃会计数并周期性告警，便于发现写入侧跟不上。
const TRACE_QUEUE_CAPACITY: usize = 4096;

/// 单个事务最多合并多少条记录。
///
/// 一条记录一个事务时，每次都要走一遍 WAL 追加 + 锁获取；批量合并后同样的锁只付
/// 一次代价。上限存在是为了不让单次事务持锁过久，阻塞 Admin 页面的查询。
const TRACE_BATCH_SIZE: usize = 256;

/// SQLite 持久化存储
pub struct TraceStore {
    conn: Mutex<Connection>,
    /// 是否启用 trace 写入（运行时可改）。false 时 insert 直接短路。
    enabled: AtomicBool,
    /// 记录保留天数（运行时可改），cleanup 时读取。
    retention_days: AtomicU64,
    /// 异步写入队列的发送端。
    ///
    /// `None` 表示未启动后台写入器（单测、以及 `spawn_writer` 之前的窗口），
    /// 此时 [`Self::insert`] 退化为同步写，保证测试里「写完立刻能查到」的语义不变。
    writer: Mutex<Option<tokio::sync::mpsc::Sender<TraceRecord>>>,
    /// 因队列满而丢弃的记录数（只增）。
    dropped: AtomicU64,
}

impl TraceStore {
    /// 打开（或创建）数据库并建表。空路径归一为当前目录下的 traces.db。
    pub fn open(path: PathBuf, enabled: bool, retention_days: u32) -> rusqlite::Result<Self> {
        let path = if path.as_os_str().is_empty() {
            PathBuf::from("traces.db")
        } else {
            path
        };
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!("创建 traces.db 目录失败 {}: {}", parent.display(), e);
                }
            }
        }
        let conn = Connection::open(&path)?;
        // WAL：并发读不阻塞写；synchronous=NORMAL：写吞吐与崩溃安全的平衡
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            enabled: AtomicBool::new(enabled),
            retention_days: AtomicU64::new(retention_days.max(1) as u64),
            writer: Mutex::new(None),
            dropped: AtomicU64::new(0),
        })
    }

    /// 退出前把 WAL 截断。
    ///
    /// 自动检查点是 PASSIVE 语义：把页搬回主库后只是**从头复用** WAL，并不缩文件。
    /// 所以某次高峰把 WAL 涨到几百 MB 之后，它就永远是那么大；而进程一直是硬退出，
    /// 没有任何时机去截断它。代价是下次启动 `Connection::open` 要为这个文件做恢复，
    /// 白付启动时间——而启动时间是挡在客户流量前面的。
    ///
    /// 拿不到写锁就放弃：卡住退出等于把新版本无限期推迟，比留着一个大 WAL 严重得多。
    pub fn checkpoint_truncate(&self) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
    }

    /// 内存数据库（traces.db 打开失败时的兜底；进程退出即丢，但保证 Admin 查询不崩）
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            enabled: AtomicBool::new(true),
            retention_days: AtomicU64::new(DEFAULT_RETENTION_DAYS),
            writer: Mutex::new(None),
            dropped: AtomicU64::new(0),
        })
    }

    /// 旧库迁移：为 traces 表补齐新增列（幂等，缺哪列加哪列）。
    /// 老版本的 traces.db 只有基础列，新增的 token/credits/first_token_ms/key_source 需在此 ALTER。
    fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        let mut existing: std::collections::HashSet<String> = std::collections::HashSet::new();
        {
            let mut stmt = conn.prepare("PRAGMA table_info(traces)")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for name in rows {
                existing.insert(name?);
            }
        }
        // (列名, 定义) —— 与 SCHEMA 中新增列保持一致
        // 注意 key_source 不带 NOT NULL：老库已有行需先以 NULL 添加再回填（SQLite ALTER ADD COLUMN
        // NOT NULL 不带常量 DEFAULT 时无法对已有行赋值）。新插入永远写入合法值。
        let columns: [(&str, &str); 22] = [
            ("input_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("output_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("cache_creation_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("credits", "REAL NOT NULL DEFAULT 0"),
            ("first_token_ms", "INTEGER"),
            ("upstream_first_byte_ms", "INTEGER"),
            ("key_source", "TEXT"),
            ("response_mode", "TEXT NOT NULL DEFAULT 'detection'"),
            ("reasoning_effort", "TEXT"),
            ("context_1m", "INTEGER NOT NULL DEFAULT 0"),
            ("thinking", "INTEGER NOT NULL DEFAULT 0"),
            ("empty_user_compat_applied", "INTEGER NOT NULL DEFAULT 0"),
            ("snapshot_id", "TEXT"),
            ("session_hash", "TEXT"),
            ("client_version", "TEXT"),
            ("compaction_diagnosis", "TEXT"),
            ("request_body_bytes", "INTEGER"),
            ("upstream_context_tokens", "INTEGER"),
            ("upstream_context_percentage", "REAL"),
            ("client_reported_tokens", "INTEGER"),
            ("compaction_diagnostics_json", "TEXT"),
        ];
        let key_source_added = !existing.contains("key_source");
        for (name, def) in columns {
            if !existing.contains(name) {
                conn.execute_batch(&format!("ALTER TABLE traces ADD COLUMN {} {};", name, def))?;
            }
        }
        // 老库 key_source 列首次添加后，按 key_id 语义回填：master apiKey (key_id=0) 之外都视为客户端 Key。
        if key_source_added {
            conn.execute_batch(
                "UPDATE traces SET key_source = CASE WHEN key_id = 0 \
                 THEN 'masterApiKey' ELSE 'clientKey' END WHERE key_source IS NULL;",
            )?;
        }
        // 必须在旧库补列之后创建，否则旧表尚无 snapshot_id 时索引会创建失败。
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_traces_snapshot ON traces(snapshot_id);
             CREATE INDEX IF NOT EXISTS idx_traces_session_ts
                 ON traces(session_hash, ts_epoch DESC);
             CREATE INDEX IF NOT EXISTS idx_traces_compaction_diagnosis
                 ON traces(compaction_diagnosis);",
        )?;
        Ok(())
    }

    /// 是否启用 trace 写入
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// 设置启用开关
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// 获取保留天数
    pub fn retention_days(&self) -> u64 {
        self.retention_days.load(Ordering::Relaxed)
    }

    /// 设置保留天数（>=1）
    pub fn set_retention_days(&self, days: u32) {
        self.retention_days
            .store(days.max(1) as u64, Ordering::Relaxed);
    }

    /// 写入一条完整链路（traces + attempts 在一个事务里）。失败仅 warn，不阻塞请求。
    /// trace 关闭时直接短路。
    /// 启动后台写入器，把落库从请求路径上摘下来。
    ///
    /// 改造前 [`Self::insert`] 直接在异步请求路径上做「加全局 Mutex + 同步 SQLite 事务」。
    /// 库涨到几百 MB 后单次写入耗时上升，而 Tokio worker 线程数有限：并发一高，
    /// 所有 worker 都卡在这把锁上，**整个运行时停转**——线上表现是上游一条 TCP 连接
    /// 都没有、入站连接堆积、吞吐从 219/分钟塌到个位数，重启才恢复。
    ///
    /// 现在请求侧只做一次 `try_send`（纳秒级，且队列满时直接丢弃而不是等待），
    /// 真正的写入在独立任务里批量完成，并且跑在 `spawn_blocking` 上，不占 worker。
    pub fn spawn_writer(self: &Arc<Self>) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TraceRecord>(TRACE_QUEUE_CAPACITY);
        *self.writer.lock() = Some(tx);

        let store = Arc::clone(self);
        tokio::spawn(async move {
            let mut batch = Vec::with_capacity(TRACE_BATCH_SIZE);
            let mut reported_drops = 0u64;
            loop {
                // recv_many 会尽量一次取走多条；返回 0 表示所有发送端已关闭。
                if rx.recv_many(&mut batch, TRACE_BATCH_SIZE).await == 0 {
                    break;
                }
                let records = std::mem::take(&mut batch);
                let writer = Arc::clone(&store);
                // 阻塞式 SQLite 写必须离开 worker 线程，否则等于把问题从
                // 请求路径挪到了后台任务，运行时照样会被堵住。
                if let Err(error) =
                    tokio::task::spawn_blocking(move || writer.insert_batch_blocking(&records))
                        .await
                {
                    tracing::warn!(%error, "trace 批量写入任务异常");
                }
                batch = Vec::with_capacity(TRACE_BATCH_SIZE);

                let dropped = store.dropped.load(Ordering::Relaxed);
                if dropped > reported_drops {
                    tracing::warn!(
                        dropped,
                        "trace 队列已满，丢弃了部分链路记录（写入侧跟不上，请考虑缩短保留期或关闭 trace）"
                    );
                    reported_drops = dropped;
                }
            }
        });
    }

    /// 因队列满被丢弃的 trace 条数。
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 记录一条链路。
    ///
    /// 后台写入器已启动时只入队即返回；未启动（单测、以及启动早期）时退化为同步写，
    /// 保证「写完立刻能查到」的既有语义。
    pub fn insert(&self, rec: TraceRecord) {
        if !self.is_enabled() {
            return;
        }
        // 取所有权而非借用：热路径因此不需要深拷贝 TraceRecord
        //（它含多个 String 和一个 Vec<TraceAttempt>，每请求一次堆分配不划算）。
        let returned = {
            let writer = self.writer.lock();
            match writer.as_ref() {
                Some(tx) => match tx.try_send(rec) {
                    Ok(()) => None,
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        // 关键：不等待。宁可丢一条可观测性数据，也不让请求为它排队。
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                    // 写入器已退出（进程收尾），回退同步写避免静默丢数据。
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(rec)) => Some(rec),
                },
                None => Some(rec),
            }
        };
        if let Some(rec) = returned {
            self.insert_blocking(&rec);
        }
    }

    /// 批量落库：整批共用一个事务，把 WAL 追加与锁获取的固定开销摊薄。
    fn insert_batch_blocking(&self, records: &[TraceRecord]) {
        if records.is_empty() {
            return;
        }
        let mut conn = self.conn.lock();
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("trace 批量事务开启失败: {}", e);
                return;
            }
        };
        let mut failed = 0usize;
        for rec in records {
            if let Err(e) = Self::write_record(&tx, rec) {
                failed += 1;
                tracing::warn!("trace 写入失败: {}", e);
            }
        }
        if let Err(e) = tx.commit() {
            tracing::warn!("trace 批量提交失败（{} 条）: {}", records.len(), e);
        } else if failed > 0 {
            tracing::warn!("trace 批量提交完成，其中 {} 条失败", failed);
        }
    }

    fn insert_blocking(&self, rec: &TraceRecord) {
        let mut conn = self.conn.lock();
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("trace 事务开启失败: {}", e);
                return;
            }
        };
        match Self::write_record(&tx, rec) {
            Ok(()) => {
                if let Err(e) = tx.commit() {
                    tracing::warn!("trace 提交失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("trace 写入失败: {}", e),
        }
    }

    /// 把一条记录写进给定事务。批量与单条路径共用，避免 SQL 两处维护。
    fn write_record(tx: &rusqlite::Transaction<'_>, rec: &TraceRecord) -> rusqlite::Result<()> {
        let ts_epoch = chrono::DateTime::parse_from_rfc3339(&rec.ts)
            .map(|d| d.timestamp())
            .unwrap_or_else(|_| Utc::now().timestamp());
        let compaction = rec.compaction.as_ref();
        let compaction_diagnosis = compaction
            .map(|snapshot| Self::infer_compaction_diagnosis(tx, &rec.trace_id, snapshot))
            .transpose()?;
        {
            tx.execute(
                "INSERT OR REPLACE INTO traces (trace_id, ts, ts_epoch, key_id, key_source, response_mode, model, \
                 is_stream, final_status, final_credential_id, error_type, error_message, \
                 total_attempts, duration_ms, interrupted_after_bytes, \
                 input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, \
                 credits, first_token_ms, upstream_first_byte_ms, reasoning_effort, context_1m, thinking, \
                 empty_user_compat_applied, snapshot_id, session_hash, client_version, \
                 compaction_diagnosis, request_body_bytes, upstream_context_tokens, \
                 upstream_context_percentage, client_reported_tokens, compaction_diagnostics_json) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,?33,?34,?35)",
                rusqlite::params![
                    rec.trace_id,
                    rec.ts,
                    ts_epoch,
                    rec.key_id as i64,
                    rec.key_source.as_str(),
                    rec.response_mode.as_str(),
                    rec.model,
                    rec.is_stream as i64,
                    rec.final_status,
                    rec.final_credential_id as i64,
                    rec.error_type,
                    rec.error_message,
                    rec.total_attempts as i64,
                    rec.duration_ms as i64,
                    rec.interrupted_after_bytes.map(|v| v as i64),
                    rec.input_tokens as i64,
                    rec.output_tokens as i64,
                    rec.cache_creation_tokens as i64,
                    rec.cache_read_tokens as i64,
                    rec.credits,
                    rec.first_token_ms.map(|v| v as i64),
                    rec.upstream_first_byte_ms.map(|v| v as i64),
                    rec.reasoning_effort,
                    rec.context_1m as i64,
                    rec.thinking as i64,
                    rec.empty_user_compat_applied as i64,
                    rec.snapshot_id,
                    compaction.and_then(|value| value.session_hash.as_deref()),
                    compaction.and_then(|value| value.client_version.as_deref()),
                    compaction_diagnosis,
                    compaction.map(|value| value.request_body_bytes as i64),
                    compaction
                        .and_then(|value| value.upstream_context_tokens)
                        .map(|value| value as i64),
                    compaction.and_then(|value| value.upstream_context_percentage),
                    compaction
                        .and_then(|value| value.client_reported_tokens)
                        .map(|value| value as i64),
                    compaction.map(|value| value.diagnostics_json.as_str()),
                ],
            )?;
            // 用「发射顺序下标」作为 attempt 主键分量，而非 provider 的重试轮次计数：
            // 一轮重试里 429 端点降级会先后发射「备用端点失败」+「主端点分类」两跳，
            // 它们的轮次计数相同，若直接以 a.attempt 作主键，INSERT OR REPLACE 会让后
            // 写入的主端点行覆盖先写入的备用端点行 —— 备用端点(runtime)失败因此在链路里
            // 不可见。改用 enumerate 下标后每一跳都得到唯一、连续、有序的主键，所有跳
            // （含 runtime 失败）都完整落库；正常单跳/轮次的 trace 编号不变。
            for (seq, a) in rec.attempts.iter().enumerate() {
                tx.execute(
                    "INSERT OR REPLACE INTO trace_attempts (trace_id, attempt, credential_id, \
                     endpoint, http_status, outcome, error_snippet, duration_ms) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    rusqlite::params![
                        rec.trace_id,
                        seq as i64,
                        a.credential_id as i64,
                        a.endpoint,
                        a.http_status.map(|v| v as i64),
                        a.outcome,
                        a.error_snippet,
                        a.duration_ms as i64,
                    ],
                )?;
            }
        }
        Ok(())
    }

    fn infer_compaction_diagnosis(
        tx: &rusqlite::Transaction<'_>,
        trace_id: &str,
        current: &CompactionTraceData,
    ) -> rusqlite::Result<String> {
        let Some(session_hash) = current.session_hash.as_deref() else {
            return Ok(current.diagnosis.clone());
        };
        let previous = tx
            .query_row(
                "SELECT request_body_bytes, upstream_context_percentage, client_reported_tokens \
                 FROM traces WHERE session_hash = ?1 AND trace_id != ?2 \
                 ORDER BY ts_epoch DESC, rowid DESC LIMIT 1",
                rusqlite::params![session_hash, trace_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?.map(|value| value as u64),
                        row.get::<_, Option<f64>>(1)?,
                        row.get::<_, Option<i64>>(2)?.map(|value| value as u64),
                    ))
                },
            )
            .optional()?;
        let Some((Some(previous_bytes), previous_percentage, previous_client_tokens)) = previous
        else {
            return Ok(current.diagnosis.clone());
        };
        if previous_bytes == 0 {
            return Ok(current.diagnosis.clone());
        }

        let previous_exposed_high_context = previous_percentage
            .is_some_and(|percentage| percentage >= 80.0)
            && previous_client_tokens.is_some();
        let stayed_large = current.request_body_bytes >= 2_500_000
            && current.request_body_bytes.saturating_mul(100) >= previous_bytes.saturating_mul(85);
        if previous_exposed_high_context && stayed_large {
            return Ok("suspected_client_compaction_not_triggered".to_string());
        }

        let shrank_at_least_twenty_percent =
            current.request_body_bytes.saturating_mul(100) <= previous_bytes.saturating_mul(80);
        if current.diagnosis == "payload_limit_preempted" && shrank_at_least_twenty_percent {
            return Ok("suspected_compaction_insufficient".to_string());
        }

        // 正向观测：上一轮已处于高压且本轮请求体明显缩小 → 客户端确实压缩了。
        //
        // 加这一条是为了验证「压缩信号阈值降到 85%」这个改动到底有没有生效。
        // 原先只有 `suspected_client_compaction_not_triggered` 这个**否定**判定，
        // 而否定判定的缺席是弱证据——请求可能只是没进高压区。有了正向计数，
        // 两者的比值才能直接回答「客户端认不认这个 stop_reason」。
        //
        // 命名用 observed 而非 confirmed：这是相关性推断（上一轮高压 + 本轮变小），
        // 不能证明因果，客户端也可能因为别的原因缩小了请求。
        if previous_exposed_high_context && shrank_at_least_twenty_percent {
            return Ok("client_compaction_observed".to_string());
        }
        Ok(current.diagnosis.clone())
    }

    /// 把已落库的 trace 与错误快照关联。相同关联可重复写入，冲突关联 fail-closed。
    pub fn link_snapshot(&self, trace_id: &str, snapshot_id: &str) -> bool {
        self.conn
            .lock()
            .execute(
                "UPDATE traces SET snapshot_id = ?1
                 WHERE trace_id = ?2 AND (snapshot_id IS NULL OR snapshot_id = ?1)",
                rusqlite::params![snapshot_id, trace_id],
            )
            .map(|changed| changed > 0)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, %trace_id, %snapshot_id, "回链错误快照失败");
                false
            })
    }

    /// 分页查询：返回 (当前页记录, 符合条件的总数)。仅 warn 失败，返回 (空, 0)。
    pub fn query_paged(&self, q: &TraceQuery) -> (Vec<TraceRecord>, usize) {
        let conn = self.conn.lock();
        match Self::query_inner(&conn, q) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("trace 查询失败: {}", e);
                (Vec::new(), 0)
            }
        }
    }

    /// 按精确 trace ID 和时间窗口批量读取利润关联字段。
    ///
    /// 每批最多 400 个 ID，为时间范围参数和不同 SQLite 参数上限保留余量。
    pub fn query_profit_traces(
        &self,
        trace_ids: &[String],
        start_epoch: i64,
        end_epoch: i64,
    ) -> rusqlite::Result<Vec<ProfitTraceRecord>> {
        if trace_ids.is_empty() || start_epoch >= end_epoch {
            return Ok(Vec::new());
        }

        let unique_ids: Vec<String> = trace_ids
            .iter()
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let conn = self.conn.lock();
        let mut records = Vec::new();
        for chunk in unique_ids.chunks(400) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT trace_id, key_id, model, credits, final_status \
                 FROM traces WHERE ts_epoch >= ? AND ts_epoch <= ? \
                 AND trace_id IN ({placeholders})"
            );
            let mut params = Vec::<rusqlite::types::Value>::with_capacity(chunk.len() + 2);
            params.push(start_epoch.into());
            params.push(end_epoch.into());
            params.extend(chunk.iter().cloned().map(rusqlite::types::Value::Text));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
                Ok(ProfitTraceRecord {
                    trace_id: row.get(0)?,
                    key_id: row.get::<_, i64>(1)? as u64,
                    model: row.get(2)?,
                    credits: row.get(3)?,
                    final_status: row.get(4)?,
                })
            })?;
            records.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(records)
    }

    /// 按账号、按分钟汇总成功与 429，给 RPM 推算用。
    ///
    /// 只数 `outcome=success` 和 `http_status=429`。没状态的 unknown 跳过——那是
    /// 还没落地的诊断行，算进去会把分母撑爆。当前这分钟不算，由调用方把
    /// `end_epoch` 截到整分。
    pub fn query_rpm_minute_buckets(
        &self,
        start_epoch: i64,
        end_epoch: i64,
    ) -> Vec<crate::admin::rpm_infer::RpmMinuteBucket> {
        if start_epoch >= end_epoch {
            return Vec::new();
        }
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT a.credential_id,
                    (t.ts_epoch / 60) * 60,
                    SUM(CASE WHEN a.outcome = 'success' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN a.http_status = 429 THEN 1 ELSE 0 END)
             FROM traces t
             JOIN trace_attempts a ON a.trace_id = t.trace_id
             WHERE t.ts_epoch >= ?1 AND t.ts_epoch < ?2 AND a.credential_id > 0
             GROUP BY a.credential_id, (t.ts_epoch / 60) * 60",
        ) {
            Ok(stmt) => stmt,
            Err(error) => {
                tracing::warn!(%error, "RPM 分钟桶查询准备失败");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map([start_epoch, end_epoch], |row| {
            Ok(crate::admin::rpm_infer::RpmMinuteBucket {
                credential_id: row.get::<_, i64>(0)? as u64,
                minute_epoch: row.get(1)?,
                successes: row.get::<_, i64>(2)? as u32,
                rate_limited: row.get::<_, i64>(3)? as u32,
            })
        }) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "RPM 分钟桶查询失败");
                return Vec::new();
            }
        };
        rows.filter_map(|row| row.ok()).collect()
    }

    /// 按模型汇总「token 吞吐 ↔ credits 消耗」，供进价测算器估算一个号能产出多少 token。
    ///
    /// token 口径取四类之和（含 cache_read）：那是上游真实处理掉的量，也是运营口中
    /// 「这个号能干多少活」的意思。只算未缓存部分会把长会话的产出低估一个数量级。
    ///
    /// 只统计成功请求：失败请求既没产出也常常没计费，混进来会同时污染分子和分母。
    pub fn query_token_credit_stats(
        &self,
        start_epoch: i64,
        end_epoch: i64,
    ) -> rusqlite::Result<Vec<(String, f64, f64)>> {
        if start_epoch >= end_epoch {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT model, \
                    SUM(COALESCE(input_tokens,0) + COALESCE(output_tokens,0) \
                        + COALESCE(cache_read_tokens,0) + COALESCE(cache_creation_tokens,0)), \
                    SUM(COALESCE(credits,0)) \
             FROM traces \
             WHERE ts_epoch >= ? AND ts_epoch <= ? AND final_status = 'success' \
             GROUP BY model",
        )?;
        let rows = stmt.query_map(rusqlite::params![start_epoch, end_epoch], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1).unwrap_or(0.0),
                row.get::<_, f64>(2).unwrap_or(0.0),
            ))
        })?;
        rows.collect()
    }

    /// 测试辅助：仅取记录、忽略总数
    #[cfg(test)]
    fn query(&self, q: &TraceQuery) -> Vec<TraceRecord> {
        self.query_paged(q).0
    }

    /// 把 [`TraceQuery`] 的过滤条件拼成 WHERE 子句 + 参数（值全部参数化绑定）
    fn build_where(q: &TraceQuery) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(s) = &q.status {
            clauses.push("final_status = ?".to_string());
            params.push(Box::new(s.clone()));
        }
        if let Some(t) = &q.error_type {
            clauses.push("error_type = ?".to_string());
            params.push(Box::new(t.clone()));
        }
        if let Some(c) = q.credential_id {
            clauses.push("final_credential_id = ?".to_string());
            params.push(Box::new(c as i64));
        }
        if let Some(k) = q.key_id {
            clauses.push("key_id = ?".to_string());
            params.push(Box::new(k as i64));
        }
        if let Some(c) = q.failed_attempt_credential_id {
            // 该凭据在某一跳失败过（不论 trace 最终成功与否）
            clauses.push(
                "EXISTS (SELECT 1 FROM trace_attempts a \
                 WHERE a.trace_id = traces.trace_id \
                 AND a.credential_id = ? AND a.outcome != 'success')"
                    .to_string(),
            );
            params.push(Box::new(c as i64));
        }
        if let Some(m) = &q.model {
            clauses.push("model = ?".to_string());
            params.push(Box::new(m.clone()));
        }
        if let Some(diagnosis) = &q.compaction_diagnosis {
            clauses.push("compaction_diagnosis = ?".to_string());
            params.push(Box::new(diagnosis.clone()));
        }
        if let Some(session_hash) = &q.session_hash {
            clauses.push("session_hash = ?".to_string());
            params.push(Box::new(session_hash.clone()));
        }
        if let Some(ids) = &q.credential_ids {
            if ids.is_empty() {
                // 空白名单 = 该分组下无凭据 → 强制零匹配
                clauses.push("1=0".to_string());
            } else {
                let placeholders: Vec<&str> = ids.iter().map(|_| "?").collect();
                clauses.push(format!(
                    "final_credential_id IN ({})",
                    placeholders.join(",")
                ));
                for id in ids {
                    params.push(Box::new(*id as i64));
                }
            }
        }
        if q.only_failed {
            clauses.push("final_status != 'success'".to_string());
        }
        if q.high_pressure_only {
            clauses.push(
                "(COALESCE(upstream_context_percentage, 0) >= 80 \
                 OR COALESCE(request_body_bytes, 0) >= 2500000 \
                 OR COALESCE(compaction_diagnosis, 'normal') != 'normal')"
                    .to_string(),
            );
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        (where_sql, params)
    }

    fn query_inner(
        conn: &Connection,
        q: &TraceQuery,
    ) -> rusqlite::Result<(Vec<TraceRecord>, usize)> {
        let (where_sql, params) = Self::build_where(q);
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

        // 总数（用于前端分页）
        let count_sql = format!("SELECT COUNT(*) FROM traces {}", where_sql);
        let total: i64 = conn.query_row(&count_sql, param_refs.as_slice(), |row| row.get(0))?;

        let limit = if q.limit == 0 {
            DEFAULT_QUERY_LIMIT
        } else {
            q.limit
        };
        let sql = format!(
            "SELECT trace_id, ts, key_id, key_source, response_mode, model, is_stream, final_status, final_credential_id, \
             error_type, error_message, total_attempts, duration_ms, interrupted_after_bytes, \
             input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, credits, first_token_ms, \
             upstream_first_byte_ms, reasoning_effort, context_1m, thinking, empty_user_compat_applied, snapshot_id, \
             session_hash, client_version, compaction_diagnosis, request_body_bytes, \
             upstream_context_tokens, upstream_context_percentage, client_reported_tokens, \
             compaction_diagnostics_json \
             FROM traces {} ORDER BY ts_epoch DESC LIMIT {} OFFSET {}",
            where_sql, limit, q.offset
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            let diagnosis: Option<String> = row.get(28)?;
            let compaction = if let Some(diagnosis) = diagnosis {
                Some(CompactionTraceData {
                    session_hash: row.get(26)?,
                    client_version: row.get(27)?,
                    diagnosis,
                    request_body_bytes: row.get::<_, Option<i64>>(29)?.unwrap_or(0) as u64,
                    upstream_context_tokens: row
                        .get::<_, Option<i64>>(30)?
                        .map(|value| value as u64),
                    upstream_context_percentage: row.get(31)?,
                    client_reported_tokens: row
                        .get::<_, Option<i64>>(32)?
                        .map(|value| value as u64),
                    diagnostics_json: row
                        .get::<_, Option<String>>(33)?
                        .unwrap_or_else(|| "{\"schemaVersion\":1}".to_string()),
                })
            } else {
                None
            };
            Ok(TraceRecord {
                trace_id: row.get(0)?,
                ts: row.get(1)?,
                key_id: row.get::<_, i64>(2)? as u64,
                key_source: TraceKeySource::from_db(row.get::<_, String>(3)?.as_str(), 3)?,
                response_mode: row
                    .get::<_, String>(4)?
                    .parse()
                    .unwrap_or(ClientResponseMode::Detection),
                model: row.get(5)?,
                is_stream: row.get::<_, i64>(6)? != 0,
                final_status: row.get(7)?,
                final_credential_id: row.get::<_, i64>(8)? as u64,
                error_type: row.get(9)?,
                error_message: row.get(10)?,
                total_attempts: row.get::<_, i64>(11)? as u32,
                duration_ms: row.get::<_, i64>(12)? as u64,
                interrupted_after_bytes: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
                input_tokens: row.get::<_, i64>(14)? as u64,
                output_tokens: row.get::<_, i64>(15)? as u64,
                cache_creation_tokens: row.get::<_, i64>(16)? as u64,
                cache_read_tokens: row.get::<_, i64>(17)? as u64,
                credits: row.get::<_, f64>(18)?,
                first_token_ms: row.get::<_, Option<i64>>(19)?.map(|v| v as u64),
                upstream_first_byte_ms: row.get::<_, Option<i64>>(20)?.map(|v| v as u64),
                reasoning_effort: row.get::<_, Option<String>>(21)?,
                context_1m: row.get::<_, i64>(22)? != 0,
                thinking: row.get::<_, i64>(23)? != 0,
                empty_user_compat_applied: row.get::<_, i64>(24)? != 0,
                snapshot_id: row.get(25)?,
                compaction,
                attempts: Vec::new(),
            })
        })?;
        let mut records: Vec<TraceRecord> = rows.collect::<rusqlite::Result<_>>()?;

        // 批量取每条 trace 的 attempts
        let mut attempt_stmt = conn.prepare(
            "SELECT attempt, credential_id, endpoint, http_status, outcome, error_snippet, \
             duration_ms FROM trace_attempts WHERE trace_id = ? ORDER BY attempt ASC",
        )?;
        for rec in &mut records {
            let attempts = attempt_stmt.query_map([&rec.trace_id], |row| {
                Ok(TraceAttempt {
                    attempt: row.get::<_, i64>(0)? as u32,
                    credential_id: row.get::<_, i64>(1)? as u64,
                    endpoint: row.get(2)?,
                    http_status: row.get::<_, Option<i64>>(3)?.map(|v| v as u16),
                    outcome: row.get(4)?,
                    error_snippet: row.get(5)?,
                    duration_ms: row.get::<_, i64>(6)? as u64,
                })
            })?;
            rec.attempts = attempts.collect::<rusqlite::Result<_>>()?;
        }
        Ok((records, total as usize))
    }

    /// 删除超过保留期的记录（traces + 关联 attempts）。仅 warn 失败。
    pub fn cleanup(&self) {
        let cutoff =
            (Utc::now() - chrono::Duration::days(self.retention_days() as i64)).timestamp();
        let mut conn = self.conn.lock();
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("trace 清理事务失败: {}", e);
                return;
            }
        };
        let res = (|| -> rusqlite::Result<usize> {
            tx.execute(
                "DELETE FROM trace_attempts WHERE trace_id IN \
                 (SELECT trace_id FROM traces WHERE ts_epoch < ?1)",
                [cutoff],
            )?;
            let n = tx.execute("DELETE FROM traces WHERE ts_epoch < ?1", [cutoff])?;
            Ok(n)
        })();
        match res {
            Ok(n) => {
                if let Err(e) = tx.commit() {
                    tracing::warn!("trace 清理提交失败: {}", e);
                } else if n > 0 {
                    tracing::info!("已清理 {} 条过期 trace 记录", n);
                }
            }
            Err(e) => tracing::warn!("trace 清理失败: {}", e),
        }
    }

    /// 删除指定凭据关联的 trace 记录，避免删除账号后新账号复用同一 credential_id
    /// 时继承旧账号的失败统计。
    pub fn delete_for_credential(&self, credential_id: u64) {
        if credential_id == 0 {
            return;
        }
        let mut conn = self.conn.lock();
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("trace 凭据清理事务失败: {}", e);
                return;
            }
        };
        let res = (|| -> rusqlite::Result<usize> {
            tx.execute(
                "DELETE FROM trace_attempts WHERE credential_id = ?1 \
                 OR trace_id IN (SELECT trace_id FROM traces WHERE final_credential_id = ?1)",
                [credential_id],
            )?;
            let n = tx.execute(
                "DELETE FROM traces WHERE final_credential_id = ?1",
                [credential_id],
            )?;
            Ok(n)
        })();
        match res {
            Ok(n) => {
                if let Err(e) = tx.commit() {
                    tracing::warn!("trace 凭据清理提交失败: {}", e);
                } else if n > 0 {
                    tracing::info!("已清理凭据 #{} 的 {} 条 trace 记录", credential_id, n);
                }
            }
            Err(e) => tracing::warn!("trace 凭据清理失败: {}", e),
        }
    }

    /// 清空全部 trace 记录（traces + 关联 attempts）。返回删除的 traces 行数。
    /// 用于管理面板「清空请求日志」按钮。仅 warn 失败，失败时返回 0。
    pub fn clear_all(&self) -> usize {
        let mut conn = self.conn.lock();
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("trace 清空事务失败: {}", e);
                return 0;
            }
        };
        let res = (|| -> rusqlite::Result<usize> {
            tx.execute("DELETE FROM trace_attempts", [])?;
            let n = tx.execute("DELETE FROM traces", [])?;
            Ok(n)
        })();
        match res {
            Ok(n) => match tx.commit() {
                Ok(()) => {
                    if n > 0 {
                        tracing::info!("已清空全部 trace 记录（{} 条）", n);
                    }
                    n
                }
                Err(e) => {
                    tracing::warn!("trace 清空提交失败: {}", e);
                    0
                }
            },
            Err(e) => {
                tracing::warn!("trace 清空失败: {}", e);
                0
            }
        }
    }

    /// 按凭据聚合失败跳数，归并为三类：鉴权 / 账号风控 / 其他。
    /// 统计 trace_attempts 里 outcome != 'success' 的跳，按 credential_id + outcome 分组。
    /// 返回 credential_id → (auth, throttle, other)。仅 warn 失败，返回空。
    pub fn failure_stats(&self) -> std::collections::HashMap<u64, FailureStats> {
        let conn = self.conn.lock();
        let mut out: std::collections::HashMap<u64, FailureStats> =
            std::collections::HashMap::new();
        let mut stmt = match conn.prepare(
            "SELECT credential_id, outcome, COUNT(*) FROM trace_attempts \
             WHERE outcome != 'success' AND credential_id != 0 \
             GROUP BY credential_id, outcome",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("trace failure_stats prepare 失败: {}", e);
                return out;
            }
        };
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
            ))
        });
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("trace failure_stats 查询失败: {}", e);
                return out;
            }
        };
        for r in rows.flatten() {
            let (cred, outcome_str, cnt) = r;
            let s = out.entry(cred).or_default();
            match outcome_str.as_str() {
                "auth_failed" => s.auth += cnt,
                "account_throttled" => s.throttle += cnt,
                _ => s.other += cnt,
            }
        }
        out
    }
}

/// 按凭据的失败分类计数（鉴权 / 账号风控 / 其他）
#[derive(Debug, Default, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureStats {
    pub auth: u64,
    pub throttle: u64,
    pub other: u64,
}

/// 共享存储句柄
pub type SharedTraceStore = Arc<TraceStore>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS traces (
    trace_id          TEXT PRIMARY KEY,
    ts                TEXT NOT NULL,
    ts_epoch          INTEGER NOT NULL,
    key_id            INTEGER NOT NULL,
    key_source        TEXT,
    response_mode     TEXT NOT NULL DEFAULT 'detection',
    model             TEXT NOT NULL,
    is_stream         INTEGER NOT NULL,
    final_status      TEXT NOT NULL,
    final_credential_id INTEGER NOT NULL,
    error_type        TEXT,
    error_message     TEXT,
    total_attempts    INTEGER NOT NULL,
    duration_ms       INTEGER NOT NULL,
    interrupted_after_bytes INTEGER,
    input_tokens      INTEGER NOT NULL DEFAULT 0,
    output_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    credits           REAL NOT NULL DEFAULT 0,
    first_token_ms    INTEGER,
    upstream_first_byte_ms INTEGER,
    reasoning_effort  TEXT,
    context_1m        INTEGER NOT NULL DEFAULT 0,
    thinking          INTEGER NOT NULL DEFAULT 0,
    empty_user_compat_applied INTEGER NOT NULL DEFAULT 0,
    snapshot_id       TEXT,
    session_hash      TEXT,
    client_version    TEXT,
    compaction_diagnosis TEXT,
    request_body_bytes INTEGER,
    upstream_context_tokens INTEGER,
    upstream_context_percentage REAL,
    client_reported_tokens INTEGER,
    compaction_diagnostics_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_traces_ts ON traces(ts_epoch DESC);
CREATE INDEX IF NOT EXISTS idx_traces_status ON traces(final_status);
CREATE INDEX IF NOT EXISTS idx_traces_cred ON traces(final_credential_id);

CREATE TABLE IF NOT EXISTS trace_attempts (
    trace_id      TEXT NOT NULL,
    attempt       INTEGER NOT NULL,
    credential_id INTEGER NOT NULL,
    endpoint      TEXT NOT NULL,
    http_status   INTEGER,
    outcome       TEXT NOT NULL,
    error_snippet TEXT,
    duration_ms   INTEGER NOT NULL,
    PRIMARY KEY (trace_id, attempt)
);
CREATE INDEX IF NOT EXISTS idx_attempts_trace ON trace_attempts(trace_id);
";

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    pub(super) struct TraceSample<'a> {
        pub(super) trace_id: &'a str,
        pub(super) status: &'a str,
        pub(super) credential_id: u64,
        pub(super) model: &'a str,
    }

    pub(super) fn sample(input: TraceSample<'_>) -> TraceRecord {
        TraceRecord {
            trace_id: input.trace_id.to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 1,
            key_source: TraceKeySource::ClientKey,
            response_mode: crate::admin::client_keys::ClientResponseMode::KiroNative,
            model: input.model.to_string(),
            is_stream: true,
            final_status: input.status.to_string(),
            final_credential_id: input.credential_id,
            error_type: if input.status == "success" {
                None
            } else {
                Some(outcome::ACCOUNT_THROTTLED.to_string())
            },
            error_message: if input.status == "success" {
                None
            } else {
                Some("blocked".to_string())
            },
            total_attempts: 2,
            duration_ms: 1200,
            interrupted_after_bytes: None,
            input_tokens: 1093,
            output_tokens: 779,
            cache_creation_tokens: 0,
            cache_read_tokens: 101760,
            credits: 0.0,
            first_token_ms: Some(3200),
            upstream_first_byte_ms: Some(2800),
            reasoning_effort: None,
            context_1m: false,
            thinking: false,
            empty_user_compat_applied: false,
            snapshot_id: None,
            compaction: None,
            attempts: vec![
                TraceAttempt {
                    attempt: 0,
                    credential_id: 9,
                    endpoint: "ide".to_string(),
                    http_status: Some(429),
                    outcome: outcome::ACCOUNT_THROTTLED.to_string(),
                    error_snippet: Some("suspicious activity".to_string()),
                    duration_ms: 400,
                },
                TraceAttempt {
                    attempt: 1,
                    credential_id: input.credential_id,
                    endpoint: "ide".to_string(),
                    http_status: if input.status == "success" {
                        Some(200)
                    } else {
                        None
                    },
                    outcome: input.status.to_string(),
                    error_snippet: None,
                    duration_ms: 800,
                },
            ],
        }
    }

    fn mem_store() -> TraceStore {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        // writer 为 None：单测走同步写路径，保证「insert 后立刻能查到」。
        TraceStore {
            conn: Mutex::new(conn),
            enabled: AtomicBool::new(true),
            retention_days: AtomicU64::new(DEFAULT_RETENTION_DAYS),
            writer: Mutex::new(None),
            dropped: AtomicU64::new(0),
        }
    }

    fn compaction(
        session_hash: &str,
        diagnosis: &str,
        request_body_bytes: u64,
        upstream_context_percentage: Option<f64>,
    ) -> crate::anthropic::compaction_diagnostics::CompactionTraceData {
        crate::anthropic::compaction_diagnostics::CompactionTraceData {
            session_hash: Some(session_hash.to_string()),
            client_version: Some("2.1.220".to_string()),
            diagnosis: diagnosis.to_string(),
            request_body_bytes,
            upstream_context_tokens: upstream_context_percentage
                .map(|percentage| (percentage * 10_000.0) as u64),
            upstream_context_percentage,
            client_reported_tokens: upstream_context_percentage
                .map(|percentage| (percentage * 10_000.0) as u64),
            diagnostics_json: "{\"schemaVersion\":1,\"containsOnlySafeCounters\":true}".to_string(),
        }
    }

    #[test]
    fn compaction_migration_is_idempotent_and_creates_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE traces (
                trace_id TEXT PRIMARY KEY,
                ts TEXT NOT NULL,
                ts_epoch INTEGER NOT NULL,
                key_id INTEGER NOT NULL,
                model TEXT NOT NULL,
                is_stream INTEGER NOT NULL,
                final_status TEXT NOT NULL,
                final_credential_id INTEGER NOT NULL,
                total_attempts INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL
            );",
        )
        .unwrap();

        TraceStore::migrate(&conn).unwrap();
        TraceStore::migrate(&conn).unwrap();

        let columns: std::collections::HashSet<String> = conn
            .prepare("PRAGMA table_info(traces)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for column in [
            "session_hash",
            "client_version",
            "compaction_diagnosis",
            "request_body_bytes",
            "upstream_context_tokens",
            "upstream_context_percentage",
            "client_reported_tokens",
            "compaction_diagnostics_json",
        ] {
            assert!(columns.contains(column), "missing migrated column {column}");
        }
        let indexes: std::collections::HashSet<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(indexes.contains("idx_traces_session_ts"));
        assert!(indexes.contains("idx_traces_compaction_diagnosis"));
    }

    #[test]
    fn compaction_fields_round_trip_and_support_all_filters() {
        let store = mem_store();
        for (trace_id, session, diagnosis, bytes, percentage) in [
            (
                "compaction-high",
                "session-high",
                "context_signal_enqueued",
                3_000_000,
                Some(90.0),
            ),
            (
                "compaction-normal",
                "session-normal",
                "normal",
                100_000,
                Some(10.0),
            ),
        ] {
            let mut record = sample(TraceSample {
                trace_id,
                status: "success",
                credential_id: 5,
                model: "claude-opus-4-8",
            });
            record.compaction = Some(compaction(session, diagnosis, bytes, percentage));
            store.insert(record);
        }

        let by_diagnosis = store.query(&TraceQuery {
            compaction_diagnosis: Some("context_signal_enqueued".to_string()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(by_diagnosis.len(), 1);
        let details = by_diagnosis[0].compaction.as_ref().unwrap();
        assert_eq!(details.session_hash.as_deref(), Some("session-high"));
        assert_eq!(details.client_version.as_deref(), Some("2.1.220"));
        assert_eq!(details.request_body_bytes, 3_000_000);
        assert_eq!(details.upstream_context_percentage, Some(90.0));
        assert!(
            details
                .diagnostics_json
                .contains("containsOnlySafeCounters")
        );

        let by_session = store.query(&TraceQuery {
            session_hash: Some("session-normal".to_string()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(by_session.len(), 1);
        assert_eq!(by_session[0].trace_id, "compaction-normal");

        let high_pressure = store.query(&TraceQuery {
            high_pressure_only: true,
            limit: 10,
            ..Default::default()
        });
        assert_eq!(high_pressure.len(), 1);
        assert_eq!(high_pressure[0].trace_id, "compaction-high");
    }

    #[test]
    fn compaction_inference_distinguishes_not_triggered_from_insufficient() {
        let store = mem_store();
        let base = DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap();

        let mut previous = sample(TraceSample {
            trace_id: "no-trigger-previous",
            status: "success",
            credential_id: 5,
            model: "claude-opus-4-8",
        });
        previous.ts = base.to_rfc3339();
        previous.compaction = Some(compaction(
            "session-no-trigger",
            "context_signal_enqueued",
            3_000_000,
            Some(90.0),
        ));
        store.insert(previous);

        let mut next = sample(TraceSample {
            trace_id: "no-trigger-next",
            status: "success",
            credential_id: 5,
            model: "claude-opus-4-8",
        });
        next.ts = (base + chrono::Duration::seconds(1)).to_rfc3339();
        next.compaction = Some(compaction(
            "session-no-trigger",
            "upstream_context_unknown",
            2_700_000,
            None,
        ));
        store.insert(next);

        let mut before_compaction = sample(TraceSample {
            trace_id: "insufficient-previous",
            status: "success",
            credential_id: 5,
            model: "claude-opus-4-8",
        });
        before_compaction.ts = base.to_rfc3339();
        before_compaction.compaction = Some(compaction(
            "session-insufficient",
            "context_signal_enqueued",
            3_000_000,
            Some(90.0),
        ));
        store.insert(before_compaction);

        let mut after_compaction = sample(TraceSample {
            trace_id: "insufficient-next",
            status: "error",
            credential_id: 5,
            model: "claude-opus-4-8",
        });
        after_compaction.ts = (base + chrono::Duration::seconds(1)).to_rfc3339();
        after_compaction.compaction = Some(compaction(
            "session-insufficient",
            "payload_limit_preempted",
            2_300_000,
            None,
        ));
        store.insert(after_compaction);

        let not_triggered = store.query(&TraceQuery {
            compaction_diagnosis: Some("suspected_client_compaction_not_triggered".to_string()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(not_triggered.len(), 1);
        assert_eq!(not_triggered[0].trace_id, "no-trigger-next");
        assert_eq!(
            not_triggered[0]
                .compaction
                .as_ref()
                .unwrap()
                .diagnostics_json,
            "{\"schemaVersion\":1,\"containsOnlySafeCounters\":true}"
        );

        // 正向观测：上一轮高压 + 本轮明显缩小 → 客户端确实压缩了。
        // 这一条是验证「压缩信号阈值降到 85%」是否生效的关键计数。
        let mut compacted_previous = sample(TraceSample {
            trace_id: "compacted-previous",
            status: "success",
            credential_id: 5,
            model: "claude-opus-4-8",
        });
        compacted_previous.ts = base.to_rfc3339();
        compacted_previous.compaction = Some(compaction(
            "session-compacted",
            "context_signal_enqueued",
            3_000_000,
            Some(90.0),
        ));
        store.insert(compacted_previous);

        let mut compacted_next = sample(TraceSample {
            trace_id: "compacted-next",
            status: "success",
            credential_id: 5,
            model: "claude-opus-4-8",
        });
        compacted_next.ts = (base + chrono::Duration::seconds(1)).to_rfc3339();
        // 缩到 40%，远低于 80% 的门槛，且不是 payload_limit_preempted。
        compacted_next.compaction =
            Some(compaction("session-compacted", "normal", 1_200_000, None));
        store.insert(compacted_next);

        let compacted = store.query(&TraceQuery {
            compaction_diagnosis: Some("client_compaction_observed".to_string()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(compacted.len(), 1, "客户端压缩必须被正向观测到");
        assert_eq!(compacted[0].trace_id, "compacted-next");

        let insufficient = store.query(&TraceQuery {
            compaction_diagnosis: Some("suspected_compaction_insufficient".to_string()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(insufficient.len(), 1);
        assert_eq!(insufficient[0].trace_id, "insufficient-next");
    }

    /// 启动写入器后，`insert` 必须只入队、不碰磁盘。
    ///
    /// 这是 2026-07-26 线上事故的回归守卫：当时 `insert` 在异步请求路径上做
    /// 「全局 Mutex + 同步 SQLite 事务」，traces.db 涨到 681MB 后，高并发下所有
    /// Tokio worker 都堵在这把锁上，运行时整体停转——上游一条 TCP 连接都建不起来，
    /// 入站连接堆到 500+，吞吐从 219/分钟塌到个位数，只能靠重启恢复。
    ///
    /// 用「持有 conn 锁的同时调用 insert」来证明它不再走同步写：若 insert 仍然去锁
    /// conn，这里会直接死锁；能返回就说明它把记录交给了队列。
    #[tokio::test]
    async fn insert_does_not_touch_the_database_lock_once_writer_is_running() {
        let store = Arc::new(mem_store());
        store.spawn_writer();

        let held = store.conn.lock();
        // 若 insert 内部仍尝试 self.conn.lock()，此处会永久阻塞。
        store.insert(sample(TraceSample {
            trace_id: "queued-1",
            status: outcome::SUCCESS,
            credential_id: 1,
            model: "claude-opus-4-8",
        }));
        drop(held);

        assert_eq!(store.dropped_count(), 0, "队列未满时不应丢弃");
    }

    /// 队列满时丢弃而不是阻塞——可观测性数据永远不能拖慢真实流量。
    #[tokio::test]
    async fn full_queue_drops_records_instead_of_blocking_the_request() {
        let store = Arc::new(mem_store());
        // 只建通道、不启动消费者，让队列必然填满。
        let (tx, _rx) = tokio::sync::mpsc::channel::<TraceRecord>(2);
        *store.writer.lock() = Some(tx);

        for _ in 0..64 {
            store.insert(sample(TraceSample {
                trace_id: "flood",
                status: outcome::SUCCESS,
                credential_id: 1,
                model: "claude-opus-4-8",
            }));
        }

        assert!(
            store.dropped_count() > 0,
            "队列填满后必须计数丢弃，而不是等待消费者"
        );
    }

    #[test]
    fn insert_and_query_roundtrip() {
        let store = mem_store();
        store.insert(sample(TraceSample {
            trace_id: "t1",
            status: "success",
            credential_id: 5,
            model: "claude-opus-4-7",
        }));
        let out = store.query(&TraceQuery {
            limit: 50,
            ..Default::default()
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].trace_id, "t1");
        assert_eq!(out[0].attempts.len(), 2);
        assert_eq!(out[0].attempts[0].outcome, outcome::ACCOUNT_THROTTLED);
        assert_eq!(out[0].key_source, TraceKeySource::ClientKey);
        assert_eq!(
            out[0].response_mode,
            crate::admin::client_keys::ClientResponseMode::KiroNative
        );
        assert_eq!(
            serde_json::to_value(&out[0]).unwrap()["responseMode"],
            "kiro_native"
        );
        // token 分项往返
        assert_eq!(out[0].input_tokens, 1093);
        assert_eq!(out[0].output_tokens, 779);
        assert_eq!(out[0].cache_read_tokens, 101760);
        assert_eq!(out[0].cache_creation_tokens, 0);
        assert_eq!(out[0].first_token_ms, Some(3200));
        assert_eq!(out[0].upstream_first_byte_ms, Some(2800));
        assert_eq!(
            serde_json::to_value(&out[0]).unwrap()["emptyUserCompatApplied"],
            false
        );
    }

    #[test]
    fn query_profit_traces_matches_exact_ids_inside_window() {
        let store = mem_store();
        let mut inside = sample(TraceSample {
            trace_id: "trace-inside",
            status: "success",
            credential_id: 5,
            model: "claude-opus-4-8",
        });
        inside.ts = DateTime::<Utc>::from_timestamp(1_700_000_100, 0)
            .unwrap()
            .to_rfc3339();
        inside.key_id = 42;
        inside.credits = 0.75;
        store.insert(inside.clone());

        let mut outside = inside.clone();
        outside.trace_id = "trace-outside".to_string();
        outside.ts = DateTime::<Utc>::from_timestamp(1_699_999_999, 0)
            .unwrap()
            .to_rfc3339();
        store.insert(outside.clone());

        let rows = store
            .query_profit_traces(
                &[
                    "trace-inside".to_string(),
                    "trace-outside".to_string(),
                    "trace-missing".to_string(),
                ],
                1_700_000_000,
                1_700_000_200,
            )
            .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].trace_id, "trace-inside");
        assert_eq!(rows[0].key_id, 42);
        assert_eq!(rows[0].model, "claude-opus-4-8");
        assert_eq!(rows[0].credits, 0.75);
        assert_eq!(rows[0].final_status, "success");
    }

    #[test]
    fn response_mode_migrates_old_trace_rows_to_detection() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE traces (
                trace_id TEXT PRIMARY KEY,
                ts TEXT NOT NULL,
                ts_epoch INTEGER NOT NULL,
                key_id INTEGER NOT NULL,
                model TEXT NOT NULL,
                is_stream INTEGER NOT NULL,
                final_status TEXT NOT NULL,
                final_credential_id INTEGER NOT NULL,
                total_attempts INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL
            );
            CREATE TABLE trace_attempts (
                trace_id TEXT NOT NULL,
                attempt INTEGER NOT NULL,
                credential_id INTEGER NOT NULL,
                endpoint TEXT NOT NULL,
                http_status INTEGER,
                outcome TEXT NOT NULL,
                error_snippet TEXT,
                duration_ms INTEGER NOT NULL,
                PRIMARY KEY (trace_id, attempt)
            );
            INSERT INTO traces (
                trace_id, ts, ts_epoch, key_id, model, is_stream,
                final_status, final_credential_id, total_attempts, duration_ms
            ) VALUES ('legacy', '2026-07-15T00:00:00Z', 1, 1, 'm', 0, 'success', 1, 0, 1);",
        )
        .unwrap();
        TraceStore::migrate(&conn).unwrap();
        let value: String = conn
            .query_row(
                "SELECT response_mode FROM traces WHERE trace_id = 'legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(value, "detection");
    }

    #[test]
    fn migrates_snapshot_id_and_round_trips_it() {
        let store = TraceStore::open_in_memory().unwrap();
        let mut rec = sample(TraceSample {
            trace_id: "trace-with-snapshot",
            status: "error",
            credential_id: 7,
            model: "claude-opus-4-8",
        });
        rec.snapshot_id = Some("snap-7".into());
        store.insert(rec.clone());
        let out = store.query(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(out[0].snapshot_id.as_deref(), Some("snap-7"));
    }

    #[test]
    fn links_existing_trace_idempotently() {
        let store = TraceStore::open_in_memory().unwrap();
        let rec = sample(TraceSample {
            trace_id: "trace-link",
            status: "error",
            credential_id: 7,
            model: "claude-opus-4-8",
        });
        store.insert(rec.clone());
        assert!(store.link_snapshot("trace-link", "snap-link"));
        assert!(store.link_snapshot("trace-link", "snap-link"));
        assert_eq!(
            store.query(&TraceQuery {
                limit: 10,
                ..Default::default()
            })[0]
                .snapshot_id
                .as_deref(),
            Some("snap-link")
        );
    }

    #[test]
    fn disabled_skips_insert() {
        let store = mem_store();
        store.set_enabled(false);
        store.insert(sample(TraceSample {
            trace_id: "t1",
            status: "success",
            credential_id: 5,
            model: "m1",
        }));
        let out = store.query(&TraceQuery {
            limit: 50,
            ..Default::default()
        });
        assert_eq!(out.len(), 0, "trace 关闭时不应写入");
        // 重新开启后写入恢复
        store.set_enabled(true);
        store.insert(sample(TraceSample {
            trace_id: "t2",
            status: "success",
            credential_id: 5,
            model: "m1",
        }));
        assert_eq!(
            store
                .query(&TraceQuery {
                    limit: 50,
                    ..Default::default()
                })
                .len(),
            1
        );
    }

    #[test]
    fn delete_for_credential_removes_failure_stats() {
        let store = mem_store();
        store.insert(sample(TraceSample {
            trace_id: "old",
            status: "error",
            credential_id: 5,
            model: "m1",
        }));
        store.insert(sample(TraceSample {
            trace_id: "keep",
            status: "error",
            credential_id: 6,
            model: "m1",
        }));

        assert!(store.failure_stats().contains_key(&5));
        store.delete_for_credential(5);

        let stats = store.failure_stats();
        assert!(!stats.contains_key(&5));
        assert!(stats.contains_key(&6));
        assert!(
            store
                .query(&TraceQuery {
                    credential_id: Some(5),
                    limit: 50,
                    ..Default::default()
                })
                .is_empty(),
            "deleted credential traces should not attach to a future account with the same id"
        );
    }

    #[test]
    fn filter_only_failed_and_status() {
        let store = mem_store();
        store.insert(sample(TraceSample {
            trace_id: "ok",
            status: "success",
            credential_id: 5,
            model: "m1",
        }));
        store.insert(sample(TraceSample {
            trace_id: "bad",
            status: "error",
            credential_id: 6,
            model: "m1",
        }));
        store.insert(sample(TraceSample {
            trace_id: "cut",
            status: "interrupted",
            credential_id: 7,
            model: "m2",
        }));

        let failed = store.query(&TraceQuery {
            only_failed: true,
            limit: 50,
            ..Default::default()
        });
        assert_eq!(failed.len(), 2);
        assert!(failed.iter().all(|r| r.final_status != "success"));

        let by_status = store.query(&TraceQuery {
            status: Some("interrupted".to_string()),
            limit: 50,
            ..Default::default()
        });
        assert_eq!(by_status.len(), 1);
        assert_eq!(by_status[0].trace_id, "cut");

        let by_model = store.query(&TraceQuery {
            model: Some("m2".to_string()),
            limit: 50,
            ..Default::default()
        });
        assert_eq!(by_model.len(), 1);
        assert_eq!(by_model[0].trace_id, "cut");
    }

    #[test]
    fn cleanup_removes_old() {
        let store = mem_store();
        store.insert(sample(TraceSample {
            trace_id: "recent",
            status: "success",
            credential_id: 5,
            model: "m1",
        }));
        // 手动塞一条 8 天前的记录
        {
            let conn = store.conn.lock();
            let old = (Utc::now() - chrono::Duration::days(8)).timestamp();
            conn.execute(
                "INSERT INTO traces (trace_id, ts, ts_epoch, key_id, key_source, model, is_stream, \
                 final_status, final_credential_id, total_attempts, duration_ms) \
                 VALUES ('old','2020',?1,1,'clientKey','m',1,'success',1,1,1)",
                [old],
            )
            .unwrap();
        }
        store.cleanup();
        let out = store.query(&TraceQuery {
            limit: 50,
            ..Default::default()
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].trace_id, "recent");
    }

    #[test]
    fn clear_all_removes_everything() {
        let store = mem_store();
        store.insert(sample(TraceSample {
            trace_id: "a",
            status: "success",
            credential_id: 5,
            model: "m1",
        }));
        store.insert(sample(TraceSample {
            trace_id: "b",
            status: "error",
            credential_id: 6,
            model: "m1",
        }));
        let cleared = store.clear_all();
        assert_eq!(cleared, 2);
        let out = store.query(&TraceQuery {
            limit: 50,
            ..Default::default()
        });
        assert!(out.is_empty(), "clear_all 后应无任何 trace");
        // attempts 也应清空：failure_stats 不再有任何条目
        assert!(
            store.failure_stats().is_empty(),
            "clear_all 后 attempts 应清空"
        );
        // 空库再清一次返回 0，不报错
        assert_eq!(store.clear_all(), 0);
    }

    #[test]
    fn query_rpm_minute_buckets_counts_success_and_429() {
        let store = mem_store();
        store.insert(sample(TraceSample {
            trace_id: "a",
            status: "success",
            credential_id: 5,
            model: "m1",
        }));
        store.insert(sample(TraceSample {
            trace_id: "b",
            status: "success",
            credential_id: 5,
            model: "m1",
        }));
        let now = Utc::now().timestamp();
        let buckets = store.query_rpm_minute_buckets(now - 120, now + 60);
        let cred5 = buckets
            .iter()
            .find(|bucket| bucket.credential_id == 5)
            .expect("success credential");
        let cred9 = buckets
            .iter()
            .find(|bucket| bucket.credential_id == 9)
            .expect("429 first hop");
        assert_eq!(cred5.successes, 2);
        assert_eq!(cred5.rate_limited, 0);
        assert_eq!(cred9.successes, 0);
        assert_eq!(cred9.rate_limited, 2);
    }

    #[test]
    fn query_inner_rejects_unknown_key_source() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO traces (trace_id, ts, ts_epoch, key_id, key_source, model, is_stream, \
             final_status, final_credential_id, total_attempts, duration_ms) \
             VALUES ('bad-source','2020',1,1,'unknown','m',1,'success',1,1,1)",
            [],
        )
        .unwrap();

        let result = TraceStore::query_inner(
            &conn,
            &TraceQuery {
                limit: 50,
                ..Default::default()
            },
        );

        assert!(result.is_err());
    }

    #[test]
    fn response_mode_unknown_disk_value_falls_back_to_detection() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO traces (trace_id, ts, ts_epoch, key_id, key_source, response_mode, model, is_stream, \
             final_status, final_credential_id, total_attempts, duration_ms) \
             VALUES ('future-mode','2020',1,1,'clientKey','future_mode','m',1,'success',1,1,1)",
            [],
        )
        .unwrap();

        let (records, total) = TraceStore::query_inner(
            &conn,
            &TraceQuery {
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(total, 1);
        assert_eq!(records[0].response_mode, ClientResponseMode::Detection);
    }

    #[test]
    fn truncate_snippet_respects_limit() {
        assert_eq!(truncate_snippet("  "), None);
        assert_eq!(truncate_snippet("hi"), Some("hi".to_string()));
        let long = "x".repeat(ERROR_SNIPPET_MAX + 100);
        let out = truncate_snippet(&long).unwrap();
        assert!(out.ends_with("…(truncated)"));
        assert!(out.len() <= ERROR_SNIPPET_MAX + 20);
    }
}

#[cfg(test)]
mod wal_checkpoint_tests {
    use super::*;

    /// 唯一目录，避免并行测试互相踩同一个 traces.db。
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kiro-rs-wal-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn checkpoint_truncate_actually_shrinks_the_wal_file() {
        // 这是 Step 1 的全部意义所在。PASSIVE 自动检查点只把页搬回主库、从头复用
        // WAL，**不缩文件**；进程又一直硬退出，没人截断。线上 traces.db-wal 因此涨到
        // 307 MB，每次启动都要为它做恢复。断言必须落在「文件真的变小了」上，
        // 只断言 checkpoint 返回 Ok 会让这个 bug 原样溜回来。
        let dir = scratch_dir("shrink");
        let db = dir.join("traces.db");
        let wal = dir.join("traces.db-wal");

        let store = TraceStore::open(db.clone(), true, 7).unwrap();
        for index in 0..200 {
            store.insert(super::tests::sample(super::tests::TraceSample {
                trace_id: &format!("trace-{index}"),
                status: "success",
                credential_id: 1,
                model: "claude-sonnet-4-6",
            }));
        }
        let grown = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert!(grown > 0, "WAL 应该已经有内容，否则这个测试没在测东西");

        store.checkpoint_truncate().unwrap();

        let after = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert_eq!(after, 0, "截断后 WAL 必须是 0 字节，涨到 {grown} 却没缩");

        // 数据不能被截断带走：checkpoint 是把页搬回主库，不是丢弃。
        drop(store);
        let reopened = TraceStore::open(db, true, 7).unwrap();
        let (_, total) = reopened.query_paged(&TraceQuery::default());
        assert_eq!(total, 200, "截断不能丢数据，页应该已经搬回主库");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
