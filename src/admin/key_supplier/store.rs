use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, types::Type};

const MAX_MESSAGE_CHARS: usize = 2000;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS supplier_events (
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
"#;

const INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_supplier_events_queue ON supplier_events(status, id);
CREATE INDEX IF NOT EXISTS idx_supplier_events_read ON supplier_events(read_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_supplier_events_event_id_unique ON supplier_events(event_id);
"#;

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
    pub event_id: String,
    pub event_type: String,
    pub purchase_order_id: Option<String>,
    pub message: Option<String>,
    pub quantity: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSupplierEvent {
    pub id: i64,
    pub event_id: String,
    pub event_type: String,
    pub purchase_order_id: Option<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSummary {
    pub purchased_count: i64,
    pub imported_count: i64,
    pub duplicate_count: i64,
    pub failed_count: i64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupplierEventPage {
    pub items: Vec<StoredSupplierEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

    pub fn insert_event(&self, event: IncomingSupplierEvent) -> rusqlite::Result<InsertOutcome> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let received_at = Utc::now().to_rfc3339();
        let message = event
            .message
            .map(|value| truncate_chars(&value, MAX_MESSAGE_CHARS));
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO supplier_events
             (event_id,event_type,purchase_order_id,message,quantity,received_at,status)
             VALUES (?1,?2,?3,?4,?5,?6,'received')",
            params![
                event.event_id,
                event.event_type,
                event.purchase_order_id,
                message,
                event.quantity,
                received_at
            ],
        )?;
        if inserted == 0 {
            tx.execute(
                "UPDATE supplier_events SET webhook_duplicate_count=webhook_duplicate_count+1 WHERE event_id=?1",
                params![event.event_id],
            )?;
        }
        tx.commit()?;
        let stored = Self::query_by_event_id(&conn, &event.event_id)?
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
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM supplier_events WHERE status='received' ORDER BY id ASC LIMIT 1",
                [],
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
        event_id: &str,
    ) -> rusqlite::Result<Option<StoredSupplierEvent>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM supplier_events WHERE event_id=?1 AND status='received'",
                params![event_id],
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
        self.transition_processing(
            id,
            "skipped",
            ProcessSummary {
                purchased_count: 0,
                imported_count: 0,
                duplicate_count: 0,
                failed_count: 0,
                message: message.map(|value| truncate_chars(value, MAX_MESSAGE_CHARS)),
            },
        )
    }

    pub fn fail(&self, id: i64, error: &str) -> rusqlite::Result<()> {
        self.transition_processing(
            id,
            "failed",
            ProcessSummary {
                purchased_count: 0,
                imported_count: 0,
                duplicate_count: 0,
                failed_count: 1,
                message: Some(truncate_chars(error, 300)),
            },
        )
    }

    pub fn retry(&self, id: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE supplier_events SET status='received', processing_started_at=NULL, last_error=NULL
             WHERE id=?1 AND status IN ('failed','skipped')",
            params![id],
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

    pub fn list(&self, limit: usize, before: Option<i64>) -> rusqlite::Result<SupplierEventPage> {
        let conn = self.conn.lock().unwrap();
        let limit = limit.clamp(1, 200) as i64;
        let mut stmt = conn.prepare(
            "SELECT id,event_id,event_type,purchase_order_id,message,quantity,received_at,status,attempts,last_error,
                    purchased,imported,duplicate_count,webhook_duplicate_count,failed_count,read_at,processing_started_at
             FROM supplier_events WHERE (?1 IS NULL OR id < ?1) ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![before, limit], Self::row_to_event)?;
        Ok(SupplierEventPage {
            items: rows.collect::<rusqlite::Result<_>>()?,
        })
    }

    pub fn unread_count(&self) -> rusqlite::Result<i64> {
        self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM supplier_events WHERE read_at IS NULL",
            [],
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

    pub fn mark_all_read(&self) -> rusqlite::Result<usize> {
        self.conn.lock().unwrap().execute(
            "UPDATE supplier_events SET read_at=?1 WHERE read_at IS NULL",
            params![Utc::now().to_rfc3339()],
        )
    }

    fn transition_processing(
        &self,
        id: i64,
        status: &str,
        summary: ProcessSummary,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE supplier_events SET status=?1, message=COALESCE(?2,message), purchased=?3, imported=?4,
             duplicate_count=?5, failed_count=?6,
             last_error=CASE WHEN ?1='failed' THEN ?2 ELSE last_error END, processing_started_at=NULL
             WHERE id=?7 AND status='processing'",
            params![
                status,
                summary.message,
                summary.purchased_count,
                summary.imported_count,
                summary.duplicate_count,
                summary.failed_count,
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
        event_id: &str,
    ) -> rusqlite::Result<Option<StoredSupplierEvent>> {
        conn.query_row("SELECT id,event_id,event_type,purchase_order_id,message,quantity,received_at,status,attempts,last_error,purchased,imported,duplicate_count,webhook_duplicate_count,failed_count,read_at,processing_started_at FROM supplier_events WHERE event_id=?1", params![event_id], Self::row_to_event).optional()
    }

    fn query_by_id(
        conn: &rusqlite::Transaction<'_>,
        id: i64,
    ) -> rusqlite::Result<Option<StoredSupplierEvent>> {
        conn.query_row("SELECT id,event_id,event_type,purchase_order_id,message,quantity,received_at,status,attempts,last_error,purchased,imported,duplicate_count,webhook_duplicate_count,failed_count,read_at,processing_started_at FROM supplier_events WHERE id=?1", params![id], Self::row_to_event).optional()
    }

    fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSupplierEvent> {
        Ok(StoredSupplierEvent {
            id: row.get(0)?,
            event_id: row.get(1)?,
            event_type: row.get(2)?,
            purchase_order_id: row.get(3)?,
            message: row.get(4)?,
            quantity: row.get(5)?,
            received_at: row.get(6)?,
            status: SupplierEventStatus::from_db(&row.get::<_, String>(7)?, 7)?,
            attempts: row.get(8)?,
            last_error: row.get(9)?,
            purchased_count: row.get(10)?,
            imported_count: row.get(11)?,
            duplicate_count: row.get(12)?,
            webhook_duplicate_count: row.get(13)?,
            failed_count: row.get(14)?,
            read_at: row.get(15)?,
            processing_started_at: row.get(16)?,
        })
    }

    fn claim_in_transaction(
        tx: &rusqlite::Transaction<'_>,
        id: i64,
    ) -> rusqlite::Result<Option<StoredSupplierEvent>> {
        let now = Utc::now().to_rfc3339();
        let changed = tx.execute(
            "UPDATE supplier_events SET status='processing', attempts=attempts+1, processing_started_at=?1 WHERE id=?2 AND status='received'",
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
    tx.execute_batch(INDEXES)?;
    tx.commit()
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
            event_id: id.to_string(),
            event_type: "purchase.requested".to_string(),
            purchase_order_id: Some("po-1".to_string()),
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
        assert_eq!(store.unread_count().unwrap(), 0);
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
        let page = store.list(10, None).unwrap();
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
                    purchased_count: 0,
                    imported_count: 0,
                    duplicate_count: 0,
                    failed_count: 0,
                    message: Some(complete_message),
                },
            )
            .unwrap();
        assert_eq!(
            store.list(1, None).unwrap().items[0]
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
            store.list(1, None).unwrap().items[0]
                .message
                .as_ref()
                .unwrap()
                .chars()
                .count(),
            MAX_MESSAGE_CHARS
        );
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
        let items = store.list(10, None).unwrap().items;
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
                    message: None,
                },
            )
            .unwrap();

        let stored = store.list(1, None).unwrap().items.remove(0);
        assert_eq!(stored.purchased_count, 3);
        assert_eq!(stored.imported_count, 1);
        assert_eq!(stored.duplicate_count, 1);
        assert_eq!(stored.failed_count, 1);
    }

    #[test]
    fn claim_by_event_id_only_claims_received_event() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        store.insert_event(event("a")).unwrap();

        let claimed = store.claim_by_event_id("a").unwrap().unwrap();
        assert_eq!(claimed.status, SupplierEventStatus::Processing);
        assert_eq!(claimed.attempts, 1);
        assert!(store.claim_by_event_id("a").unwrap().is_none());
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
                    duplicate_count: 0,
                    failed_count: 0,
                    message: None,
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
    fn list_paginates_and_tracks_unread() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        for id in ["a", "b", "c"] {
            store.insert_event(event(id)).unwrap();
        }
        let page = store.list(999, None).unwrap();
        assert_eq!(page.items.len(), 3);
        assert_eq!(page.items[0].event_id, "c");
        assert_eq!(
            store.list(2, Some(page.items[0].id)).unwrap().items.len(),
            2
        );
        assert_eq!(store.unread_count().unwrap(), 3);
        store.mark_read(&[page.items[0].id]).unwrap();
        assert_eq!(store.unread_count().unwrap(), 2);
        store.mark_all_read().unwrap();
        assert_eq!(store.unread_count().unwrap(), 0);
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
        assert!(store.list(1, None).is_err());
    }
}
