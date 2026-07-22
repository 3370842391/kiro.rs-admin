//! 批发系统 SQLite 数据层（P1）
//!
//! 单文件 `wholesale.db`，表：mothers / customers / holdings / cdks / wallet_entries。
//! 参考 `admin::TraceStore` 的 `Mutex<Connection>` + WAL 模式。
//!
//! **资金安全铁律**：余额相关(扣费/退款/CDK 兑换/购买)必须在**单个事务**内
//! 完成"改余额 + 写账本"，账本不可改只能反向记账；并发占号用事务防一号双发。

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub type SharedWholesaleStore = Arc<WholesaleStore>;

/// 默认价格：700 分 = 7 元 / 每交付一个正常号
pub const DEFAULT_PRICE_CENTS: i64 = 700;
/// 默认质保时长：30 分钟
pub const DEFAULT_WARRANTY_SECS: i64 = 1800;
/// 默认常驻号池目标数
pub const DEFAULT_TARGET: i64 = 5;
/// 死亡号界面墓碑期：5 分钟后从实时视图清理
pub const PURGE_AFTER_SECS: i64 = 300;

/// 母号状态
pub mod mother_state {
    pub const FRESH: &str = "fresh";
    pub const ACTIVE: &str = "active";
    pub const AGING: &str = "aging";
    pub const DEAD: &str = "dead";
}

/// 持有状态
pub mod holding_status {
    pub const ACTIVE: &str = "active";
    pub const LOW_QUOTA: &str = "low_quota";
    pub const DEAD: &str = "dead";
}

// ───────────────────────── 数据结构 ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mother {
    pub directory_id: String,
    pub start_url: String,
    pub region: Option<String>,
    pub added_at: String,
    pub state: String,
    pub died_at: Option<String>,
    pub child_total: i64,
    pub child_alive: i64,
    pub child_dead: i64,
    pub last_death_at: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Customer {
    pub id: i64,
    pub uid: String,
    #[serde(skip_serializing)] // 绝不外泄 key 明文
    pub api_key: String,
    pub username: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub email: Option<String>,
    pub balance_cents: i64,
    pub target: i64,
    pub disabled: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Holding {
    pub id: i64,
    pub customer_id: i64,
    pub credential_id: i64,
    pub mother_id: String,
    pub public_id: String,
    pub ksk_id: Option<String>,
    #[serde(skip_serializing)] // 密文只在服务端流转
    pub ksk_cipher: Option<Vec<u8>>,
    pub region: Option<String>,
    pub born_at: String,
    pub warranty_until: String,
    pub status: String,
    pub last_quota: Option<String>,
    pub charged_cents: i64,
    pub died_at: Option<String>,
    pub purge_at: Option<String>,
    pub claim_refunded: bool,
    pub last_probe_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cdk {
    pub code: String,
    pub value_cents: i64,
    pub created_at: String,
    pub created_by: Option<String>,
    pub redeemed_at: Option<String>,
    pub redeemed_by: Option<i64>,
    pub batch: Option<String>,
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletEntry {
    pub id: i64,
    pub customer_id: i64,
    pub delta_cents: i64,
    pub balance_after: i64,
    pub reason: String,
    pub reference: Option<String>,
    pub operator: Option<String>,
    pub created_at: String,
}

/// 交付后返回给上层用于建 ksk 的占号结果
#[derive(Debug, Clone)]
pub struct ReservedAccount {
    pub credential_id: i64,
    pub mother_id: String,
    pub region: Option<String>,
}

// ───────────────────────── Store ─────────────────────────

pub struct WholesaleStore {
    conn: Mutex<Connection>,
}

impl WholesaleStore {
    pub fn open(path: PathBuf) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Self::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS mothers (
              directory_id  TEXT PRIMARY KEY,
              start_url     TEXT NOT NULL,
              region        TEXT,
              added_at      TEXT NOT NULL,
              state         TEXT NOT NULL DEFAULT 'fresh',
              died_at       TEXT,
              child_total   INTEGER NOT NULL DEFAULT 0,
              child_alive   INTEGER NOT NULL DEFAULT 0,
              child_dead    INTEGER NOT NULL DEFAULT 0,
              last_death_at TEXT,
              note          TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_mothers_state ON mothers(state, added_at);

            CREATE TABLE IF NOT EXISTS customers (
              id            INTEGER PRIMARY KEY AUTOINCREMENT,
              uid           TEXT UNIQUE NOT NULL,
              api_key       TEXT UNIQUE NOT NULL,
              username      TEXT UNIQUE NOT NULL,
              password_hash TEXT NOT NULL,
              email         TEXT,
              balance_cents INTEGER NOT NULL DEFAULT 0,
              target        INTEGER NOT NULL DEFAULT 5,
              disabled      INTEGER NOT NULL DEFAULT 0,
              created_at    TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS holdings (
              id             INTEGER PRIMARY KEY AUTOINCREMENT,
              customer_id    INTEGER NOT NULL,
              credential_id  INTEGER NOT NULL,
              mother_id      TEXT NOT NULL,
              public_id      TEXT NOT NULL,
              ksk_id         TEXT,
              ksk_cipher     BLOB,
              region         TEXT,
              born_at        TEXT NOT NULL,
              warranty_until TEXT NOT NULL,
              status         TEXT NOT NULL DEFAULT 'active',
              last_quota     TEXT,
              charged_cents  INTEGER NOT NULL DEFAULT 0,
              died_at        TEXT,
              purge_at       TEXT,
              claim_refunded INTEGER NOT NULL DEFAULT 0,
              last_probe_at  TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_holdings_cust ON holdings(customer_id, status);
            CREATE INDEX IF NOT EXISTS idx_holdings_warranty ON holdings(status, warranty_until);
            CREATE INDEX IF NOT EXISTS idx_holdings_cred ON holdings(credential_id);

            CREATE TABLE IF NOT EXISTS cdks (
              code         TEXT PRIMARY KEY,
              value_cents  INTEGER NOT NULL,
              created_at   TEXT NOT NULL,
              created_by   TEXT,
              redeemed_at  TEXT,
              redeemed_by  INTEGER,
              batch        TEXT,
              disabled     INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_cdks_redeemed ON cdks(redeemed_at);

            CREATE TABLE IF NOT EXISTS wallet_entries (
              id            INTEGER PRIMARY KEY AUTOINCREMENT,
              customer_id   INTEGER NOT NULL,
              delta_cents   INTEGER NOT NULL,
              balance_after INTEGER NOT NULL,
              reason        TEXT NOT NULL,
              reference     TEXT,
              operator      TEXT,
              created_at    TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_wallet_cust ON wallet_entries(customer_id, id);

            -- 销售池：标记 credentials.json 里哪些号可售 + 当前销售状态
            CREATE TABLE IF NOT EXISTS sale_accounts (
              credential_id INTEGER PRIMARY KEY,
              mother_id     TEXT NOT NULL,
              region        TEXT,
              public_id     TEXT NOT NULL,
              sale_state    TEXT NOT NULL DEFAULT 'available',  -- available/reserved/delivered/dead/disabled
              added_at      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sale_state ON sale_accounts(sale_state, mother_id);
            "#,
        )?;
        Ok(())
    }

    // ───────── 客户 ─────────

    /// 注册新客户（uid / api_key 由调用方生成，保证唯一）。
    /// username 冲突返回 Err。
    pub fn create_customer(
        &self,
        uid: &str,
        api_key: &str,
        username: &str,
        password_hash: &str,
        email: Option<&str>,
    ) -> rusqlite::Result<Customer> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO customers (uid, api_key, username, password_hash, email, balance_cents, target, disabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, 0, ?7)",
            params![uid, api_key, username, password_hash, email, DEFAULT_TARGET, now],
        )?;
        let id = conn.last_insert_rowid();
        Ok(Customer {
            id,
            uid: uid.to_string(),
            api_key: api_key.to_string(),
            username: username.to_string(),
            password_hash: password_hash.to_string(),
            email: email.map(|s| s.to_string()),
            balance_cents: 0,
            target: DEFAULT_TARGET,
            disabled: false,
            created_at: now,
        })
    }

    pub fn customer_by_api_key(&self, api_key: &str) -> rusqlite::Result<Option<Customer>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, uid, api_key, username, password_hash, email, balance_cents, target, disabled, created_at
             FROM customers WHERE api_key = ?1",
            params![api_key],
            Self::row_to_customer,
        )
        .optional()
    }

    pub fn customer_by_username(&self, username: &str) -> rusqlite::Result<Option<Customer>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, uid, api_key, username, password_hash, email, balance_cents, target, disabled, created_at
             FROM customers WHERE username = ?1",
            params![username],
            Self::row_to_customer,
        )
        .optional()
    }

    pub fn customer_by_id(&self, id: i64) -> rusqlite::Result<Option<Customer>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, uid, api_key, username, password_hash, email, balance_cents, target, disabled, created_at
             FROM customers WHERE id = ?1",
            params![id],
            Self::row_to_customer,
        )
        .optional()
    }

    pub fn list_customers(&self) -> rusqlite::Result<Vec<Customer>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, uid, api_key, username, password_hash, email, balance_cents, target, disabled, created_at
             FROM customers ORDER BY id",
        )?;
        let rows = stmt.query_map([], Self::row_to_customer)?;
        rows.collect()
    }

    pub fn set_customer_target(&self, id: i64, target: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE customers SET target = ?1 WHERE id = ?2",
            params![target.max(0), id],
        )?;
        Ok(())
    }

    pub fn set_customer_disabled(&self, id: i64, disabled: bool) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE customers SET disabled = ?1 WHERE id = ?2",
            params![disabled as i64, id],
        )?;
        Ok(())
    }

    fn row_to_customer(row: &rusqlite::Row) -> rusqlite::Result<Customer> {
        Ok(Customer {
            id: row.get(0)?,
            uid: row.get(1)?,
            api_key: row.get(2)?,
            username: row.get(3)?,
            password_hash: row.get(4)?,
            email: row.get(5)?,
            balance_cents: row.get(6)?,
            target: row.get(7)?,
            disabled: row.get::<_, i64>(8)? != 0,
            created_at: row.get(9)?,
        })
    }

    // ───────── 钱包账本（资金安全核心，全部事务内） ─────────

    /// 原子调整余额 + 写一条账本。`delta_cents` 正为加(充值/退款)、负为扣(购买)。
    /// 若结果余额为负则回滚并返回 Err（服务端硬拒负余额）。
    /// 返回调整后余额。
    pub fn apply_wallet_delta(
        &self,
        customer_id: i64,
        delta_cents: i64,
        reason: &str,
        reference: Option<&str>,
        operator: Option<&str>,
    ) -> rusqlite::Result<i64> {
        let now = Utc::now().to_rfc3339();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let current: i64 = tx.query_row(
            "SELECT balance_cents FROM customers WHERE id = ?1",
            params![customer_id],
            |r| r.get(0),
        )?;
        let after = current + delta_cents;
        if after < 0 {
            tx.rollback()?;
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                Some(format!("余额不足: 当前 {} 变动 {}", current, delta_cents)),
            ));
        }
        tx.execute(
            "UPDATE customers SET balance_cents = ?1 WHERE id = ?2",
            params![after, customer_id],
        )?;
        tx.execute(
            "INSERT INTO wallet_entries (customer_id, delta_cents, balance_after, reason, reference, operator, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![customer_id, delta_cents, after, reason, reference, operator, now],
        )?;
        tx.commit()?;
        Ok(after)
    }

    pub fn customer_balance(&self, customer_id: i64) -> rusqlite::Result<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT balance_cents FROM customers WHERE id = ?1",
            params![customer_id],
            |r| r.get(0),
        )
    }

    pub fn list_wallet_entries(&self, customer_id: i64, limit: i64) -> rusqlite::Result<Vec<WalletEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, customer_id, delta_cents, balance_after, reason, reference, operator, created_at
             FROM wallet_entries WHERE customer_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![customer_id, limit], |row| {
            Ok(WalletEntry {
                id: row.get(0)?,
                customer_id: row.get(1)?,
                delta_cents: row.get(2)?,
                balance_after: row.get(3)?,
                reason: row.get(4)?,
                reference: row.get(5)?,
                operator: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;
        rows.collect()
    }

    // ───────── CDK 卡密 ─────────

    /// 批量生成 CDK（codes 由调用方生成保证唯一）。
    pub fn create_cdks(
        &self,
        codes: &[String],
        value_cents: i64,
        created_by: Option<&str>,
        batch: Option<&str>,
    ) -> rusqlite::Result<usize> {
        let now = Utc::now().to_rfc3339();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut n = 0;
        for code in codes {
            tx.execute(
                "INSERT INTO cdks (code, value_cents, created_at, created_by, batch, disabled)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                params![code, value_cents, now, created_by, batch],
            )?;
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }

    /// 兑换 CDK：事务内校验未用未禁用 → 加余额 + 写账本 + 标记已用。
    /// 返回兑换后的余额；卡密无效/已用返回 Err。
    pub fn redeem_cdk(&self, code: &str, customer_id: i64) -> Result<i64, String> {
        let now = Utc::now().to_rfc3339();
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(|e| e.to_string())?;

        let row: Option<(i64, Option<String>, i64)> = tx
            .query_row(
                "SELECT value_cents, redeemed_at, disabled FROM cdks WHERE code = ?1",
                params![code],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;

        let (value_cents, redeemed_at, disabled) = match row {
            Some(v) => v,
            None => return Err("卡密不存在".to_string()),
        };
        if disabled != 0 {
            return Err("卡密已作废".to_string());
        }
        if redeemed_at.is_some() {
            return Err("卡密已被使用".to_string());
        }

        let current: i64 = tx
            .query_row(
                "SELECT balance_cents FROM customers WHERE id = ?1",
                params![customer_id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let after = current + value_cents;

        tx.execute(
            "UPDATE customers SET balance_cents = ?1 WHERE id = ?2",
            params![after, customer_id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO wallet_entries (customer_id, delta_cents, balance_after, reason, reference, operator, created_at)
             VALUES (?1, ?2, ?3, 'cdk_redeem', ?4, 'system', ?5)",
            params![customer_id, value_cents, after, code, now],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE cdks SET redeemed_at = ?1, redeemed_by = ?2 WHERE code = ?3",
            params![now, customer_id, code],
        )
        .map_err(|e| e.to_string())?;

        tx.commit().map_err(|e| e.to_string())?;
        Ok(after)
    }

    pub fn list_cdks(&self, only_unused: bool, limit: i64) -> rusqlite::Result<Vec<Cdk>> {
        let conn = self.conn.lock();
        let sql = if only_unused {
            "SELECT code, value_cents, created_at, created_by, redeemed_at, redeemed_by, batch, disabled
             FROM cdks WHERE redeemed_at IS NULL AND disabled = 0 ORDER BY created_at DESC LIMIT ?1"
        } else {
            "SELECT code, value_cents, created_at, created_by, redeemed_at, redeemed_by, batch, disabled
             FROM cdks ORDER BY created_at DESC LIMIT ?1"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(Cdk {
                code: row.get(0)?,
                value_cents: row.get(1)?,
                created_at: row.get(2)?,
                created_by: row.get(3)?,
                redeemed_at: row.get(4)?,
                redeemed_by: row.get(5)?,
                batch: row.get(6)?,
                disabled: row.get::<_, i64>(7)? != 0,
            })
        })?;
        rows.collect()
    }

    pub fn disable_cdk(&self, code: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute("UPDATE cdks SET disabled = 1 WHERE code = ?1", params![code])?;
        Ok(())
    }

    // ───────── 母号 ─────────

    /// upsert 母号：入库子号时自动建档（新母号默认 fresh）。
    pub fn upsert_mother(&self, directory_id: &str, start_url: &str, region: Option<&str>) -> rusqlite::Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO mothers (directory_id, start_url, region, added_at, state)
             VALUES (?1, ?2, ?3, ?4, 'fresh')
             ON CONFLICT(directory_id) DO UPDATE SET start_url = excluded.start_url,
                region = COALESCE(excluded.region, mothers.region)",
            params![directory_id, start_url, region, now],
        )?;
        Ok(())
    }

    pub fn list_mothers(&self) -> rusqlite::Result<Vec<Mother>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT directory_id, start_url, region, added_at, state, died_at, child_total, child_alive, child_dead, last_death_at, note
             FROM mothers ORDER BY added_at DESC",
        )?;
        let rows = stmt.query_map([], Self::row_to_mother)?;
        rows.collect()
    }

    pub fn set_mother_state(&self, directory_id: &str, state: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        let died = if state == mother_state::DEAD {
            Some(Utc::now().to_rfc3339())
        } else {
            None
        };
        conn.execute(
            "UPDATE mothers SET state = ?1, died_at = COALESCE(?2, died_at) WHERE directory_id = ?3",
            params![state, died, directory_id],
        )?;
        Ok(())
    }

    /// 记一次子号死亡：母号 child_dead+1、last_death_at 更新。返回该母号近 1h 死亡数。
    pub fn record_child_death(&self, directory_id: &str) -> rusqlite::Result<i64> {
        let now = Utc::now();
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE mothers SET child_dead = child_dead + 1, last_death_at = ?1 WHERE directory_id = ?2",
            params![now.to_rfc3339(), directory_id],
        )?;
        // 近 1h 该母号名下判死的 holdings 数（跨客户）
        let since = (now - chrono::Duration::hours(1)).to_rfc3339();
        conn.query_row(
            "SELECT COUNT(*) FROM holdings WHERE mother_id = ?1 AND status = 'dead' AND died_at >= ?2",
            params![directory_id, since],
            |r| r.get(0),
        )
    }

    /// 若某母号近 1h 死亡数达到阈值,则置为 dead。返回是否已置 dead。
    pub fn set_mother_state_if_dying(&self, directory_id: &str, threshold_1h: i64) -> rusqlite::Result<bool> {
        let since = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let conn = self.conn.lock();
        let deaths: i64 = conn.query_row(
            "SELECT COUNT(*) FROM holdings WHERE mother_id = ?1 AND status = 'dead' AND died_at >= ?2",
            params![directory_id, since],
            |r| r.get(0),
        )?;
        if deaths >= threshold_1h {
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE mothers SET state = 'dead', died_at = COALESCE(died_at, ?1) WHERE directory_id = ?2",
                params![now, directory_id],
            )?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn row_to_mother(row: &rusqlite::Row) -> rusqlite::Result<Mother> {
        Ok(Mother {
            directory_id: row.get(0)?,
            start_url: row.get(1)?,
            region: row.get(2)?,
            added_at: row.get(3)?,
            state: row.get(4)?,
            died_at: row.get(5)?,
            child_total: row.get(6)?,
            child_alive: row.get(7)?,
            child_dead: row.get(8)?,
            last_death_at: row.get(9)?,
            note: row.get(10)?,
        })
    }

    // ───────── 销售池 ─────────

    /// 入库一个可售子号（测活通过后调用）。public_id 由调用方生成。
    pub fn add_sale_account(
        &self,
        credential_id: i64,
        mother_id: &str,
        region: Option<&str>,
        public_id: &str,
    ) -> rusqlite::Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sale_accounts (credential_id, mother_id, region, public_id, sale_state, added_at)
             VALUES (?1, ?2, ?3, ?4, 'available', ?5)
             ON CONFLICT(credential_id) DO UPDATE SET sale_state = 'available'",
            params![credential_id, mother_id, region, public_id, now],
        )?;
        Ok(())
    }

    /// 各母号可售余量（sale_state='available'），仅统计新鲜/活跃母号。
    pub fn available_by_mother(&self) -> rusqlite::Result<Vec<(String, String, Option<String>, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT m.directory_id, m.state, m.region, COUNT(s.credential_id)
             FROM mothers m
             LEFT JOIN sale_accounts s ON s.mother_id = m.directory_id AND s.sale_state = 'available'
             WHERE m.state IN ('fresh','active')
             GROUP BY m.directory_id
             HAVING COUNT(s.credential_id) > 0
             ORDER BY m.added_at DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect()
    }

    /// **原子占号**（防一号双发）：从可售池挑一个，排除指定母号、限定新鲜度、
    /// 可选限定 region，事务内置为 reserved 并返回。挑不到返回 Ok(None)。
    ///
    /// 母号新鲜度优先(fresh>active),同鲜度按母号 added_at 新的优先(下个新鲜母号)。
    /// `exclude_mothers` 用于质保补号避开死号母号。
    pub fn reserve_account(
        &self,
        region: Option<&str>,
        exclude_mothers: &[String],
        allow_aging: bool,
    ) -> rusqlite::Result<Option<ReservedAccount>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;

        // 动态构建排除条件
        let states: &[&str] = if allow_aging {
            &["fresh", "active", "aging"]
        } else {
            &["fresh", "active"]
        };
        let state_ph = states.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let excl_ph = exclude_mothers.iter().map(|_| "?").collect::<Vec<_>>().join(",");

        let mut sql = format!(
            "SELECT s.credential_id, s.mother_id, s.region
             FROM sale_accounts s JOIN mothers m ON m.directory_id = s.mother_id
             WHERE s.sale_state = 'available' AND m.state IN ({state_ph})"
        );
        if region.is_some() {
            sql.push_str(" AND (s.region = ? OR s.region IS NULL)");
        }
        if !exclude_mothers.is_empty() {
            sql.push_str(&format!(" AND s.mother_id NOT IN ({excl_ph})"));
        }
        // fresh 优先，其次母号越新越优先，最后 credential_id 稳定排序
        sql.push_str(
            " ORDER BY CASE m.state WHEN 'fresh' THEN 0 WHEN 'active' THEN 1 ELSE 2 END, m.added_at DESC, s.credential_id
             LIMIT 1",
        );

        // 组装参数：states... [region] [excludes...]
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for s in states {
            binds.push(Box::new(s.to_string()));
        }
        if let Some(r) = region {
            binds.push(Box::new(r.to_string()));
        }
        for m in exclude_mothers {
            binds.push(Box::new(m.clone()));
        }
        let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();

        let picked: Option<ReservedAccount> = tx
            .query_row(&sql, bind_refs.as_slice(), |row| {
                Ok(ReservedAccount {
                    credential_id: row.get(0)?,
                    mother_id: row.get(1)?,
                    region: row.get(2)?,
                })
            })
            .optional()?;

        if let Some(acc) = &picked {
            tx.execute(
                "UPDATE sale_accounts SET sale_state = 'reserved' WHERE credential_id = ?1",
                params![acc.credential_id],
            )?;
        }
        tx.commit()?;
        Ok(picked)
    }

    /// 释放占用（验活失败 / 建 ksk 失败时回滚为可售）。
    pub fn release_account(&self, credential_id: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sale_accounts SET sale_state = 'available' WHERE credential_id = ?1 AND sale_state = 'reserved'",
            params![credential_id],
        )?;
        Ok(())
    }

    pub fn public_id_for(&self, credential_id: i64) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT public_id FROM sale_accounts WHERE credential_id = ?1",
            params![credential_id],
            |r| r.get(0),
        )
        .optional()
    }

    fn set_sale_state(&self, conn: &Connection, credential_id: i64, state: &str) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE sale_accounts SET sale_state = ?1 WHERE credential_id = ?2",
            params![state, credential_id],
        )?;
        Ok(())
    }

    // ───────── 持有 / 交付 ─────────

    /// 交付成功：**事务内**扣费 + 写账本 + 写 holdings + 号置 delivered。
    /// 余额不足则回滚返回 Err(String)。返回新建的 holding。
    #[allow(clippy::too_many_arguments)]
    pub fn deliver_holding(
        &self,
        customer_id: i64,
        credential_id: i64,
        mother_id: &str,
        public_id: &str,
        ksk_id: Option<&str>,
        ksk_cipher: Option<&[u8]>,
        region: Option<&str>,
        price_cents: i64,
        warranty_secs: i64,
    ) -> Result<Holding, String> {
        let now = Utc::now();
        let born_at = now.to_rfc3339();
        let warranty_until = (now + chrono::Duration::seconds(warranty_secs)).to_rfc3339();

        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(|e| e.to_string())?;

        let current: i64 = tx
            .query_row("SELECT balance_cents FROM customers WHERE id = ?1", params![customer_id], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if current < price_cents {
            tx.rollback().ok();
            return Err("insufficient_balance".to_string());
        }
        let after = current - price_cents;
        tx.execute("UPDATE customers SET balance_cents = ?1 WHERE id = ?2", params![after, customer_id])
            .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO wallet_entries (customer_id, delta_cents, balance_after, reason, reference, operator, created_at)
             VALUES (?1, ?2, ?3, 'acquire', ?4, 'system', ?5)",
            params![customer_id, -price_cents, after, public_id, born_at],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO holdings (customer_id, credential_id, mother_id, public_id, ksk_id, ksk_cipher, region, born_at, warranty_until, status, charged_cents, claim_refunded)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', ?10, 0)",
            params![customer_id, credential_id, mother_id, public_id, ksk_id, ksk_cipher, region, born_at, warranty_until, price_cents],
        )
        .map_err(|e| e.to_string())?;
        let hid = tx.last_insert_rowid();
        tx.execute(
            "UPDATE sale_accounts SET sale_state = 'delivered' WHERE credential_id = ?1",
            params![credential_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;

        Ok(Holding {
            id: hid,
            customer_id,
            credential_id,
            mother_id: mother_id.to_string(),
            public_id: public_id.to_string(),
            ksk_id: ksk_id.map(|s| s.to_string()),
            ksk_cipher: ksk_cipher.map(|b| b.to_vec()),
            region: region.map(|s| s.to_string()),
            born_at,
            warranty_until,
            status: holding_status::ACTIVE.to_string(),
            last_quota: None,
            charged_cents: price_cents,
            died_at: None,
            purge_at: None,
            claim_refunded: false,
            last_probe_at: None,
        })
    }

    /// 客户当前号池：默认排除已过墓碑期(purge_at 已到)的死号。
    pub fn holdings_for_customer(&self, customer_id: i64, include_purged: bool) -> rusqlite::Result<Vec<Holding>> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        let sql = if include_purged {
            "SELECT id, customer_id, credential_id, mother_id, public_id, ksk_id, ksk_cipher, region, born_at, warranty_until, status, last_quota, charged_cents, died_at, purge_at, claim_refunded, last_probe_at
             FROM holdings WHERE customer_id = ?1 ORDER BY id DESC".to_string()
        } else {
            // 活号 + 尚在墓碑期(purge_at 未到或为空)的死号
            format!(
                "SELECT id, customer_id, credential_id, mother_id, public_id, ksk_id, ksk_cipher, region, born_at, warranty_until, status, last_quota, charged_cents, died_at, purge_at, claim_refunded, last_probe_at
                 FROM holdings WHERE customer_id = ?1 AND (status != 'dead' OR purge_at IS NULL OR purge_at > '{now}') ORDER BY id DESC"
            )
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![customer_id], Self::row_to_holding)?;
        rows.collect()
    }

    /// 客户名下"正常号"数量（status = active，用于 sync 判断 need）。
    pub fn active_count(&self, customer_id: i64) -> rusqlite::Result<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM holdings WHERE customer_id = ?1 AND status = 'active'",
            params![customer_id],
            |r| r.get(0),
        )
    }

    /// 所有仍需探活的 holdings：活着(active/low_quota)且未过保 → 探封号+额度;
    /// 也含过保但仍活的(继续更新界面状态但不再退款)。返回 (holding_id, credential_id, mother_id, customer_id, warranty_until, status)。
    pub fn holdings_to_probe(&self) -> rusqlite::Result<Vec<(i64, i64, String, i64, String, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, credential_id, mother_id, customer_id, warranty_until, status
             FROM holdings WHERE status IN ('active','low_quota') ORDER BY last_probe_at IS NULL DESC, last_probe_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?;
        rows.collect()
    }

    /// 更新探活结果:status + last_quota + last_probe_at(号还活着时用)。
    pub fn update_holding_probe(&self, holding_id: i64, status: &str, last_quota: Option<&str>) -> rusqlite::Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE holdings SET status = ?1, last_quota = COALESCE(?2, last_quota), last_probe_at = ?3 WHERE id = ?4",
            params![status, last_quota, now, holding_id],
        )?;
        Ok(())
    }

    /// 判死一个 holding：置 dead + died_at + purge_at(+5min) + last_probe_at，号置 dead。
    /// 若在保且未退过 → 事务内退余额 + 写账本 + claim_refunded=1。返回是否发生退款。
    pub fn mark_holding_dead(&self, holding_id: i64) -> Result<bool, String> {
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let purge_at = (now + chrono::Duration::seconds(PURGE_AFTER_SECS)).to_rfc3339();

        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(|e| e.to_string())?;

        let row: Option<(i64, i64, i64, String, i64, String)> = tx
            .query_row(
                "SELECT customer_id, credential_id, charged_cents, warranty_until, claim_refunded, public_id
                 FROM holdings WHERE id = ?1 AND status != 'dead'",
                params![holding_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;

        let (customer_id, credential_id, charged, warranty_until, claim_refunded, public_id) = match row {
            Some(v) => v,
            None => {
                tx.rollback().ok();
                return Ok(false); // 已是 dead 或不存在
            }
        };

        tx.execute(
            "UPDATE holdings SET status = 'dead', died_at = ?1, purge_at = ?2, last_probe_at = ?1 WHERE id = ?3",
            params![now_s, purge_at, holding_id],
        )
        .map_err(|e| e.to_string())?;
        // 号从销售池移除(dead)
        tx.execute(
            "UPDATE sale_accounts SET sale_state = 'dead' WHERE credential_id = ?1",
            params![credential_id],
        )
        .map_err(|e| e.to_string())?;

        // 在保 + 未退过 → 退款
        let in_warranty = now_s.as_str() <= warranty_until.as_str();
        let mut refunded = false;
        if in_warranty && claim_refunded == 0 {
            let current: i64 = tx
                .query_row("SELECT balance_cents FROM customers WHERE id = ?1", params![customer_id], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            let after = current + charged;
            tx.execute("UPDATE customers SET balance_cents = ?1 WHERE id = ?2", params![after, customer_id])
                .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT INTO wallet_entries (customer_id, delta_cents, balance_after, reason, reference, operator, created_at)
                 VALUES (?1, ?2, ?3, 'warranty_refund', ?4, 'system', ?5)",
                params![customer_id, charged, after, public_id, now_s],
            )
            .map_err(|e| e.to_string())?;
            tx.execute("UPDATE holdings SET claim_refunded = 1 WHERE id = ?1", params![holding_id])
                .map_err(|e| e.to_string())?;
            refunded = true;
        }

        tx.commit().map_err(|e| e.to_string())?;
        Ok(refunded)
    }

    fn row_to_holding(row: &rusqlite::Row) -> rusqlite::Result<Holding> {
        Ok(Holding {
            id: row.get(0)?,
            customer_id: row.get(1)?,
            credential_id: row.get(2)?,
            mother_id: row.get(3)?,
            public_id: row.get(4)?,
            ksk_id: row.get(5)?,
            ksk_cipher: row.get(6)?,
            region: row.get(7)?,
            born_at: row.get(8)?,
            warranty_until: row.get(9)?,
            status: row.get(10)?,
            last_quota: row.get(11)?,
            charged_cents: row.get(12)?,
            died_at: row.get(13)?,
            purge_at: row.get(14)?,
            claim_refunded: row.get::<_, i64>(15)? != 0,
            last_probe_at: row.get(16)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> WholesaleStore {
        WholesaleStore::open_in_memory().unwrap()
    }

    fn seed_pool(s: &WholesaleStore, mother: &str, region: &str, n: i64) {
        s.upsert_mother(mother, &format!("https://{mother}.awsapps.com/start"), Some(region)).unwrap();
        for i in 0..n {
            let cid = 1000 + i + (mother.bytes().map(|b| b as i64).sum::<i64>() * 100);
            s.add_sale_account(cid, mother, Some(region), &format!("{mother}-{i:04}")).unwrap();
        }
    }

    #[test]
    fn register_and_lookup_customer() {
        let s = store();
        let c = s.create_customer("u_1", "wsk_abc", "alice", "hash", Some("a@b.com")).unwrap();
        assert_eq!(c.balance_cents, 0);
        assert_eq!(c.target, DEFAULT_TARGET);
        assert_eq!(s.customer_by_api_key("wsk_abc").unwrap().unwrap().uid, "u_1");
        assert_eq!(s.customer_by_username("alice").unwrap().unwrap().id, c.id);
        // 重名拒绝
        assert!(s.create_customer("u_2", "wsk_xyz", "alice", "h", None).is_err());
    }

    #[test]
    fn cdk_redeem_adds_balance_once() {
        let s = store();
        let c = s.create_customer("u_1", "wsk_a", "alice", "h", None).unwrap();
        s.create_cdks(&["CDK-1".into()], 5000, Some("admin"), Some("b1")).unwrap();
        let bal = s.redeem_cdk("CDK-1", c.id).unwrap();
        assert_eq!(bal, 5000);
        // 二次兑换被拒
        assert!(s.redeem_cdk("CDK-1", c.id).is_err());
        // 不存在
        assert!(s.redeem_cdk("CDK-NOPE", c.id).is_err());
    }

    #[test]
    fn wallet_rejects_negative() {
        let s = store();
        let c = s.create_customer("u_1", "wsk_a", "alice", "h", None).unwrap();
        assert!(s.apply_wallet_delta(c.id, -100, "acquire", None, None).is_err());
        s.apply_wallet_delta(c.id, 1000, "admin_adjust", None, Some("admin")).unwrap();
        assert_eq!(s.customer_balance(c.id).unwrap(), 1000);
    }

    #[test]
    fn reserve_prefers_fresh_then_newest_mother_and_excludes() {
        let s = store();
        seed_pool(&s, "d-aaaaaaaaaa", "us-east-1", 2);
        // 让第二个母号更新(added_at 更晚)
        std::thread::sleep(std::time::Duration::from_millis(5));
        seed_pool(&s, "d-bbbbbbbbbb", "us-east-1", 2);

        // 都是 fresh,应挑更新的 d-bbb
        let r = s.reserve_account(Some("us-east-1"), &[], false).unwrap().unwrap();
        assert_eq!(r.mother_id, "d-bbbbbbbbbb");

        // 排除 d-bbb → 只能挑 d-aaa（补号换母号场景）
        let r2 = s
            .reserve_account(Some("us-east-1"), &["d-bbbbbbbbbb".to_string()], false)
            .unwrap()
            .unwrap();
        assert_eq!(r2.mother_id, "d-aaaaaaaaaa");
    }

    #[test]
    fn reserved_account_not_double_delivered() {
        let s = store();
        seed_pool(&s, "d-aaaaaaaaaa", "us-east-1", 1);
        let r1 = s.reserve_account(None, &[], false).unwrap();
        assert!(r1.is_some());
        // 池里只有 1 个,第二次占号应为空(已被 reserve)
        let r2 = s.reserve_account(None, &[], false).unwrap();
        assert!(r2.is_none());
    }

    #[test]
    fn deliver_charges_and_creates_holding() {
        let s = store();
        let c = s.create_customer("u_1", "wsk_a", "alice", "h", None).unwrap();
        s.apply_wallet_delta(c.id, 2000, "cdk_redeem", None, None).unwrap();
        seed_pool(&s, "d-aaaaaaaaaa", "us-east-1", 3);
        let acc = s.reserve_account(None, &[], false).unwrap().unwrap();

        let h = s
            .deliver_holding(c.id, acc.credential_id, &acc.mother_id, "d-aaaaaaaaaa-0000",
                Some("kskid_1"), Some(b"cipher"), Some("us-east-1"), DEFAULT_PRICE_CENTS, DEFAULT_WARRANTY_SECS)
            .unwrap();
        assert_eq!(h.status, "active");
        assert_eq!(s.customer_balance(c.id).unwrap(), 2000 - DEFAULT_PRICE_CENTS);
        assert_eq!(s.active_count(c.id).unwrap(), 1);
    }

    #[test]
    fn deliver_insufficient_balance_rolls_back() {
        let s = store();
        let c = s.create_customer("u_1", "wsk_a", "alice", "h", None).unwrap();
        seed_pool(&s, "d-aaaaaaaaaa", "us-east-1", 1);
        let acc = s.reserve_account(None, &[], false).unwrap().unwrap();
        let err = s
            .deliver_holding(c.id, acc.credential_id, &acc.mother_id, "p", None, None, None,
                DEFAULT_PRICE_CENTS, DEFAULT_WARRANTY_SECS)
            .unwrap_err();
        assert_eq!(err, "insufficient_balance");
        assert_eq!(s.active_count(c.id).unwrap(), 0);
    }

    #[test]
    fn mark_dead_in_warranty_refunds_once() {
        let s = store();
        let c = s.create_customer("u_1", "wsk_a", "alice", "h", None).unwrap();
        s.apply_wallet_delta(c.id, 2000, "cdk_redeem", None, None).unwrap();
        seed_pool(&s, "d-aaaaaaaaaa", "us-east-1", 1);
        let acc = s.reserve_account(None, &[], false).unwrap().unwrap();
        let h = s
            .deliver_holding(c.id, acc.credential_id, &acc.mother_id, "p", None, None, None,
                DEFAULT_PRICE_CENTS, DEFAULT_WARRANTY_SECS)
            .unwrap();
        let bal_after_buy = s.customer_balance(c.id).unwrap();

        let refunded = s.mark_holding_dead(h.id).unwrap();
        assert!(refunded);
        assert_eq!(s.customer_balance(c.id).unwrap(), bal_after_buy + DEFAULT_PRICE_CENTS);
        // 再次判死不重复退(已是 dead)
        assert!(!s.mark_holding_dead(h.id).unwrap());
    }

    #[test]
    fn mark_dead_out_of_warranty_no_refund() {
        let s = store();
        let c = s.create_customer("u_1", "wsk_a", "alice", "h", None).unwrap();
        s.apply_wallet_delta(c.id, 2000, "cdk_redeem", None, None).unwrap();
        seed_pool(&s, "d-aaaaaaaaaa", "us-east-1", 1);
        let acc = s.reserve_account(None, &[], false).unwrap().unwrap();
        // 质保 0 秒 → 立刻过保
        let h = s
            .deliver_holding(c.id, acc.credential_id, &acc.mother_id, "p", None, None, None, DEFAULT_PRICE_CENTS, 0)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let bal = s.customer_balance(c.id).unwrap();
        let refunded = s.mark_holding_dead(h.id).unwrap();
        assert!(!refunded);
        assert_eq!(s.customer_balance(c.id).unwrap(), bal); // 不退
    }

    #[test]
    fn dead_holding_hidden_after_purge() {
        let s = store();
        let c = s.create_customer("u_1", "wsk_a", "alice", "h", None).unwrap();
        s.apply_wallet_delta(c.id, 2000, "cdk_redeem", None, None).unwrap();
        seed_pool(&s, "d-aaaaaaaaaa", "us-east-1", 1);
        let acc = s.reserve_account(None, &[], false).unwrap().unwrap();
        let h = s
            .deliver_holding(c.id, acc.credential_id, &acc.mother_id, "p", None, None, None, DEFAULT_PRICE_CENTS, DEFAULT_WARRANTY_SECS)
            .unwrap();
        s.mark_holding_dead(h.id).unwrap();
        // 刚死,墓碑期内 → 仍显示
        let visible = s.holdings_for_customer(c.id, false).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].status, "dead");
        // include_purged 永远能看到
        assert_eq!(s.holdings_for_customer(c.id, true).unwrap().len(), 1);
    }
}
