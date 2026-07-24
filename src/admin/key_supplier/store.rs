use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, types::Type, Connection, OptionalExtension, TransactionBehavior};

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
    failed_count INTEGER NOT NULL DEFAULT 0,
    read_at TEXT,
    processing_started_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_supplier_events_queue ON supplier_events(status, id);
CREATE INDEX IF NOT EXISTS idx_supplier_events_read ON supplier_events(read_at);
"#;

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
    pub purchased: bool,
    pub imported: bool,
    pub duplicate_count: i64,
    pub failed_count: i64,
    pub read_at: Option<String>,
    pub processing_started_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSummary {
    pub purchased: bool,
    pub imported: bool,
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
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert_event(&self, event: IncomingSupplierEvent) -> rusqlite::Result<InsertOutcome> {
        let conn = self.conn.lock().unwrap();
        let received_at = Utc::now().to_rfc3339();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO supplier_events
             (event_id,event_type,purchase_order_id,message,quantity,received_at,status)
             VALUES (?1,?2,?3,?4,?5,?6,'received')",
            params![
                event.event_id,
                event.event_type,
                event.purchase_order_id,
                event.message,
                event.quantity,
                received_at
            ],
        )?;
        let stored = Self::query_by_event_id(&conn, &event.event_id)?
            .expect("inserted or duplicate event must be queryable");
        if inserted == 1 {
            Ok(InsertOutcome::Inserted(stored))
        } else {
            conn.execute(
                "UPDATE supplier_events SET duplicate_count = duplicate_count + 1 WHERE event_id = ?1",
                params![event.event_id],
            )?;
            Ok(InsertOutcome::Duplicate(
                Self::query_by_event_id(&conn, &event.event_id)?.unwrap(),
            ))
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
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE supplier_events SET status='processing', attempts=attempts+1, processing_started_at=?1 WHERE id=?2 AND status='received'",
            params![now, id],
        )?;
        let stored = Self::query_by_id(&tx, id)?.expect("claimed event must exist");
        tx.commit()?;
        Ok(Some(stored))
    }

    pub fn complete(&self, id: i64, summary: ProcessSummary) -> rusqlite::Result<()> {
        self.transition_processing(
            id,
            "succeeded",
            summary.message,
            summary.purchased,
            summary.imported,
        )
    }

    pub fn skip(&self, id: i64, message: Option<&str>) -> rusqlite::Result<()> {
        self.transition_processing(id, "skipped", message.map(str::to_owned), false, false)
    }

    pub fn fail(&self, id: i64, error: &str) -> rusqlite::Result<()> {
        self.transition_processing(id, "failed", Some(truncate_chars(error, 300)), false, false)
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
                    purchased,imported,duplicate_count,failed_count,read_at,processing_started_at
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
        message: Option<String>,
        purchased: bool,
        imported: bool,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE supplier_events SET status=?1, message=COALESCE(?2,message), purchased=?3, imported=?4,
             last_error=CASE WHEN ?1='failed' THEN ?2 ELSE last_error END, processing_started_at=NULL,
             failed_count=CASE WHEN ?1='failed' THEN failed_count+1 ELSE failed_count END
             WHERE id=?5 AND status='processing'",
            params![status, message, purchased, imported, id],
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
        conn.query_row("SELECT id,event_id,event_type,purchase_order_id,message,quantity,received_at,status,attempts,last_error,purchased,imported,duplicate_count,failed_count,read_at,processing_started_at FROM supplier_events WHERE event_id=?1", params![event_id], Self::row_to_event).optional()
    }

    fn query_by_id(
        conn: &rusqlite::Transaction<'_>,
        id: i64,
    ) -> rusqlite::Result<Option<StoredSupplierEvent>> {
        conn.query_row("SELECT id,event_id,event_type,purchase_order_id,message,quantity,received_at,status,attempts,last_error,purchased,imported,duplicate_count,failed_count,read_at,processing_started_at FROM supplier_events WHERE id=?1", params![id], Self::row_to_event).optional()
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
            purchased: row.get(10)?,
            imported: row.get(11)?,
            duplicate_count: row.get(12)?,
            failed_count: row.get(13)?,
            read_at: row.get(14)?,
            processing_started_at: row.get(15)?,
        })
    }
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
        assert_eq!(store.list(10, None).unwrap().items.len(), 1);
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
    fn state_transitions_retry_and_stale_recovery_are_restricted() {
        let store = SupplierEventStore::open_in_memory().unwrap();
        store.insert_event(event("a")).unwrap();
        let claimed = store.claim_next().unwrap().unwrap();
        store
            .complete(
                claimed.id,
                ProcessSummary {
                    purchased: true,
                    imported: true,
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
