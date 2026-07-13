use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, OptionalExtension as _, params, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::common::error_snapshot::{EncodedPayloadPart, SnapshotPayloadKind};

const SCHEMA_VERSION: i64 = 1;
const DEFAULT_QUERY_LIMIT: usize = 50;
const MAX_QUERY_LIMIT: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotSeverity {
    Critical,
    Error,
    Warning,
    Info,
}

impl SnapshotSeverity {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }

    fn from_db(value: &str) -> Result<Self, String> {
        match value {
            "critical" => Ok(Self::Critical),
            "error" => Ok(Self::Error),
            "warning" => Ok(Self::Warning),
            "info" => Ok(Self::Info),
            _ => Err(format!("未知快照严重级别: {value}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ErrorSnapshotPolicy {
    pub enabled: bool,
    pub retention_days: u32,
    pub max_storage_bytes: u64,
    pub capture_recovered: bool,
    pub capture_bodies: bool,
    pub min_free_disk_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotWrite {
    pub snapshot_id: String,
    pub trace_id: String,
    pub ts: String,
    pub ts_epoch: i64,
    pub model: String,
    pub is_stream: bool,
    pub key_id: u64,
    pub key_source: crate::admin::trace_db::TraceKeySource,
    pub final_credential_id: u64,
    pub endpoint: Option<String>,
    pub http_status: Option<u16>,
    pub final_status: String,
    pub error_type: String,
    pub severity: SnapshotSeverity,
    pub error_message: Option<String>,
    pub recovered: bool,
    pub pinned: bool,
    pub retention_exempt: bool,
    pub omitted_due_to_disk_pressure: bool,
    pub payloads: Vec<EncodedPayloadPart>,
}

#[derive(Debug, Default, Clone)]
pub struct SnapshotQuery {
    pub trace_id: Option<String>,
    pub model: Option<String>,
    pub error_type: Option<String>,
    pub http_status: Option<u16>,
    pub credential_id: Option<u64>,
    pub severity: Option<SnapshotSeverity>,
    pub recovered: Option<bool>,
    pub pinned: Option<bool>,
    pub from_epoch: Option<i64>,
    pub to_epoch: Option<i64>,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted(String),
    Existing(String),
    Fallback(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureMode {
    Full,
    MetadataOnly,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FallbackImportReport {
    pub imported: usize,
    pub existing: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceReport {
    pub deleted: usize,
    pub imported: usize,
    pub disk_pressure: bool,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatus {
    pub db_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub fallback_bytes: u64,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub max_storage_bytes: u64,
    pub min_free_disk_bytes: u64,
    pub disk_pressure: bool,
    pub records: u64,
    pub pinned_records: u64,
    pub critical_records: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSummary {
    pub snapshot_id: String,
    pub trace_id: String,
    pub ts: String,
    pub model: String,
    pub is_stream: bool,
    pub key_id: u64,
    pub key_source: crate::admin::trace_db::TraceKeySource,
    pub final_credential_id: u64,
    pub endpoint: Option<String>,
    pub http_status: Option<u16>,
    pub final_status: String,
    pub error_type: String,
    pub severity: SnapshotSeverity,
    pub error_message: Option<String>,
    pub recovered: bool,
    pub pinned: bool,
    pub retention_exempt: bool,
    pub omitted_due_to_disk_pressure: bool,
    pub payload_count: u32,
    pub original_bytes: u64,
    pub compressed_bytes: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPayloadMeta {
    pub seq: u32,
    pub kind: SnapshotPayloadKind,
    pub attempt: Option<u32>,
    pub content_type: String,
    pub original_bytes: u64,
    pub compressed_bytes: u64,
    pub sha256: String,
    pub part_count: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDetail {
    #[serde(flatten)]
    pub summary: SnapshotSummary,
    pub payloads: Vec<SnapshotPayloadMeta>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPage {
    pub records: Vec<SnapshotSummary>,
    pub total: usize,
}

#[derive(Debug, Clone)]
pub struct DecodedPayload {
    pub meta: SnapshotPayloadMeta,
    pub data: Vec<u8>,
}

pub struct ErrorSnapshotStore {
    conn: Mutex<Connection>,
    #[allow(dead_code)]
    db_path: Option<PathBuf>,
    #[allow(dead_code)]
    fallback_dir: Option<PathBuf>,
    policy: RwLock<ErrorSnapshotPolicy>,
    disk_pressure: AtomicBool,
    storage_probe: Arc<dyn StorageProbe>,
}

pub type SharedErrorSnapshotStore = Arc<ErrorSnapshotStore>;

trait StorageProbe: Send + Sync {
    fn available_bytes(&self, path: &std::path::Path) -> std::io::Result<u64>;
    fn tree_bytes(&self, paths: &[PathBuf]) -> std::io::Result<u64>;
}

#[derive(Debug)]
struct RealStorageProbe;

impl StorageProbe for RealStorageProbe {
    fn available_bytes(&self, path: &std::path::Path) -> std::io::Result<u64> {
        fs2::available_space(path)
    }

    fn tree_bytes(&self, paths: &[PathBuf]) -> std::io::Result<u64> {
        paths.iter().try_fold(0u64, |total, path| {
            total
                .checked_add(path_tree_bytes(path)?)
                .ok_or_else(|| std::io::Error::other("快照目录大小溢出"))
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct FallbackEnvelope {
    version: u32,
    snapshot: serde_json::Value,
    payloads: Vec<FallbackPayloadPart>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FallbackPayloadPart {
    seq: u32,
    kind: SnapshotPayloadKind,
    attempt: Option<u32>,
    codec: String,
    content_type: String,
    part_index: u32,
    part_count: u32,
    original_bytes: u64,
    compressed_bytes: u64,
    sha256: String,
    data_b64: String,
}

impl ErrorSnapshotStore {
    pub fn open(
        path: PathBuf,
        fallback_dir: PathBuf,
        policy: ErrorSnapshotPolicy,
    ) -> rusqlite::Result<Self> {
        let is_new = !path.exists();
        let conn = Connection::open(&path)?;
        initialize_connection(&conn, is_new)?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: Some(path),
            fallback_dir: Some(fallback_dir),
            policy: RwLock::new(policy),
            disk_pressure: AtomicBool::new(false),
            storage_probe: Arc::new(RealStorageProbe),
        })
    }

    pub fn open_in_memory(policy: ErrorSnapshotPolicy) -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        initialize_connection(&conn, true)?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: None,
            fallback_dir: None,
            policy: RwLock::new(policy),
            disk_pressure: AtomicBool::new(false),
            storage_probe: Arc::new(RealStorageProbe),
        })
    }

    pub fn open_in_memory_with_fallback(
        fallback_dir: PathBuf,
        policy: ErrorSnapshotPolicy,
    ) -> rusqlite::Result<Self> {
        let mut store = Self::open_in_memory(policy)?;
        store.fallback_dir = Some(fallback_dir);
        Ok(store)
    }

    #[cfg(test)]
    fn open_in_memory_with_probe(
        policy: ErrorSnapshotPolicy,
        storage_probe: Arc<dyn StorageProbe>,
    ) -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        initialize_connection(&conn, true)?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: None,
            fallback_dir: None,
            policy: RwLock::new(policy),
            disk_pressure: AtomicBool::new(false),
            storage_probe,
        })
    }

    pub fn policy(&self) -> ErrorSnapshotPolicy {
        self.policy.read().clone()
    }

    pub fn set_policy(&self, policy: ErrorSnapshotPolicy) {
        *self.policy.write() = policy;
    }

    pub fn insert(&self, write: &SnapshotWrite) -> anyhow::Result<InsertOutcome> {
        let mut conn = self.conn.lock();
        if let Some(existing) = conn
            .query_row(
                "SELECT snapshot_id FROM error_snapshots WHERE trace_id = ?1",
                params![write.trace_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(InsertOutcome::Existing(existing));
        }

        let payload_count = write
            .payloads
            .iter()
            .map(|part| part.seq)
            .collect::<std::collections::HashSet<_>>()
            .len();
        let original_bytes = write.payloads.iter().try_fold(0u64, |total, part| {
            total
                .checked_add(part.original_bytes)
                .ok_or_else(|| anyhow::anyhow!("快照原始长度溢出"))
        })?;
        let compressed_bytes = write.payloads.iter().try_fold(0u64, |total, part| {
            total
                .checked_add(u64::try_from(part.data.len())?)
                .ok_or_else(|| anyhow::anyhow!("快照压缩长度溢出"))
        })?;
        let now = chrono::Utc::now().timestamp();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO error_snapshots (
                snapshot_id, trace_id, ts, ts_epoch, model, is_stream, key_id, key_source,
                final_credential_id, endpoint, http_status, final_status, error_type, severity,
                error_message, recovered, pinned, retention_exempt, omitted_due_to_disk_pressure,
                payload_count, original_bytes, compressed_bytes, created_at, updated_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
             )",
            params![
                write.snapshot_id,
                write.trace_id,
                write.ts,
                write.ts_epoch,
                write.model,
                write.is_stream,
                to_i64(write.key_id, "key_id")?,
                write.key_source.as_str(),
                to_i64(write.final_credential_id, "final_credential_id")?,
                write.endpoint,
                write.http_status.map(i64::from),
                write.final_status,
                write.error_type,
                write.severity.as_str(),
                write.error_message,
                write.recovered,
                write.pinned,
                write.retention_exempt,
                write.omitted_due_to_disk_pressure,
                i64::try_from(payload_count)?,
                to_i64(original_bytes, "original_bytes")?,
                to_i64(compressed_bytes, "compressed_bytes")?,
                now,
                now,
            ],
        )?;
        for part in &write.payloads {
            tx.execute(
                "INSERT INTO error_snapshot_payloads (
                    snapshot_id, seq, kind, attempt, codec, content_type, part_index, part_count,
                    original_bytes, compressed_bytes, sha256, data
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    write.snapshot_id,
                    i64::from(part.seq),
                    payload_kind_str(part.kind),
                    part.attempt.map(i64::from),
                    part.codec,
                    part.content_type,
                    i64::from(part.part_index),
                    i64::from(part.part_count),
                    to_i64(part.original_bytes, "payload original_bytes")?,
                    i64::try_from(part.data.len())?,
                    part.sha256,
                    part.data,
                ],
            )?;
        }
        tx.commit()?;
        Ok(InsertOutcome::Inserted(write.snapshot_id.clone()))
    }

    pub fn query_paged(&self, query: &SnapshotQuery) -> anyhow::Result<SnapshotPage> {
        let (where_sql, values) = build_where(query)?;
        let conn = self.conn.lock();
        let count_sql = format!("SELECT COUNT(*) FROM error_snapshots{where_sql}");
        let total_i64: i64 =
            conn.query_row(&count_sql, params_from_iter(values.iter()), |row| {
                row.get(0)
            })?;

        let limit = if query.limit == 0 {
            DEFAULT_QUERY_LIMIT
        } else {
            query.limit.min(MAX_QUERY_LIMIT)
        };
        let mut page_values = values;
        page_values.push(rusqlite::types::Value::Integer(i64::try_from(limit)?));
        page_values.push(rusqlite::types::Value::Integer(i64::try_from(
            query.offset,
        )?));
        let sql = format!(
            "{}{} ORDER BY ts_epoch DESC, snapshot_id DESC LIMIT ? OFFSET ?",
            summary_select(),
            where_sql
        );
        let mut stmt = conn.prepare(&sql)?;
        let records = stmt
            .query_map(params_from_iter(page_values.iter()), summary_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(SnapshotPage {
            records,
            total: usize::try_from(total_i64)?,
        })
    }

    pub fn get(&self, id: &str) -> anyhow::Result<Option<SnapshotDetail>> {
        let conn = self.conn.lock();
        let sql = format!("{} WHERE snapshot_id = ?1", summary_select());
        let Some(summary) = conn
            .query_row(&sql, params![id], summary_from_row)
            .optional()?
        else {
            return Ok(None);
        };
        let mut stmt = conn.prepare(
            "SELECT seq, kind, attempt, content_type, SUM(original_bytes),
                    SUM(compressed_bytes), sha256, COUNT(*)
             FROM error_snapshot_payloads WHERE snapshot_id = ?1
             GROUP BY seq, kind, attempt, content_type, sha256 ORDER BY seq ASC",
        )?;
        let payloads = stmt
            .query_map(params![id], payload_meta_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(SnapshotDetail { summary, payloads }))
    }

    pub fn read_payload(
        &self,
        id: &str,
        logical_seq: u32,
    ) -> anyhow::Result<Option<DecodedPayload>> {
        let parts = {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT seq, kind, attempt, codec, content_type, part_index, part_count,
                        original_bytes, sha256, data
                 FROM error_snapshot_payloads
                 WHERE snapshot_id = ?1 AND seq = ?2 ORDER BY part_index ASC",
            )?;
            stmt.query_map(params![id, i64::from(logical_seq)], |row| {
                Ok(EncodedPayloadPart {
                    seq: from_u32(row.get::<_, i64>(0)?, 0)?,
                    kind: payload_kind_from_db(&row.get::<_, String>(1)?, 1)?,
                    attempt: row
                        .get::<_, Option<i64>>(2)?
                        .map(|value| from_u32(value, 2))
                        .transpose()?,
                    codec: row.get(3)?,
                    content_type: row.get(4)?,
                    part_index: from_u32(row.get::<_, i64>(5)?, 5)?,
                    part_count: from_u32(row.get::<_, i64>(6)?, 6)?,
                    original_bytes: from_u64(row.get::<_, i64>(7)?, 7)?,
                    sha256: row.get(8)?,
                    data: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if parts.is_empty() {
            return Ok(None);
        }
        let meta = SnapshotPayloadMeta {
            seq: logical_seq,
            kind: parts[0].kind,
            attempt: parts[0].attempt,
            content_type: parts[0].content_type.clone(),
            original_bytes: parts.iter().map(|part| part.original_bytes).sum(),
            compressed_bytes: parts
                .iter()
                .map(|part| u64::try_from(part.data.len()))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .sum(),
            sha256: parts[0].sha256.clone(),
            part_count: u32::try_from(parts.len())?,
        };
        let data = crate::anthropic::error_snapshot::decode_payload_parts(
            &parts,
            crate::anthropic::error_snapshot::MAX_DECOMPRESSED_PAYLOAD_BYTES,
        )?;
        Ok(Some(DecodedPayload { meta, data }))
    }

    pub fn set_pinned(&self, id: &str, pinned: bool) -> anyhow::Result<bool> {
        let changed = self.conn.lock().execute(
            "UPDATE error_snapshots SET pinned = ?2, updated_at = ?3 WHERE snapshot_id = ?1",
            params![id, pinned, chrono::Utc::now().timestamp()],
        )?;
        Ok(changed > 0)
    }

    pub fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let changed = self.conn.lock().execute(
            "DELETE FROM error_snapshots WHERE snapshot_id = ?1",
            params![id],
        )?;
        Ok(changed > 0)
    }

    pub fn insert_with_fallback(&self, write: &SnapshotWrite) -> anyhow::Result<InsertOutcome> {
        let mut last_error = None;
        for delay_ms in [0u64, 25, 75, 150] {
            if delay_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            match self.insert(write) {
                Ok(outcome) => return Ok(outcome),
                Err(error) if is_busy_error(&error) => last_error = Some(error),
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            }
        }
        let error = last_error.unwrap_or_else(|| anyhow::anyhow!("未知快照数据库错误"));
        let Some(dir) = &self.fallback_dir else {
            return Err(error);
        };
        write_fallback_atomic(dir, write)?;
        tracing::warn!(
            snapshot_id = %write.snapshot_id,
            trace_id = %write.trace_id,
            error_type = %write.error_type,
            "错误快照数据库写入失败，已写入 fallback"
        );
        Ok(InsertOutcome::Fallback(write.snapshot_id.clone()))
    }

    pub fn import_fallback(&self) -> anyhow::Result<FallbackImportReport> {
        let Some(dir) = &self.fallback_dir else {
            return Ok(FallbackImportReport::default());
        };
        self.import_fallback_dir(dir)
    }

    pub fn import_fallback_dir(
        &self,
        dir: &std::path::Path,
    ) -> anyhow::Result<FallbackImportReport> {
        if !dir.exists() {
            return Ok(FallbackImportReport::default());
        }
        let corrupt_dir = dir.join("corrupt");
        let mut report = FallbackImportReport::default();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file()
                || path.extension().and_then(|value| value.to_str()) != Some("zst")
                || !path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.ends_with(".snapshot.zst"))
            {
                continue;
            }
            let write = match read_fallback(&path) {
                Ok(write) => write,
                Err(error) => {
                    report.failed += 1;
                    std::fs::create_dir_all(&corrupt_dir)?;
                    let name = path
                        .file_name()
                        .ok_or_else(|| anyhow::anyhow!("fallback 文件名缺失"))?;
                    let target = corrupt_dir.join(name);
                    if target.exists() {
                        let unique = corrupt_dir.join(format!(
                            "{}.{}.corrupt",
                            name.to_string_lossy(),
                            uuid::Uuid::new_v4()
                        ));
                        std::fs::rename(&path, unique)?;
                    } else {
                        std::fs::rename(&path, target)?;
                    }
                    tracing::warn!(file = %name.to_string_lossy(), error = %error, "fallback 导入失败，已隔离");
                    continue;
                }
            };
            match self.insert(&write) {
                Ok(InsertOutcome::Inserted(_)) | Ok(InsertOutcome::Fallback(_)) => {
                    report.imported += 1;
                    std::fs::remove_file(&path)?;
                }
                Ok(InsertOutcome::Existing(_)) => {
                    report.existing += 1;
                    std::fs::remove_file(&path)?;
                }
                Err(error) => {
                    report.failed += 1;
                    tracing::warn!(
                        snapshot_id = %write.snapshot_id,
                        trace_id = %write.trace_id,
                        error_type = %write.error_type,
                        error = %error,
                        "fallback 数据库导入失败，保留文件等待下次重试"
                    );
                }
            }
        }
        Ok(report)
    }

    pub fn run_maintenance(&self) -> anyhow::Result<MaintenanceReport> {
        self.run_maintenance_at(chrono::Utc::now().timestamp())
    }

    pub fn run_maintenance_at(&self, now_epoch: i64) -> anyhow::Result<MaintenanceReport> {
        let imported = self.import_fallback()?.imported;
        let policy = self.policy();
        let retention_secs = i64::from(policy.retention_days).saturating_mul(86_400);
        let cutoff = now_epoch.saturating_sub(retention_secs);
        let mut deleted = 0usize;
        {
            let conn = self.conn.lock();
            for severity in ["warning", "error", "info"] {
                deleted += conn.execute(
                    "DELETE FROM error_snapshots
                     WHERE ts_epoch < ?1 AND severity = ?2
                       AND pinned = 0 AND retention_exempt = 0 AND severity <> 'critical'",
                    params![cutoff, severity],
                )?;
            }
        }

        let mut status = self.storage_status()?;
        if status.total_bytes > policy.max_storage_bytes {
            let conn = self.conn.lock();
            loop {
                let candidate: Option<(String, u64)> = conn
                    .query_row(
                        "SELECT snapshot_id, compressed_bytes FROM error_snapshots
                         WHERE pinned = 0 AND retention_exempt = 0 AND severity <> 'critical'
                           AND severity IN ('warning', 'error', 'info')
                         ORDER BY CASE severity WHEN 'warning' THEN 0 WHEN 'error' THEN 1 ELSE 2 END,
                                  ts_epoch ASC LIMIT 1",
                        [],
                        |row| Ok((row.get(0)?, from_u64(row.get::<_, i64>(1)?, 1)?)),
                    )
                    .optional()?;
                let Some((id, estimated_bytes)) = candidate else {
                    break;
                };
                deleted += conn.execute(
                    "DELETE FROM error_snapshots WHERE snapshot_id = ?1
                       AND pinned = 0 AND retention_exempt = 0 AND severity <> 'critical'",
                    params![id],
                )?;
                status.total_bytes = status.total_bytes.saturating_sub(estimated_bytes.max(1));
                if status.total_bytes <= policy.max_storage_bytes {
                    break;
                }
            }
        }
        {
            let conn = self.conn.lock();
            conn.execute_batch(
                "PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum(4096);",
            )?;
        }
        status = self.storage_status()?;
        let disk_pressure = status.available_bytes < policy.min_free_disk_bytes;
        self.disk_pressure.store(disk_pressure, Ordering::Release);
        Ok(MaintenanceReport {
            deleted,
            imported,
            disk_pressure,
            total_bytes: status.total_bytes,
        })
    }

    pub fn capture_mode(&self) -> CaptureMode {
        if self.disk_pressure.load(Ordering::Acquire) {
            CaptureMode::MetadataOnly
        } else {
            CaptureMode::Full
        }
    }

    pub fn storage_status(&self) -> anyhow::Result<StorageStatus> {
        let policy = self.policy();
        let mut paths = Vec::new();
        let (db_bytes, wal_bytes, shm_bytes) = if let Some(db) = &self.db_path {
            let wal = sidecar_path(db, "-wal");
            let shm = sidecar_path(db, "-shm");
            paths.extend([db.clone(), wal.clone(), shm.clone()]);
            (
                self.storage_probe.tree_bytes(std::slice::from_ref(db))?,
                self.storage_probe.tree_bytes(std::slice::from_ref(&wal))?,
                self.storage_probe.tree_bytes(std::slice::from_ref(&shm))?,
            )
        } else {
            (0, 0, 0)
        };
        let fallback_bytes = if let Some(fallback) = &self.fallback_dir {
            paths.push(fallback.clone());
            self.storage_probe
                .tree_bytes(std::slice::from_ref(fallback))?
        } else {
            0
        };
        let total_bytes = if paths.is_empty() {
            self.storage_probe.tree_bytes(&[])?
        } else {
            self.storage_probe.tree_bytes(&paths)?
        };
        let probe_path = self
            .db_path
            .as_deref()
            .and_then(std::path::Path::parent)
            .or(self.fallback_dir.as_deref())
            .unwrap_or_else(|| std::path::Path::new("."));
        let available_bytes = self.storage_probe.available_bytes(probe_path)?;
        let conn = self.conn.lock();
        let (records, pinned_records, critical_records): (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN pinned = 1 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN severity = 'critical' THEN 1 ELSE 0 END), 0)
             FROM error_snapshots",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(StorageStatus {
            db_bytes,
            wal_bytes,
            shm_bytes,
            fallback_bytes,
            total_bytes,
            available_bytes,
            max_storage_bytes: policy.max_storage_bytes,
            min_free_disk_bytes: policy.min_free_disk_bytes,
            disk_pressure: available_bytes < policy.min_free_disk_bytes,
            records: u64::try_from(records)?,
            pinned_records: u64::try_from(pinned_records)?,
            critical_records: u64::try_from(critical_records)?,
        })
    }

    pub fn recent_trace_links(&self, since_epoch: i64) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT trace_id, snapshot_id FROM error_snapshots
             WHERE ts_epoch >= ?1 ORDER BY ts_epoch ASC",
        )?;
        Ok(stmt
            .query_map(params![since_epoch], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn write_fallback_atomic(dir: &std::path::Path, write: &SnapshotWrite) -> anyhow::Result<()> {
    validate_snapshot_filename(&write.snapshot_id)?;
    std::fs::create_dir_all(dir)?;
    let final_path = dir.join(format!("{}.snapshot.zst", write.snapshot_id));
    if final_path.exists() {
        return Ok(());
    }
    let mut snapshot = serde_json::to_value(write)?;
    snapshot
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("fallback 快照元数据不是对象"))?
        .remove("payloads");
    let payloads = write
        .payloads
        .iter()
        .map(|part| -> anyhow::Result<FallbackPayloadPart> {
            Ok(FallbackPayloadPart {
                seq: part.seq,
                kind: part.kind,
                attempt: part.attempt,
                codec: part.codec.clone(),
                content_type: part.content_type.clone(),
                part_index: part.part_index,
                part_count: part.part_count,
                original_bytes: part.original_bytes,
                compressed_bytes: u64::try_from(part.data.len())?,
                sha256: part.sha256.clone(),
                data_b64: base64::engine::general_purpose::STANDARD.encode(&part.data),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let envelope = FallbackEnvelope {
        version: 1,
        snapshot,
        payloads,
    };
    let serialized = serde_json::to_vec(&envelope)?;
    let compressed = zstd::stream::encode_all(serialized.as_slice(), 3)?;
    let temp = dir.join(format!(
        ".{}.{}.tmp",
        write.snapshot_id,
        uuid::Uuid::new_v4()
    ));
    if let Err(error) =
        std::fs::write(&temp, compressed).and_then(|_| std::fs::rename(&temp, &final_path))
    {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

fn read_fallback(path: &std::path::Path) -> anyhow::Result<SnapshotWrite> {
    const MAX_FALLBACK_ENVELOPE_BYTES: u64 = 512 * 1024 * 1024;
    if std::fs::metadata(path)?.len() > MAX_FALLBACK_ENVELOPE_BYTES {
        anyhow::bail!("fallback 压缩文件超过读取上限");
    }
    let compressed = std::fs::read(path)?;
    let decoder = zstd::stream::read::Decoder::new(compressed.as_slice())?;
    let mut serialized = Vec::new();
    decoder
        .take(MAX_FALLBACK_ENVELOPE_BYTES + 1)
        .read_to_end(&mut serialized)?;
    if u64::try_from(serialized.len())? > MAX_FALLBACK_ENVELOPE_BYTES {
        anyhow::bail!("fallback envelope 超过解压上限");
    }
    let envelope: FallbackEnvelope = serde_json::from_slice(&serialized)?;
    if envelope.version != 1 {
        anyhow::bail!("不支持的 fallback 版本: {}", envelope.version);
    }
    let mut parts = Vec::with_capacity(envelope.payloads.len());
    for part in envelope.payloads {
        let data = base64::engine::general_purpose::STANDARD.decode(&part.data_b64)?;
        if u64::try_from(data.len())? != part.compressed_bytes {
            anyhow::bail!("fallback payload 压缩长度校验失败");
        }
        parts.push(EncodedPayloadPart {
            seq: part.seq,
            kind: part.kind,
            attempt: part.attempt,
            codec: part.codec,
            content_type: part.content_type,
            part_index: part.part_index,
            part_count: part.part_count,
            original_bytes: part.original_bytes,
            sha256: part.sha256,
            data,
        });
    }
    let mut snapshot = envelope.snapshot;
    snapshot
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("fallback 快照元数据不是对象"))?
        .insert("payloads".to_string(), serde_json::to_value(parts)?);
    let write: SnapshotWrite = serde_json::from_value(snapshot)?;
    validate_snapshot_filename(&write.snapshot_id)?;
    Ok(write)
}

fn validate_snapshot_filename(id: &str) -> anyhow::Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        anyhow::bail!("snapshot_id 不能安全用作 fallback 文件名");
    }
    Ok(())
}

fn is_busy_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<rusqlite::Error>())
        .is_some_and(|sqlite| {
            matches!(
                sqlite,
                rusqlite::Error::SqliteFailure(code, _)
                    if matches!(
                        code.code,
                        rusqlite::ErrorCode::DatabaseBusy
                            | rusqlite::ErrorCode::DatabaseLocked
                    )
            )
        })
}

fn sidecar_path(path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn path_tree_bytes(path: &std::path::Path) -> std::io::Result<u64> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }
    std::fs::read_dir(path)?.try_fold(0u64, |total, entry| {
        total
            .checked_add(path_tree_bytes(&entry?.path())?)
            .ok_or_else(|| std::io::Error::other("快照目录大小溢出"))
    })
}

fn initialize_connection(conn: &Connection, is_new: bool) -> rusqlite::Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
    if is_new {
        conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
    }
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "error_snapshots.db schema 版本 {version} 高于当前支持版本 {SCHEMA_VERSION}"
        )));
    }
    conn.execute_batch(SCHEMA)?;
    if version < SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    conn.pragma_update(None, "journal_mode", "WAL")?;
    Ok(())
}

fn summary_select() -> &'static str {
    "SELECT snapshot_id, trace_id, ts, model, is_stream, key_id, key_source,
            final_credential_id, endpoint, http_status, final_status, error_type, severity,
            error_message, recovered, pinned, retention_exempt, omitted_due_to_disk_pressure,
            payload_count, original_bytes, compressed_bytes, created_at, updated_at
     FROM error_snapshots"
}

fn summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotSummary> {
    Ok(SnapshotSummary {
        snapshot_id: row.get(0)?,
        trace_id: row.get(1)?,
        ts: row.get(2)?,
        model: row.get(3)?,
        is_stream: row.get(4)?,
        key_id: from_u64(row.get::<_, i64>(5)?, 5)?,
        key_source: key_source_from_db(&row.get::<_, String>(6)?, 6)?,
        final_credential_id: from_u64(row.get::<_, i64>(7)?, 7)?,
        endpoint: row.get(8)?,
        http_status: row
            .get::<_, Option<i64>>(9)?
            .map(|value| u16::try_from(value).map_err(sql_range_error(9)))
            .transpose()?,
        final_status: row.get(10)?,
        error_type: row.get(11)?,
        severity: SnapshotSeverity::from_db(&row.get::<_, String>(12)?).map_err(|error| {
            sql_decode_error(
                12,
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })?,
        error_message: row.get(13)?,
        recovered: row.get(14)?,
        pinned: row.get(15)?,
        retention_exempt: row.get(16)?,
        omitted_due_to_disk_pressure: row.get(17)?,
        payload_count: from_u32(row.get::<_, i64>(18)?, 18)?,
        original_bytes: from_u64(row.get::<_, i64>(19)?, 19)?,
        compressed_bytes: from_u64(row.get::<_, i64>(20)?, 20)?,
        created_at: row.get(21)?,
        updated_at: row.get(22)?,
    })
}

fn payload_meta_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotPayloadMeta> {
    Ok(SnapshotPayloadMeta {
        seq: from_u32(row.get::<_, i64>(0)?, 0)?,
        kind: payload_kind_from_db(&row.get::<_, String>(1)?, 1)?,
        attempt: row
            .get::<_, Option<i64>>(2)?
            .map(|value| from_u32(value, 2))
            .transpose()?,
        content_type: row.get(3)?,
        original_bytes: from_u64(row.get::<_, i64>(4)?, 4)?,
        compressed_bytes: from_u64(row.get::<_, i64>(5)?, 5)?,
        sha256: row.get(6)?,
        part_count: from_u32(row.get::<_, i64>(7)?, 7)?,
    })
}

fn build_where(query: &SnapshotQuery) -> anyhow::Result<(String, Vec<rusqlite::types::Value>)> {
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    macro_rules! push_value {
        ($column:literal, $value:expr) => {{
            clauses.push(concat!($column, " = ?"));
            values.push($value);
        }};
    }
    if let Some(value) = &query.trace_id {
        push_value!("trace_id", value.clone().into());
    }
    if let Some(value) = &query.model {
        push_value!("model", value.clone().into());
    }
    if let Some(value) = &query.error_type {
        push_value!("error_type", value.clone().into());
    }
    if let Some(value) = query.http_status {
        push_value!("http_status", i64::from(value).into());
    }
    if let Some(value) = query.credential_id {
        push_value!(
            "final_credential_id",
            to_i64(value, "credential_id")?.into()
        );
    }
    if let Some(value) = &query.severity {
        push_value!("severity", value.as_str().to_string().into());
    }
    if let Some(value) = query.recovered {
        push_value!("recovered", i64::from(value).into());
    }
    if let Some(value) = query.pinned {
        push_value!("pinned", i64::from(value).into());
    }
    if let Some(value) = query.from_epoch {
        clauses.push("ts_epoch >= ?");
        values.push(value.into());
    }
    if let Some(value) = query.to_epoch {
        clauses.push("ts_epoch <= ?");
        values.push(value.into());
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    Ok((where_sql, values))
}

fn to_i64(value: u64, field: &str) -> anyhow::Result<i64> {
    i64::try_from(value).map_err(|_| anyhow::anyhow!("{field} 超出 SQLite INTEGER 范围"))
}

fn from_u64(value: i64, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(sql_range_error(column))
}

fn from_u32(value: i64, column: usize) -> rusqlite::Result<u32> {
    u32::try_from(value).map_err(sql_range_error(column))
}

fn sql_range_error<T: std::error::Error + Send + Sync + 'static>(
    column: usize,
) -> impl FnOnce(T) -> rusqlite::Error {
    move |error| sql_decode_error(column, error)
}

fn sql_decode_error(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Integer,
        Box::new(error),
    )
}

fn key_source_from_db(
    value: &str,
    column: usize,
) -> rusqlite::Result<crate::admin::trace_db::TraceKeySource> {
    match value {
        "masterApiKey" => Ok(crate::admin::trace_db::TraceKeySource::MasterApiKey),
        "clientKey" => Ok(crate::admin::trace_db::TraceKeySource::ClientKey),
        _ => Err(sql_decode_error(
            column,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("未知 trace key_source: {value}"),
            ),
        )),
    }
}

fn payload_kind_str(kind: SnapshotPayloadKind) -> &'static str {
    match kind {
        SnapshotPayloadKind::ClientRequest => "client_request",
        SnapshotPayloadKind::KiroRequest => "kiro_request",
        SnapshotPayloadKind::UpstreamResponse => "upstream_response",
        SnapshotPayloadKind::ToolDiagnostics => "tool_diagnostics",
        SnapshotPayloadKind::StreamTail => "stream_tail",
        SnapshotPayloadKind::InternalError => "internal_error",
    }
}

fn payload_kind_from_db(value: &str, column: usize) -> rusqlite::Result<SnapshotPayloadKind> {
    match value {
        "client_request" => Ok(SnapshotPayloadKind::ClientRequest),
        "kiro_request" => Ok(SnapshotPayloadKind::KiroRequest),
        "upstream_response" => Ok(SnapshotPayloadKind::UpstreamResponse),
        "tool_diagnostics" => Ok(SnapshotPayloadKind::ToolDiagnostics),
        "stream_tail" => Ok(SnapshotPayloadKind::StreamTail),
        "internal_error" => Ok(SnapshotPayloadKind::InternalError),
        _ => Err(sql_decode_error(
            column,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("未知快照 payload kind: {value}"),
            ),
        )),
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS error_snapshots (
  snapshot_id TEXT PRIMARY KEY,
  trace_id TEXT NOT NULL UNIQUE,
  ts TEXT NOT NULL,
  ts_epoch INTEGER NOT NULL,
  model TEXT NOT NULL,
  is_stream INTEGER NOT NULL,
  key_id INTEGER NOT NULL,
  key_source TEXT NOT NULL,
  final_credential_id INTEGER NOT NULL,
  endpoint TEXT,
  http_status INTEGER,
  final_status TEXT NOT NULL,
  error_type TEXT NOT NULL,
  severity TEXT NOT NULL,
  error_message TEXT,
  recovered INTEGER NOT NULL,
  pinned INTEGER NOT NULL DEFAULT 0,
  retention_exempt INTEGER NOT NULL DEFAULT 0,
  omitted_due_to_disk_pressure INTEGER NOT NULL DEFAULT 0,
  payload_count INTEGER NOT NULL,
  original_bytes INTEGER NOT NULL,
  compressed_bytes INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_error_snapshots_ts ON error_snapshots(ts_epoch DESC);
CREATE INDEX IF NOT EXISTS idx_error_snapshots_trace ON error_snapshots(trace_id);
CREATE INDEX IF NOT EXISTS idx_error_snapshots_severity ON error_snapshots(severity, ts_epoch DESC);
CREATE INDEX IF NOT EXISTS idx_error_snapshots_type ON error_snapshots(error_type, ts_epoch DESC);
CREATE INDEX IF NOT EXISTS idx_error_snapshots_status ON error_snapshots(http_status, ts_epoch DESC);
CREATE INDEX IF NOT EXISTS idx_error_snapshots_credential ON error_snapshots(final_credential_id, ts_epoch DESC);
CREATE INDEX IF NOT EXISTS idx_error_snapshots_pinned ON error_snapshots(pinned, ts_epoch DESC);

CREATE TABLE IF NOT EXISTS error_snapshot_payloads (
  snapshot_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL,
  attempt INTEGER,
  codec TEXT NOT NULL,
  content_type TEXT NOT NULL,
  part_index INTEGER NOT NULL,
  part_count INTEGER NOT NULL,
  original_bytes INTEGER NOT NULL,
  compressed_bytes INTEGER NOT NULL,
  sha256 TEXT NOT NULL,
  data BLOB NOT NULL,
  PRIMARY KEY (snapshot_id, seq, part_index),
  FOREIGN KEY (snapshot_id) REFERENCES error_snapshots(snapshot_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_error_payloads_snapshot ON error_snapshot_payloads(snapshot_id, seq);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FixedProbe {
        tree_bytes: u64,
        available_bytes: u64,
    }

    impl StorageProbe for FixedProbe {
        fn available_bytes(&self, _path: &std::path::Path) -> std::io::Result<u64> {
            Ok(self.available_bytes)
        }

        fn tree_bytes(&self, _paths: &[PathBuf]) -> std::io::Result<u64> {
            Ok(self.tree_bytes)
        }
    }

    fn test_policy() -> ErrorSnapshotPolicy {
        ErrorSnapshotPolicy {
            enabled: true,
            retention_days: 90,
            max_storage_bytes: 200 * 1024 * 1024 * 1024,
            capture_recovered: true,
            capture_bodies: true,
            min_free_disk_bytes: 100 * 1024 * 1024 * 1024,
        }
    }

    fn sample_write(snapshot_id: &str, trace_id: &str) -> SnapshotWrite {
        let mut first = crate::anthropic::error_snapshot::encode_payload(
            crate::common::error_snapshot::SnapshotPayloadKind::ClientRequest,
            None,
            "application/json",
            r#"{"request":"完整"}"#.as_bytes(),
        )
        .unwrap();
        let mut second = crate::anthropic::error_snapshot::encode_payload(
            crate::common::error_snapshot::SnapshotPayloadKind::InternalError,
            Some(0),
            "text/plain",
            b"upstream failed",
        )
        .unwrap();
        for part in &mut first {
            part.seq = 0;
        }
        for part in &mut second {
            part.seq = 1;
        }
        first.extend(second);
        SnapshotWrite {
            snapshot_id: snapshot_id.to_string(),
            trace_id: trace_id.to_string(),
            ts: "2026-07-14T00:00:00Z".to_string(),
            ts_epoch: 1_752_451_200,
            model: "claude-opus-4-8".to_string(),
            is_stream: true,
            key_id: 7,
            key_source: crate::admin::trace_db::TraceKeySource::ClientKey,
            final_credential_id: 9,
            endpoint: Some("ide".to_string()),
            http_status: Some(502),
            final_status: "error".to_string(),
            error_type: "upstream_error".to_string(),
            severity: SnapshotSeverity::Error,
            error_message: Some("upstream failed".to_string()),
            recovered: false,
            pinned: false,
            retention_exempt: false,
            omitted_due_to_disk_pressure: false,
            payloads: first,
        }
    }

    fn test_store_with_probe(tree_bytes: u64, available_bytes: u64) -> ErrorSnapshotStore {
        ErrorSnapshotStore::open_in_memory_with_probe(
            test_policy(),
            Arc::new(FixedProbe {
                tree_bytes,
                available_bytes,
            }),
        )
        .unwrap()
    }

    fn insert_at(
        store: &ErrorSnapshotStore,
        id: &str,
        severity: SnapshotSeverity,
        pinned: bool,
        retention_exempt: bool,
        ts_epoch: i64,
    ) {
        let mut write = sample_write(id, &format!("trace-{id}"));
        write.severity = severity;
        write.pinned = pinned;
        write.retention_exempt = retention_exempt;
        write.ts_epoch = ts_epoch;
        store.insert(&write).unwrap();
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kiro-{name}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn inserts_snapshot_and_payloads_atomically_and_lists_without_blob_data() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let write = sample_write("snap-1", "trace-1");
        store.insert(&write).unwrap();

        let page = store
            .query_paged(&SnapshotQuery {
                limit: 50,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.records[0].snapshot_id, "snap-1");
        assert_eq!(page.records[0].payload_count, 2);

        let detail = store.get("snap-1").unwrap().unwrap();
        assert_eq!(detail.payloads.len(), 2);
        assert!(detail.payloads.iter().all(|p| p.compressed_bytes > 0));

        let payload = store.read_payload("snap-1", 0).unwrap().unwrap();
        assert_eq!(payload.meta.content_type, "application/json");
        assert_eq!(payload.data, r#"{"request":"完整"}"#.as_bytes());
    }

    #[test]
    fn duplicate_trace_id_is_idempotent() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let first = sample_write("snap-1", "trace-1");
        let second = sample_write("snap-2", "trace-1");
        assert_eq!(
            store.insert(&first).unwrap(),
            InsertOutcome::Inserted("snap-1".into())
        );
        assert_eq!(
            store.insert(&second).unwrap(),
            InsertOutcome::Existing("snap-1".into())
        );
    }

    #[test]
    fn file_database_reopens_idempotently_and_rejects_future_schema() {
        let root =
            std::env::temp_dir().join(format!("kiro-error-snapshot-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join("error_snapshots.db");
        let fallback = root.join("fallback");

        let store =
            ErrorSnapshotStore::open(db_path.clone(), fallback.clone(), test_policy()).unwrap();
        store.insert(&sample_write("snap-1", "trace-1")).unwrap();
        drop(store);

        let reopened = ErrorSnapshotStore::open(db_path.clone(), fallback, test_policy()).unwrap();
        assert_eq!(
            reopened
                .query_paged(&SnapshotQuery::default())
                .unwrap()
                .total,
            1
        );
        drop(reopened);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);
        let error = ErrorSnapshotStore::open(db_path, root.join("fallback-2"), test_policy())
            .err()
            .expect("未来 schema 必须拒绝打开");
        assert!(error.to_string().contains("高于当前支持版本"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pin_and_delete_update_only_the_requested_snapshot() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        store.insert(&sample_write("snap-1", "trace-1")).unwrap();

        assert!(store.set_pinned("snap-1", true).unwrap());
        let detail = store.get("snap-1").unwrap().unwrap();
        assert!(detail.summary.pinned);
        assert!(store.delete("snap-1").unwrap());
        assert!(store.get("snap-1").unwrap().is_none());
        assert!(!store.delete("missing").unwrap());
    }

    #[test]
    fn cleanup_never_deletes_pinned_or_critical_records() {
        let store = test_store_with_probe(50, 1_000);
        let mut policy = store.policy();
        policy.retention_days = 1;
        policy.max_storage_bytes = 1_000;
        policy.min_free_disk_bytes = 100;
        store.set_policy(policy);
        insert_at(
            &store,
            "warning-old",
            SnapshotSeverity::Warning,
            false,
            false,
            1,
        );
        insert_at(
            &store,
            "error-old",
            SnapshotSeverity::Error,
            false,
            false,
            2,
        );
        insert_at(&store, "pinned", SnapshotSeverity::Warning, true, false, 3);
        insert_at(
            &store,
            "critical",
            SnapshotSeverity::Critical,
            false,
            true,
            4,
        );

        let report = store.run_maintenance_at(100 * 86_400).unwrap();

        assert!(report.deleted >= 2);
        assert!(store.get("pinned").unwrap().is_some());
        assert!(store.get("critical").unwrap().is_some());
    }

    #[test]
    fn low_free_space_enters_metadata_only_mode() {
        let store = test_store_with_probe(10_000, 99);
        let mut policy = store.policy();
        policy.min_free_disk_bytes = 100;
        store.set_policy(policy);

        let report = store.run_maintenance_at(1_000).unwrap();

        assert!(report.disk_pressure);
        assert_eq!(store.capture_mode(), CaptureMode::MetadataOnly);
    }

    #[test]
    fn fallback_round_trip_is_atomic_and_idempotent() {
        let dir = temp_path("snapshot-fallback");
        let write = sample_write("snap-fallback", "trace-fallback");
        write_fallback_atomic(&dir, &write).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        assert_eq!(store.import_fallback_dir(&dir).unwrap().imported, 1);
        assert_eq!(store.import_fallback_dir(&dir).unwrap().imported, 0);
        assert!(store.get("snap-fallback").unwrap().is_some());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
