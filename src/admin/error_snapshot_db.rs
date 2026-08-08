use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use base64::Engine as _;
use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, OptionalExtension as _, params, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::common::error_snapshot::{EncodedPayloadPart, SnapshotPayloadKind};

const SCHEMA_VERSION: i64 = 3;
/// 同一请求在短时间内重复失败时只保留一份完整现场。
const DEDUP_WINDOW_SECS: i64 = 60;
const DEFAULT_QUERY_LIMIT: usize = 50;
const MAX_QUERY_LIMIT: usize = 1000;
const MAINTENANCE_BATCH_SIZE: usize = 512;

/// `stream_tail` 的独立保留期：48 小时。
///
/// 短于快照库自身的 `retention_days`，因为尾部里是模型输出明文，而断流排障
/// 基本在 1–2 天内完成。见 [`ErrorSnapshotStore::prune_expired_stream_tails`]。
const STREAM_TAIL_RETENTION_SECS: i64 = 48 * 3600;

/// 完整请求体（`client_request` / `kiro_request` / `upstream_response`）的保留期。
///
/// 这三类占了全库体积的**绝大部分**——线上实测 `client_request` 3.5 GB、
/// `kiro_request` 1.9 GB，其余五类加起来不到 90 MB。它们是排障时最有用的东西，
/// 但和 `stream_tail` 一样：排障基本在 1–2 天内完成，没有理由跟着快照库的
/// `retention_days`（默认 7 天）一起躺着。实测 ≥3 天的这两类合计 2 GB，
/// 全是"存着不会再看"的正文。
///
/// 取 72h 而不是 48h：给跨周末的排障留一天余量。需要长期留证的个案用
/// `pinned` / `retention_exempt` 显式保下来——本清理会跳过它们，这正是那两个
/// 标记存在的意义。快照元数据（时间、模型、凭据、attempt 链、错误类型）不受影响，
/// 按 `retention_days` 走，所以趋势统计不会因此断档。
const REQUEST_BODY_RETENTION_SECS: i64 = 72 * 3600;

/// 每轮维护回收的空闲页上限（页大小 4 KiB → 约 32 MiB）。
///
/// `PRAGMA auto_vacuum=INCREMENTAL` 只是**允许**回收，不会自己回收：必须显式执行
/// `PRAGMA incremental_vacuum`。之前从没执行过，于是线上 877,882 个空闲页
/// （**3.6 GB**）一直占着文件不放——删了数据文件也不缩。
///
/// 分批而不是一次回收干净：`incremental_vacuum` 整个过程持有连接锁，一次啃 3.6 GB
/// 会把请求热路径上的快照写入卡住好几秒。取 32 MiB 让单轮锁持有时间落在几十毫秒，
/// 而维护循环在 `needs_follow_up` 时是 250 ms 一轮，线上那 3.6 GB 约半分钟能啃完。
/// 追平之后每小时只需回收当轮删掉的那点，批量大小就更不构成问题。
const INCREMENTAL_VACUUM_PAGES: u32 = 8_192;
const SNAPSHOT_ROW_OVERHEAD_BYTES: u64 = 64 * 1024;

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

impl ErrorSnapshotPolicy {
    pub fn from_config(config: &crate::model::config::Config) -> Self {
        const GIB: u64 = 1024 * 1024 * 1024;
        Self {
            enabled: config.error_snapshot_enabled,
            retention_days: config.error_snapshot_retention_days,
            max_storage_bytes: config.error_snapshot_max_storage_gb.saturating_mul(GIB),
            capture_recovered: config.error_snapshot_capture_recovered,
            capture_bodies: config.error_snapshot_capture_bodies,
            min_free_disk_bytes: config.error_snapshot_min_free_disk_gb.saturating_mul(GIB),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotWrite {
    pub snapshot_id: String,
    pub trace_id: String,
    /// Sanitized client request 的稳定指纹。旧 fallback/数据库记录没有该字段时为空，
    /// 这样可以避免把无法确认相同请求的旧记录误合并。
    #[serde(default)]
    pub request_fingerprint: String,
    pub ts: String,
    pub ts_epoch: i64,
    pub model: String,
    pub is_stream: bool,
    pub key_id: u64,
    pub key_source: crate::admin::trace_db::TraceKeySource,
    #[serde(default)]
    pub response_mode: crate::admin::client_keys::ClientResponseMode,
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
    SkippedCapacity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureMode {
    Full,
    CriticalOnly,
    MetadataOnly,
    Disabled,
}

impl CaptureMode {
    fn as_u8(self) -> u8 {
        match self {
            Self::Full => 0,
            Self::CriticalOnly => 1,
            Self::MetadataOnly => 2,
            Self::Disabled => 3,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::CriticalOnly,
            2 => Self::MetadataOnly,
            3 => Self::Disabled,
            _ => Self::Full,
        }
    }
}

fn capture_mode_for(
    live_bytes: u64,
    max_storage_bytes: u64,
    available_bytes: u64,
    min_free_disk_bytes: u64,
    enabled: bool,
) -> CaptureMode {
    if !enabled || max_storage_bytes == 0 || live_bytes >= max_storage_bytes {
        return CaptureMode::Disabled;
    }
    if available_bytes < min_free_disk_bytes {
        return CaptureMode::MetadataOnly;
    }
    let utilization = u128::from(live_bytes).saturating_mul(100);
    let maximum = u128::from(max_storage_bytes);
    if utilization >= maximum.saturating_mul(90) {
        CaptureMode::MetadataOnly
    } else if utilization >= maximum.saturating_mul(80) {
        CaptureMode::CriticalOnly
    } else {
        CaptureMode::Full
    }
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
    pub needs_follow_up: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatus {
    pub db_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub fallback_bytes: u64,
    pub total_bytes: u64,
    pub allocated_bytes: u64,
    pub live_bytes: u64,
    pub reusable_bytes: u64,
    pub available_bytes: u64,
    pub max_storage_bytes: u64,
    pub min_free_disk_bytes: u64,
    pub disk_pressure: bool,
    pub records: u64,
    pub pinned_records: u64,
    pub critical_records: u64,
    pub skipped_capacity: u64,
    pub capture_mode: CaptureMode,
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
    pub response_mode: crate::admin::client_keys::ClientResponseMode,
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
    /// 短窗口内合并的重复错误数（首份记录为 1）。
    pub duplicate_count: u64,
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
    capture_mode: AtomicU8,
    skipped_capacity: AtomicU64,
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
        let initial_mode = if policy.enabled { 0 } else { 3 };
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: Some(path),
            fallback_dir: Some(fallback_dir),
            policy: RwLock::new(policy),
            capture_mode: AtomicU8::new(initial_mode),
            skipped_capacity: AtomicU64::new(0),
            storage_probe: Arc::new(RealStorageProbe),
        })
    }

    pub fn open_in_memory(policy: ErrorSnapshotPolicy) -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        initialize_connection(&conn, true)?;
        let initial_mode = if policy.enabled { 0 } else { 3 };
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: None,
            fallback_dir: None,
            policy: RwLock::new(policy),
            capture_mode: AtomicU8::new(initial_mode),
            skipped_capacity: AtomicU64::new(0),
            storage_probe: Arc::new(RealStorageProbe),
        })
    }

    /// 退出前把 WAL 截断。理由与 [`crate::admin::TraceStore::checkpoint_truncate`] 相同：
    /// PASSIVE 自动检查点只复用不缩文件，硬退出又从不截断，于是启动时白付恢复开销。
    ///
    /// 拿不到写锁就放弃——卡住退出比留着一个大 WAL 严重得多。
    pub fn checkpoint_truncate(&self) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
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
        let initial_mode = if policy.enabled { 0 } else { 3 };
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: None,
            fallback_dir: None,
            policy: RwLock::new(policy),
            capture_mode: AtomicU8::new(initial_mode),
            skipped_capacity: AtomicU64::new(0),
            storage_probe,
        })
    }

    pub fn policy(&self) -> ErrorSnapshotPolicy {
        self.policy.read().clone()
    }

    pub fn set_policy(&self, policy: ErrorSnapshotPolicy) {
        self.capture_mode.store(
            if policy.enabled {
                CaptureMode::Full.as_u8()
            } else {
                CaptureMode::Disabled.as_u8()
            },
            Ordering::Release,
        );
        *self.policy.write() = policy;
    }

    fn capacity_state_with_conn(&self, conn: &Connection) -> anyhow::Result<(CaptureMode, u64)> {
        let (_, sqlite_live_bytes, _) = sqlite_page_metrics(conn)?;
        let wal_bytes = self
            .db_path
            .as_ref()
            .map(|path| sidecar_path(path, "-wal"))
            .map(|path| self.storage_probe.tree_bytes(&[path]))
            .transpose()?
            .unwrap_or(0);
        let fallback_bytes = self
            .fallback_dir
            .as_ref()
            .map(|path| self.storage_probe.tree_bytes(std::slice::from_ref(path)))
            .transpose()?
            .unwrap_or(0);
        let probe_path = self
            .db_path
            .as_deref()
            .and_then(std::path::Path::parent)
            .or(self.fallback_dir.as_deref())
            .unwrap_or_else(|| std::path::Path::new("."));
        let available_bytes = self.storage_probe.available_bytes(probe_path)?;
        let policy = self.policy();
        let live_bytes = sqlite_live_bytes
            .saturating_add(wal_bytes)
            .saturating_add(fallback_bytes);
        Ok((
            capture_mode_for(
                live_bytes,
                policy.max_storage_bytes,
                available_bytes,
                policy.min_free_disk_bytes,
                policy.enabled,
            ),
            live_bytes,
        ))
    }

    pub fn insert(&self, write: &SnapshotWrite) -> anyhow::Result<InsertOutcome> {
        let mut conn = self.conn.lock();
        let (mut mode, live_bytes) = self.capacity_state_with_conn(&conn)?;
        let policy = self.policy();
        let payload_bytes = write.payloads.iter().fold(0u64, |total, part| {
            total.saturating_add(u64::try_from(part.data.len()).unwrap_or(u64::MAX))
        });
        let projected_bytes = live_bytes
            .saturating_add(SNAPSHOT_ROW_OVERHEAD_BYTES)
            .saturating_add(payload_bytes);
        if projected_bytes > policy.max_storage_bytes {
            mode = if write.severity == SnapshotSeverity::Critical
                && live_bytes.saturating_add(SNAPSHOT_ROW_OVERHEAD_BYTES)
                    <= policy.max_storage_bytes
            {
                CaptureMode::MetadataOnly
            } else {
                CaptureMode::Disabled
            };
        }
        self.capture_mode.store(mode.as_u8(), Ordering::Release);
        if mode == CaptureMode::Disabled
            || (mode != CaptureMode::Full && write.severity != SnapshotSeverity::Critical)
        {
            self.skipped_capacity.fetch_add(1, Ordering::Relaxed);
            return Ok(InsertOutcome::SkippedCapacity);
        }
        let mut metadata_write = None;
        if mode == CaptureMode::MetadataOnly {
            let mut sanitized = write.clone();
            sanitized.payloads.clear();
            sanitized.omitted_due_to_disk_pressure = true;
            metadata_write = Some(sanitized);
        }
        let write = metadata_write.as_ref().unwrap_or(write);
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

        // trace_id 只保证单个请求幂等；相同请求可能因为客户端重试产生不同 trace_id。
        // 只有拥有非空请求指纹时才合并，并且必须同时匹配错误类型和响应模式。
        let now = chrono::Utc::now().timestamp();
        if !write.request_fingerprint.is_empty()
            && let Some(existing) = conn
                .query_row(
                    "SELECT snapshot_id FROM error_snapshots
                     WHERE request_fingerprint = ?1
                       AND error_type = ?2
                       AND response_mode = ?3
                       AND updated_at >= ?4
                     ORDER BY updated_at DESC, snapshot_id DESC LIMIT 1",
                    params![
                        write.request_fingerprint,
                        write.error_type,
                        write.response_mode.as_str(),
                        now.saturating_sub(DEDUP_WINDOW_SECS),
                    ],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
        {
            conn.execute(
                "UPDATE error_snapshots
                 SET duplicate_count = COALESCE(duplicate_count, 1) + 1,
                     updated_at = ?2
                 WHERE snapshot_id = ?1",
                params![existing, now],
            )?;
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
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO error_snapshots (
                snapshot_id, trace_id, ts, ts_epoch, request_fingerprint, model, is_stream, key_id, key_source,
                response_mode, final_credential_id, endpoint, http_status, final_status, error_type, severity,
                error_message, recovered, pinned, retention_exempt, omitted_due_to_disk_pressure,
                payload_count, original_bytes, compressed_bytes, duplicate_count, created_at, updated_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27
             )",
            params![
                write.snapshot_id,
                write.trace_id,
                write.ts,
                write.ts_epoch,
                write.request_fingerprint,
                write.model,
                write.is_stream,
                to_i64(write.key_id, "key_id")?,
                write.key_source.as_str(),
                write.response_mode.as_str(),
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
                1i64,
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
        for entry in std::fs::read_dir(dir)?.take(MAINTENANCE_BATCH_SIZE) {
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
                Ok(InsertOutcome::SkippedCapacity) => {
                    report.failed += 1;
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
        let import_report = self.import_fallback()?;
        let imported = import_report.imported;
        let fallback_may_have_more = import_report
            .imported
            .saturating_add(import_report.existing)
            >= MAINTENANCE_BATCH_SIZE;
        let policy = self.policy();
        let retention_secs = i64::from(policy.retention_days).saturating_mul(86_400);
        let cutoff = now_epoch.saturating_sub(retention_secs);
        let before = self.storage_status()?;
        let target_bytes = policy.max_storage_bytes.saturating_mul(70) / 100;
        let capacity_cleanup = before.live_bytes > target_bytes;
        let started = std::time::Instant::now();
        let mut deleted = 0usize;
        {
            let mut conn = self.conn.lock();
            let tx = conn.transaction()?;
            let candidates = {
                let mut stmt = tx.prepare(
                    "SELECT snapshot_id FROM error_snapshots
                     WHERE pinned = 0 AND retention_exempt = 0 AND severity <> 'critical'
                       AND severity IN ('warning', 'error', 'info')
                       AND (ts_epoch < ?1 OR ?2 = 1)
                     ORDER BY CASE severity WHEN 'warning' THEN 0 WHEN 'error' THEN 1 ELSE 2 END,
                              ts_epoch ASC
                     LIMIT 512",
                )?;
                stmt.query_map(params![cutoff, capacity_cleanup], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for id in candidates {
                if started.elapsed() >= std::time::Duration::from_millis(250) {
                    break;
                }
                deleted += tx.execute(
                    "DELETE FROM error_snapshots WHERE snapshot_id = ?1
                       AND pinned = 0 AND retention_exempt = 0 AND severity <> 'critical'",
                    params![id],
                )?;
            }
            tx.commit()?;
        }
        self.prune_expired_stream_tails(now_epoch)?;
        // 超期请求体：全库体积的大头（实测 client_request 3.5 GB + kiro_request 1.9 GB），
        // 排障价值集中在头两三天。每轮有上界，靠 needs_follow_up 推进。
        let expired_bodies_stripped = self.prune_expired_request_bodies(now_epoch)?;
        if expired_bodies_stripped > 0 {
            tracing::info!(
                stripped_payloads = expired_bodies_stripped,
                retention_hours = REQUEST_BODY_RETENTION_SECS / 3600,
                "清理超期请求体（保留快照元数据与诊断分片）"
            );
        }
        // 历史 client_disconnected 请求体的回填清理。每轮有上界，靠 needs_follow_up
        // 驱动维护循环继续推进；清完后不再匹配候选条件，自然停下。
        let legacy_bodies_stripped = self.prune_legacy_client_disconnected_bodies()?;
        if legacy_bodies_stripped > 0 {
            tracing::info!(
                stripped_payloads = legacy_bodies_stripped,
                "清理历史 client_disconnected 请求体（保留元数据与 1% 采样）"
            );
        }
        // 放在所有删除之后：先产生空闲页，再回收。否则这一轮删掉的还得等下一轮。
        let reclaimed_pages = self.reclaim_free_pages()?;
        if reclaimed_pages > 0 {
            tracing::info!(reclaimed_pages, "回收快照库空闲页");
        }
        let status = self.storage_status()?;
        let (has_more_expired, has_capacity_candidates): (bool, bool) =
            self.conn.lock().query_row(
                "SELECT
                EXISTS(
                    SELECT 1 FROM error_snapshots
                    WHERE ts_epoch < ?1 AND pinned = 0 AND retention_exempt = 0
                      AND severity <> 'critical'
                ),
                EXISTS(
                    SELECT 1 FROM error_snapshots
                    WHERE pinned = 0 AND retention_exempt = 0
                      AND severity <> 'critical'
                )",
                params![cutoff],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
        let needs_follow_up = fallback_may_have_more
            || has_more_expired
            // 本轮确实清掉了历史请求体，说明可能还有——再来一轮。
            || legacy_bodies_stripped > 0
            || expired_bodies_stripped > 0
            // 还回收出了空闲页，说明 freelist 可能没啃完——再来一轮。
            || reclaimed_pages > 0
            || (status.live_bytes > target_bytes && has_capacity_candidates);
        let disk_pressure = status.available_bytes < policy.min_free_disk_bytes;
        self.capture_mode
            .store(status.capture_mode.as_u8(), Ordering::Release);
        Ok(MaintenanceReport {
            deleted,
            imported,
            disk_pressure,
            total_bytes: status.total_bytes,
            needs_follow_up,
        })
    }

    pub fn capture_mode(&self) -> CaptureMode {
        CaptureMode::from_u8(self.capture_mode.load(Ordering::Acquire))
    }

    /// 回填清理：把历史 `client_disconnected` 快照的请求体删掉，只留元数据。
    ///
    /// 写入侧的 1% 采样只拦**新**快照，已经落库的那批（线上 19088 条 / 3.2 GB）
    /// 不会自己消失——保留期到了才会整条过期。这一步按**同一个采样谓词**回填：
    /// 命中 1% 的保留请求体，其余只留 `tool_diagnostics` / `internal_error` 诊断分片。
    /// 复用同一谓词是为了让历史与新数据的采样口径一致，不出现两套行为。
    ///
    /// 顺带清掉这批快照上的 `stream_tail`：按新规则，尾部只对三类断流才存，
    /// `client_disconnected` 的尾部既无用又是正文。
    ///
    /// 三条安全性质：
    /// - `pinned` / `retention_exempt` 一律不动——那是运维显式保下来的证据；
    /// - 只删 payload 行，快照元数据（时间、模型、凭据、attempt 链）全部保留；
    /// - 每轮有 LIMIT 上界，靠维护循环分批推进，不在单次事务里啃 3 GB。
    ///
    /// 清完后这些快照不再匹配候选条件，因此该步骤天然自终止，可反复执行。
    fn prune_legacy_client_disconnected_bodies(&self) -> anyhow::Result<usize> {
        /// 每轮处理的快照条数上界。取 256：与 `MAINTENANCE_BATCH_SIZE` 同量级，
        /// 单次事务足够短，靠维护循环多跑几轮把历史啃完。
        const BATCH: usize = 256;

        let candidates: Vec<(String, String)> = {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT s.snapshot_id, s.trace_id FROM error_snapshots s
                   WHERE s.error_type = 'client_disconnected'
                     AND s.pinned = 0 AND s.retention_exempt = 0
                     AND EXISTS (
                         SELECT 1 FROM error_snapshot_payloads p
                          WHERE p.snapshot_id = s.snapshot_id
                            AND p.kind IN ('client_request', 'kiro_request',
                                           'upstream_response', 'stream_tail')
                     )
                   LIMIT ?1",
            )?;
            stmt.query_map(params![BATCH], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if candidates.is_empty() {
            return Ok(0);
        }

        // 采样命中的整条留着，作为「是不是我们太慢把客户逼断的」的排查素材。
        let strip: Vec<String> = candidates
            .into_iter()
            .filter(|(_, trace_id)| {
                !crate::anthropic::error_snapshot::client_disconnected_body_sampled(trace_id)
            })
            .map(|(snapshot_id, _)| snapshot_id)
            .collect();
        if strip.is_empty() {
            return Ok(0);
        }

        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut deleted = 0usize;
        for snapshot_id in &strip {
            deleted += tx.execute(
                "DELETE FROM error_snapshot_payloads
                   WHERE snapshot_id = ?1
                     AND kind IN ('client_request', 'kiro_request',
                                  'upstream_response', 'stream_tail')",
                params![snapshot_id],
            )?;
            // 聚合字段按剩余分片重算：Admin UI 直接展示这几个值，
            // 不重算会出现「分片数与字节数对不上」。
            tx.execute(
                "UPDATE error_snapshots SET
                     payload_count = (
                         SELECT COUNT(*) FROM error_snapshot_payloads p
                         WHERE p.snapshot_id = ?1
                     ),
                     original_bytes = COALESCE((
                         SELECT SUM(p.original_bytes) FROM error_snapshot_payloads p
                         WHERE p.snapshot_id = ?1
                     ), 0),
                     compressed_bytes = COALESCE((
                         SELECT SUM(p.compressed_bytes) FROM error_snapshot_payloads p
                         WHERE p.snapshot_id = ?1
                     ), 0)
                   WHERE snapshot_id = ?1",
                params![snapshot_id],
            )?;
        }
        tx.commit()?;
        Ok(deleted)
    }

    /// 清理超期的完整请求体，保留快照元数据与诊断分片。
    ///
    /// 见 [`REQUEST_BODY_RETENTION_SECS`]：这三类占全库体积的绝大部分，而它们的
    /// 排障价值集中在头两三天。删掉后快照本身还在——时间、模型、凭据、attempt 链、
    /// 错误类型、`tool_diagnostics` 全部保留，Admin UI 上仍能看到这条错误发生过、
    /// 长什么样，只是点不开原始请求体。
    ///
    /// 与 [`Self::prune_expired_stream_tails`] 的一处**刻意差异**：这里跳过
    /// `pinned` / `retention_exempt`。运维把一条快照钉住，要留的正是请求体；
    /// 连它一起删等于让那两个标记失去意义。
    ///
    /// 每轮按快照分批，不在单次事务里啃 2 GB——那会长时间持锁，把请求热路径上的
    /// 快照写入一起卡住。靠维护循环多跑几轮推进；清完后不再匹配候选条件，自然停下。
    fn prune_expired_request_bodies(&self, now_epoch: i64) -> anyhow::Result<usize> {
        /// 每轮处理的快照条数上界，与 `prune_legacy_client_disconnected_bodies` 同量级。
        const BATCH: usize = 256;

        let cutoff = now_epoch.saturating_sub(REQUEST_BODY_RETENTION_SECS);
        let candidates: Vec<String> = {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT DISTINCT s.snapshot_id
                   FROM error_snapshots s
                   JOIN error_snapshot_payloads p ON p.snapshot_id = s.snapshot_id
                  WHERE s.ts_epoch < ?1
                    AND s.pinned = 0 AND s.retention_exempt = 0
                    AND p.kind IN ('client_request', 'kiro_request', 'upstream_response')
                  LIMIT ?2",
            )?;
            stmt.query_map(params![cutoff, BATCH as i64], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if candidates.is_empty() {
            return Ok(0);
        }

        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut deleted = 0usize;
        for snapshot_id in &candidates {
            deleted += tx.execute(
                "DELETE FROM error_snapshot_payloads
                   WHERE snapshot_id = ?1
                     AND kind IN ('client_request', 'kiro_request', 'upstream_response')",
                params![snapshot_id],
            )?;
            // 聚合字段按剩余分片重算：Admin UI 直接展示这几个值，不重算会出现
            // 「分片数 2 但字节数还含已删请求体」这种自相矛盾的行。
            tx.execute(
                "UPDATE error_snapshots SET
                     payload_count = (
                         SELECT COUNT(*) FROM error_snapshot_payloads p
                         WHERE p.snapshot_id = ?1
                     ),
                     original_bytes = COALESCE((
                         SELECT SUM(p.original_bytes) FROM error_snapshot_payloads p
                         WHERE p.snapshot_id = ?1
                     ), 0),
                     compressed_bytes = COALESCE((
                         SELECT SUM(p.compressed_bytes) FROM error_snapshot_payloads p
                         WHERE p.snapshot_id = ?1
                     ), 0)
                   WHERE snapshot_id = ?1",
                params![snapshot_id],
            )?;
        }
        tx.commit()?;
        Ok(deleted)
    }

    /// 把已删数据留下的空闲页还给文件系统。
    ///
    /// 见 [`INCREMENTAL_VACUUM_PAGES`]：`auto_vacuum=INCREMENTAL` 只是允许回收，
    /// 不显式执行 `incremental_vacuum` 就永远不回收。返回本轮实际回收的页数，
    /// 供维护循环判断要不要再来一轮。
    ///
    /// 若建库时没开 incremental auto_vacuum（`PRAGMA auto_vacuum` 返回 0/1），
    /// 这条 pragma 是无害的空操作——不会报错，也不会阻塞，只是回收不了。
    fn reclaim_free_pages(&self) -> anyhow::Result<u64> {
        let conn = self.conn.lock();
        let before: u64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        if before == 0 {
            return Ok(0);
        }
        // 必须把这条 pragma **step 到底**：`incremental_vacuum` 每 step 只释放一页，
        // 参数是上限而非一次性批量。用 `execute_batch` 只会 step 一次——实测每轮就回收
        // 1 页（4 KiB），线上那 3.6 GB 要跑 87 万轮、约 61 小时，其间 `needs_follow_up`
        // 恒为真，维护循环被钉在 250 ms 空转，而日志还一直显示"回收成功"。
        {
            let mut stmt = conn.prepare(&format!(
                "PRAGMA incremental_vacuum({INCREMENTAL_VACUUM_PAGES})"
            ))?;
            let mut rows = stmt.query([])?;
            while rows.next()?.is_some() {}
        }
        // 库跑在 WAL 模式下：`incremental_vacuum` 先把收缩写进 WAL，主文件要等
        // checkpoint 才真的变小。不显式推一把就得等自动 checkpoint（默认攒够 1000 页），
        // 表现是"日志说回收了，`ls` 看文件没动"。
        //
        // 用 PASSIVE 而不是 TRUNCATE：PASSIVE 遇到活跃读者会直接放弃、绝不阻塞，
        // 而这条路径跑在持有连接锁的维护线程上，卡住等于卡住快照写入。
        // 这一轮没推完，下一轮还会再来。
        conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
        let after: u64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        Ok(before.saturating_sub(after))
    }

    /// 单独清理超期的 `stream_tail` payload，保留其余分片与快照元数据。
    ///
    /// `stream_tail` 存的是上游 event-stream 原始帧，里面就是**模型输出的明文**。
    /// 它对续写/断流排障有用，但排障基本在 1–2 天内完成，没有理由跟着快照库的
    /// 保留期躺 7 天。缩短这个窗口是「对话正文落盘」这件事最有效的缓解手段。
    ///
    /// 只删 payload 行、不删快照：断流的元数据（时间、模型、凭据、attempt 链）
    /// 仍然要能查到，丢的只是正文。
    fn prune_expired_stream_tails(&self, now_epoch: i64) -> anyhow::Result<usize> {
        let cutoff = now_epoch.saturating_sub(STREAM_TAIL_RETENTION_SECS);
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let deleted = tx.execute(
            "DELETE FROM error_snapshot_payloads
               WHERE kind = 'stream_tail'
                 AND snapshot_id IN (
                     SELECT snapshot_id FROM error_snapshots WHERE ts_epoch < ?1
                 )",
            params![cutoff],
        )?;
        if deleted > 0 {
            // payload 行删掉后，快照上的聚合字段会失真（Admin UI 直接展示这几个值），
            // 在同一事务里按剩余分片重算，避免出现「有 3 个分片但字节数含已删尾部」。
            tx.execute(
                "UPDATE error_snapshots SET
                     payload_count = (
                         SELECT COUNT(*) FROM error_snapshot_payloads p
                         WHERE p.snapshot_id = error_snapshots.snapshot_id
                     ),
                     original_bytes = COALESCE((
                         SELECT SUM(p.original_bytes) FROM error_snapshot_payloads p
                         WHERE p.snapshot_id = error_snapshots.snapshot_id
                     ), 0),
                     compressed_bytes = COALESCE((
                         SELECT SUM(p.compressed_bytes) FROM error_snapshot_payloads p
                         WHERE p.snapshot_id = error_snapshots.snapshot_id
                     ), 0)
                   WHERE ts_epoch < ?1",
                params![cutoff],
            )?;
        }
        tx.commit()?;
        Ok(deleted)
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
        let (sqlite_allocated_bytes, sqlite_live_bytes, reusable_bytes) =
            sqlite_page_metrics(&conn)?;
        let (records, pinned_records, critical_records): (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN pinned = 1 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN severity = 'critical' THEN 1 ELSE 0 END), 0)
             FROM error_snapshots",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let allocated_bytes = db_bytes
            .max(sqlite_allocated_bytes)
            .saturating_add(wal_bytes)
            .saturating_add(shm_bytes)
            .saturating_add(fallback_bytes);
        let live_bytes = sqlite_live_bytes
            .saturating_add(wal_bytes)
            .saturating_add(fallback_bytes);
        let capture_mode = capture_mode_for(
            live_bytes,
            policy.max_storage_bytes,
            available_bytes,
            policy.min_free_disk_bytes,
            policy.enabled,
        );
        self.capture_mode
            .store(capture_mode.as_u8(), Ordering::Release);
        Ok(StorageStatus {
            db_bytes,
            wal_bytes,
            shm_bytes,
            fallback_bytes,
            total_bytes,
            allocated_bytes,
            live_bytes,
            reusable_bytes,
            available_bytes,
            max_storage_bytes: policy.max_storage_bytes,
            min_free_disk_bytes: policy.min_free_disk_bytes,
            disk_pressure: available_bytes < policy.min_free_disk_bytes,
            records: u64::try_from(records)?,
            pinned_records: u64::try_from(pinned_records)?,
            critical_records: u64::try_from(critical_records)?,
            skipped_capacity: self.skipped_capacity.load(Ordering::Relaxed),
            capture_mode,
        })
    }

    pub fn recent_trace_links(&self, since_epoch: i64) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT trace_id, snapshot_id FROM error_snapshots
             WHERE ts_epoch >= ?1 ORDER BY ts_epoch DESC LIMIT ?2",
        )?;
        Ok(stmt
            .query_map(params![since_epoch, MAINTENANCE_BATCH_SIZE], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
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

fn sqlite_page_metrics(conn: &Connection) -> anyhow::Result<(u64, u64, u64)> {
    let page_count =
        u64::try_from(conn.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))?)?;
    let freelist_count =
        u64::try_from(conn.query_row("PRAGMA freelist_count", [], |row| row.get::<_, i64>(0))?)?;
    let page_size =
        u64::try_from(conn.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))?)?;
    let allocated_bytes = page_count.saturating_mul(page_size);
    let reusable_bytes = freelist_count.min(page_count).saturating_mul(page_size);
    let live_bytes = allocated_bytes.saturating_sub(reusable_bytes);
    Ok((allocated_bytes, live_bytes, reusable_bytes))
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

fn ensure_response_mode_column(conn: &Connection) -> rusqlite::Result<()> {
    let mut statement = conn.prepare("PRAGMA table_info(error_snapshots)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|name| name == "response_mode") {
        conn.execute_batch(
            "ALTER TABLE error_snapshots
             ADD COLUMN response_mode TEXT NOT NULL DEFAULT 'detection';",
        )?;
    }
    Ok(())
}

fn ensure_dedup_columns(conn: &Connection) -> rusqlite::Result<()> {
    let mut statement = conn.prepare("PRAGMA table_info(error_snapshots)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|name| name == "request_fingerprint") {
        conn.execute_batch(
            "ALTER TABLE error_snapshots
             ADD COLUMN request_fingerprint TEXT NOT NULL DEFAULT '';",
        )?;
    }
    if !columns.iter().any(|name| name == "duplicate_count") {
        conn.execute_batch(
            "ALTER TABLE error_snapshots
             ADD COLUMN duplicate_count INTEGER NOT NULL DEFAULT 1;",
        )?;
    }
    Ok(())
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
    ensure_response_mode_column(conn)?;
    ensure_dedup_columns(conn)?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_error_snapshots_dedup
         ON error_snapshots(request_fingerprint, error_type, response_mode, updated_at DESC);",
    )?;
    if version < SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    conn.pragma_update(None, "journal_mode", "WAL")?;
    Ok(())
}

fn summary_select() -> &'static str {
    "SELECT snapshot_id, trace_id, ts, model, is_stream, key_id, key_source,
            response_mode, final_credential_id, endpoint, http_status, final_status, error_type, severity,
            error_message, recovered, pinned, retention_exempt, omitted_due_to_disk_pressure,
            payload_count, original_bytes, compressed_bytes, created_at, updated_at, duplicate_count
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
        response_mode: row
            .get::<_, String>(7)?
            .parse()
            .unwrap_or(crate::admin::client_keys::ClientResponseMode::Detection),
        final_credential_id: from_u64(row.get::<_, i64>(8)?, 8)?,
        endpoint: row.get(9)?,
        http_status: row
            .get::<_, Option<i64>>(10)?
            .map(|value| u16::try_from(value).map_err(sql_range_error(10)))
            .transpose()?,
        final_status: row.get(11)?,
        error_type: row.get(12)?,
        severity: SnapshotSeverity::from_db(&row.get::<_, String>(13)?).map_err(|error| {
            sql_decode_error(
                13,
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })?,
        error_message: row.get(14)?,
        recovered: row.get(15)?,
        pinned: row.get(16)?,
        retention_exempt: row.get(17)?,
        omitted_due_to_disk_pressure: row.get(18)?,
        payload_count: from_u32(row.get::<_, i64>(19)?, 19)?,
        original_bytes: from_u64(row.get::<_, i64>(20)?, 20)?,
        compressed_bytes: from_u64(row.get::<_, i64>(21)?, 21)?,
        created_at: row.get(22)?,
        updated_at: row.get(23)?,
        duplicate_count: from_u64(row.get::<_, i64>(24)?, 24)?,
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
  request_fingerprint TEXT NOT NULL DEFAULT '',
  model TEXT NOT NULL,
  is_stream INTEGER NOT NULL,
  key_id INTEGER NOT NULL,
  key_source TEXT NOT NULL,
  response_mode TEXT NOT NULL DEFAULT 'detection',
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
  duplicate_count INTEGER NOT NULL DEFAULT 1,
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
            min_free_disk_bytes: 0,
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
            request_fingerprint: format!("request-fingerprint-{trace_id}"),
            ts: "2026-07-14T00:00:00Z".to_string(),
            ts_epoch: 1_752_451_200,
            model: "claude-opus-4-8".to_string(),
            is_stream: true,
            key_id: 7,
            key_source: crate::admin::trace_db::TraceKeySource::ClientKey,
            response_mode: crate::admin::client_keys::ClientResponseMode::KiroNative,
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

    /// 带一个 `stream_tail` 分片的快照，用于验证尾部的独立保留期。
    fn sample_write_with_stream_tail(
        snapshot_id: &str,
        trace_id: &str,
        ts_epoch: i64,
    ) -> SnapshotWrite {
        let mut write = sample_write(snapshot_id, trace_id);
        write.ts_epoch = ts_epoch;
        let mut tail = crate::anthropic::error_snapshot::encode_payload(
            crate::common::error_snapshot::SnapshotPayloadKind::StreamTail,
            None,
            "application/octet-stream",
            // 故意用非 UTF-8 字节：这正是线上尾部的形态。
            &[0x00u8, 0x00, 0x01, 0x2c, 0xff, 0xfe],
        )
        .unwrap();
        for part in &mut tail {
            part.seq = 2;
        }
        write.payloads.extend(tail);
        write
    }

    fn stream_tail_part_count(store: &ErrorSnapshotStore, snapshot_id: &str) -> i64 {
        store
            .conn
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM error_snapshot_payloads
                   WHERE snapshot_id = ?1 AND kind = 'stream_tail'",
                params![snapshot_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// `stream_tail` 里是模型输出明文，必须比快照库自身的保留期更早清掉，
    /// 且只清尾部——断流的元数据仍要能查。
    fn payload_count_of_kind(store: &ErrorSnapshotStore, snapshot_id: &str, kind: &str) -> i64 {
        store
            .conn
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM error_snapshot_payloads
                   WHERE snapshot_id = ?1 AND kind = ?2",
                params![snapshot_id, kind],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// 超期请求体被删、元数据与诊断分片留下；未超期的一动不动。
    ///
    /// 请求体是全库体积的大头（线上 client_request 3.5 GB + kiro_request 1.9 GB），
    /// 但排障价值集中在头两三天。删了之后这条错误在 Admin UI 上仍然看得到、
    /// 仍然知道它是什么错，只是点不开原始正文。
    #[test]
    fn expired_request_bodies_are_dropped_but_metadata_and_diagnostics_stay() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let now = 1_800_000_000i64;
        let stale_ts = now - 73 * 3600;
        let fresh_ts = now - 3600;

        let mut stale = sample_write("snap-stale", "t-stale");
        stale.ts_epoch = stale_ts;
        store.insert_with_fallback(&stale).unwrap();
        let mut fresh = sample_write("snap-fresh", "t-fresh");
        fresh.ts_epoch = fresh_ts;
        store.insert_with_fallback(&fresh).unwrap();

        assert_eq!(
            payload_count_of_kind(&store, "snap-stale", "client_request"),
            1
        );
        let deleted = store.prune_expired_request_bodies(now).unwrap();

        assert_eq!(deleted, 1, "只应删掉超过 72 小时的那一条请求体");
        assert_eq!(
            payload_count_of_kind(&store, "snap-stale", "client_request"),
            0
        );
        assert_eq!(
            payload_count_of_kind(&store, "snap-fresh", "client_request"),
            1,
            "72 小时内的请求体必须保留，那是排障主要素材"
        );
        assert_eq!(
            payload_count_of_kind(&store, "snap-stale", "internal_error"),
            1,
            "诊断分片体积很小，不跟着请求体一起删"
        );

        let (payload_count, error_type, compressed): (i64, String, i64) = store
            .conn
            .lock()
            .query_row(
                "SELECT payload_count, error_type, compressed_bytes
                   FROM error_snapshots WHERE snapshot_id = ?1",
                params!["snap-stale"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(error_type, "upstream_error", "元数据必须留下");
        assert_eq!(payload_count, 1, "聚合字段应按剩余分片重算（2 - 1 = 1）");
        assert!(
            compressed > 0,
            "重算后仍应反映剩余分片的体积，而不是清零或保留旧值"
        );
    }

    /// 钉住的快照连请求体一起保留——这正是 pinned 存在的意义。
    ///
    /// 与 `prune_expired_stream_tails` 的刻意差异：那边删的是模型输出明文，
    /// 缩短窗口本身就是目的；这边删的是排障素材，运维显式钉住就该留住。
    #[test]
    fn pinned_and_exempt_snapshots_keep_their_request_bodies() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let now = 1_800_000_000i64;
        let stale_ts = now - 100 * 3600;

        let mut pinned = sample_write("snap-pinned", "t-pinned");
        pinned.ts_epoch = stale_ts;
        pinned.pinned = true;
        store.insert_with_fallback(&pinned).unwrap();

        let mut exempt = sample_write("snap-exempt", "t-exempt");
        exempt.ts_epoch = stale_ts;
        exempt.retention_exempt = true;
        store.insert_with_fallback(&exempt).unwrap();

        let deleted = store.prune_expired_request_bodies(now).unwrap();

        assert_eq!(deleted, 0, "pinned / retention_exempt 的请求体一个都不能删");
        assert_eq!(
            payload_count_of_kind(&store, "snap-pinned", "client_request"),
            1
        );
        assert_eq!(
            payload_count_of_kind(&store, "snap-exempt", "client_request"),
            1
        );
    }

    /// 清理可反复执行：第二轮没有候选，返回 0 而不是再删一遍或报错。
    #[test]
    fn pruning_request_bodies_is_idempotent() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let now = 1_800_000_000i64;
        let mut stale = sample_write("snap-stale", "t-stale");
        stale.ts_epoch = now - 73 * 3600;
        store.insert_with_fallback(&stale).unwrap();

        assert_eq!(store.prune_expired_request_bodies(now).unwrap(), 1);
        assert_eq!(
            store.prune_expired_request_bodies(now).unwrap(),
            0,
            "清完后不该再有候选，否则维护循环会被 needs_follow_up 卡住空转"
        );
    }

    #[test]
    fn stream_tail_expires_before_snapshot_and_keeps_metadata() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let now = 1_800_000_000i64;
        // 一条 49 小时前（尾部应过期），一条 1 小时前（尾部应保留）。
        let stale_ts = now - 49 * 3600;
        let fresh_ts = now - 3600;
        store
            .insert_with_fallback(&sample_write_with_stream_tail(
                "snap-stale",
                "t-stale",
                stale_ts,
            ))
            .unwrap();
        store
            .insert_with_fallback(&sample_write_with_stream_tail(
                "snap-fresh",
                "t-fresh",
                fresh_ts,
            ))
            .unwrap();

        assert_eq!(stream_tail_part_count(&store, "snap-stale"), 1);
        let deleted = store.prune_expired_stream_tails(now).unwrap();

        assert_eq!(deleted, 1, "只应删掉超过 48 小时的那一条尾部");
        assert_eq!(stream_tail_part_count(&store, "snap-stale"), 0);
        assert_eq!(
            stream_tail_part_count(&store, "snap-fresh"),
            1,
            "48 小时内的尾部必须保留，否则续写排障没有素材"
        );

        // 快照本身与其余分片不能被带走。
        let (payload_count, error_type): (i64, String) = store
            .conn
            .lock()
            .query_row(
                "SELECT payload_count, error_type FROM error_snapshots WHERE snapshot_id = ?1",
                params!["snap-stale"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(error_type, "upstream_error", "元数据必须留下");
        assert_eq!(payload_count, 2, "聚合字段应按剩余分片重算（3 - 1 = 2）");
    }

    fn body_part_count(store: &ErrorSnapshotStore, snapshot_id: &str) -> i64 {
        store
            .conn
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM error_snapshot_payloads
                   WHERE snapshot_id = ?1
                     AND kind IN ('client_request', 'kiro_request',
                                  'upstream_response', 'stream_tail')",
                params![snapshot_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// 找一个采样**未**命中的 trace_id：这批才会被剥掉请求体。
    fn unsampled_trace_id() -> String {
        (0..10_000)
            .map(|i| format!("cd-trace-{i}"))
            .find(|id| !crate::anthropic::error_snapshot::client_disconnected_body_sampled(id))
            .expect("1% 采样率下必然存在未命中的 trace_id")
    }

    /// 找一个采样命中的 trace_id：这批要整条保留。
    fn sampled_trace_id() -> String {
        (0..100_000)
            .map(|i| format!("cd-trace-{i}"))
            .find(|id| crate::anthropic::error_snapshot::client_disconnected_body_sampled(id))
            .expect("1% 采样率下 10 万个样本必然有命中")
    }

    fn client_disconnected_write(snapshot_id: &str, trace_id: &str) -> SnapshotWrite {
        let mut write = sample_write(snapshot_id, trace_id);
        write.error_type = "client_disconnected".to_string();
        write
    }

    /// 写入侧采样只拦新快照，历史 3.2 GB 不会自己消失，所以要回填清理。
    /// 这条同时钉住三件事：非采样的剥 body、采样的整条保留、元数据不丢。
    #[test]
    fn legacy_client_disconnected_bodies_are_stripped_except_the_sample() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let unsampled = unsampled_trace_id();
        let sampled = sampled_trace_id();

        store
            .insert_with_fallback(&client_disconnected_write("snap-strip", &unsampled))
            .unwrap();
        store
            .insert_with_fallback(&client_disconnected_write("snap-keep", &sampled))
            .unwrap();
        // 非 client_disconnected 的快照一律不受影响。
        store
            .insert_with_fallback(&sample_write("snap-other", "trace-other"))
            .unwrap();

        assert_eq!(body_part_count(&store, "snap-strip"), 1);
        let stripped = store.prune_legacy_client_disconnected_bodies().unwrap();

        assert_eq!(stripped, 1, "只应剥掉采样未命中的那一条");
        assert_eq!(body_part_count(&store, "snap-strip"), 0);
        assert_eq!(
            body_part_count(&store, "snap-keep"),
            1,
            "采样命中的 1% 必须留住请求体，否则丢掉全部排查素材"
        );
        assert_eq!(
            body_part_count(&store, "snap-other"),
            1,
            "其它错误类型的快照不能被牵连"
        );

        // 元数据与诊断分片必须留下，且聚合字段按剩余分片重算。
        let (payload_count, error_type, model): (i64, String, String) = store
            .conn
            .lock()
            .query_row(
                "SELECT payload_count, error_type, model FROM error_snapshots
                   WHERE snapshot_id = ?1",
                params!["snap-strip"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(error_type, "client_disconnected");
        assert_eq!(model, "claude-opus-4-8", "元数据必须留下");
        assert_eq!(payload_count, 1, "剩下 internal_error 一个分片");

        // 幂等：清完不再匹配候选条件，重复执行是 no-op。
        assert_eq!(store.prune_legacy_client_disconnected_bodies().unwrap(), 0);
    }

    /// `pinned` / `retention_exempt` 是运维显式保下来的证据，回填清理不能碰。
    #[test]
    fn legacy_cleanup_never_touches_pinned_or_exempt_snapshots() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let unsampled = unsampled_trace_id();

        let mut pinned = client_disconnected_write("snap-pinned", &unsampled);
        pinned.pinned = true;
        store.insert_with_fallback(&pinned).unwrap();

        let mut exempt = client_disconnected_write("snap-exempt", "cd-trace-exempt");
        exempt.retention_exempt = true;
        store.insert_with_fallback(&exempt).unwrap();

        assert_eq!(store.prune_legacy_client_disconnected_bodies().unwrap(), 0);
        assert_eq!(body_part_count(&store, "snap-pinned"), 1);
        assert_eq!(body_part_count(&store, "snap-exempt"), 1);
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
        assert_eq!(
            page.records[0].response_mode,
            crate::admin::client_keys::ClientResponseMode::KiroNative
        );

        let detail = store.get("snap-1").unwrap().unwrap();
        assert_eq!(detail.payloads.len(), 2);
        assert!(detail.payloads.iter().all(|p| p.compressed_bytes > 0));

        let payload = store.read_payload("snap-1", 0).unwrap().unwrap();
        assert_eq!(payload.meta.content_type, "application/json");
        assert_eq!(payload.data, r#"{"request":"完整"}"#.as_bytes());
    }

    #[test]
    fn response_mode_migrates_v1_snapshot_schema_to_detection() {
        let conn = Connection::open_in_memory().unwrap();
        let legacy_schema =
            SCHEMA.replace("  response_mode TEXT NOT NULL DEFAULT 'detection',\n", "");
        assert_ne!(legacy_schema, SCHEMA);
        conn.execute_batch(&legacy_schema).unwrap();
        conn.execute(
            "INSERT INTO error_snapshots (
                snapshot_id, trace_id, ts, ts_epoch, model, is_stream, key_id, key_source,
                final_credential_id, endpoint, http_status, final_status, error_type, severity,
                error_message, recovered, pinned, retention_exempt, omitted_due_to_disk_pressure,
                payload_count, original_bytes, compressed_bytes, created_at, updated_at
             ) VALUES (
                'legacy-snapshot', 'legacy-trace', '2026-07-15T00:00:00Z', 1, 'm', 0, 7,
                'clientKey', 9, NULL, 500, 'error', 'legacy_error', 'error', NULL, 0, 0, 0,
                0, 0, 0, 0, 1, 1
             )",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        initialize_connection(&conn, false).unwrap();

        let columns = conn
            .prepare("PRAGMA table_info(error_snapshots)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(columns.iter().any(|name| name == "response_mode"));
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let response_mode: String = conn
            .query_row(
                "SELECT response_mode FROM error_snapshots WHERE snapshot_id = 'legacy-snapshot'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(response_mode, "detection");
    }

    #[test]
    fn response_mode_unknown_disk_value_falls_back_to_detection() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        store
            .insert(&sample_write("snap-unknown-mode", "trace-unknown-mode"))
            .unwrap();
        store
            .conn
            .lock()
            .execute(
                "UPDATE error_snapshots SET response_mode = 'future_mode' WHERE snapshot_id = 'snap-unknown-mode'",
                [],
            )
            .unwrap();

        let detail = store.get("snap-unknown-mode").unwrap().unwrap();
        assert_eq!(
            detail.summary.response_mode,
            crate::admin::client_keys::ClientResponseMode::Detection
        );
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
    fn duplicate_request_within_window_updates_count_without_replacing_payload() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let first = sample_write("snap-dedup-first", "trace-dedup-first");
        let mut second = sample_write("snap-dedup-second", "trace-dedup-second");
        second.request_fingerprint = first.request_fingerprint.clone();
        second.payloads.clear();

        assert_eq!(
            store.insert(&first).unwrap(),
            InsertOutcome::Inserted("snap-dedup-first".into())
        );
        assert_eq!(
            store.insert(&second).unwrap(),
            InsertOutcome::Existing("snap-dedup-first".into())
        );

        let detail = store.get("snap-dedup-first").unwrap().unwrap();
        assert_eq!(detail.summary.duplicate_count, 2);
        assert_eq!(detail.payloads.len(), 2);
        assert!(store.get("snap-dedup-second").unwrap().is_none());
    }

    #[test]
    fn duplicate_request_after_window_creates_new_snapshot() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let first = sample_write("snap-dedup-expired-first", "trace-dedup-expired-first");
        store.insert(&first).unwrap();
        store
            .conn
            .lock()
            .execute(
                "UPDATE error_snapshots SET updated_at = ?2 WHERE snapshot_id = ?1",
                params![
                    first.snapshot_id,
                    chrono::Utc::now().timestamp() - DEDUP_WINDOW_SECS - 1
                ],
            )
            .unwrap();

        let second = sample_write("snap-dedup-expired-second", "trace-dedup-expired-second");
        assert_eq!(
            store.insert(&second).unwrap(),
            InsertOutcome::Inserted("snap-dedup-expired-second".into())
        );
        assert_eq!(
            store.query_paged(&SnapshotQuery::default()).unwrap().total,
            2
        );
    }

    #[test]
    fn duplicate_request_does_not_merge_different_error_or_response_mode() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let first = sample_write("snap-dedup-type-first", "trace-dedup-type-first");
        store.insert(&first).unwrap();

        let mut different_error = sample_write("snap-dedup-type-second", "trace-dedup-type-second");
        different_error.request_fingerprint = first.request_fingerprint.clone();
        different_error.error_type = "different_error".to_string();
        assert!(matches!(
            store.insert(&different_error).unwrap(),
            InsertOutcome::Inserted(_)
        ));

        let mut different_mode = sample_write("snap-dedup-mode-second", "trace-dedup-mode-second");
        different_mode.request_fingerprint = first.request_fingerprint.clone();
        different_mode.response_mode = crate::admin::client_keys::ClientResponseMode::Detection;
        assert!(matches!(
            store.insert(&different_mode).unwrap(),
            InsertOutcome::Inserted(_)
        ));
        assert_eq!(
            store.query_paged(&SnapshotQuery::default()).unwrap().total,
            3
        );
    }

    /// 回收必须让**文件真的变小**，不能只是 freelist 计数下降。
    ///
    /// 这条测试用真实文件而非内存库，因为要验的正是 WAL 下的落地行为：
    /// `incremental_vacuum` 先把收缩写进 WAL，不 checkpoint 主文件不会动——
    /// 表现是"日志说回收了，`ls` 看文件没动"。线上正是 877,882 个空闲页
    /// （3.6 GB）一直占着文件，所以这里断言的是字节数，不是页数。
    #[test]
    fn reclaiming_free_pages_actually_shrinks_the_file() {
        let root =
            std::env::temp_dir().join(format!("kiro-error-snapshot-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join("error_snapshots.db");
        let store = ErrorSnapshotStore::open(db_path.clone(), root.join("fallback"), test_policy())
            .unwrap();

        // 写够量再删，才有可观的空闲页可回收。
        for index in 0..400 {
            let mut write = sample_write(&format!("snap-{index}"), &format!("trace-{index}"));
            write.payloads = crate::anthropic::error_snapshot::encode_payload(
                crate::common::error_snapshot::SnapshotPayloadKind::ClientRequest,
                None,
                "application/json",
                // zstd 压不动的随机内容，保证真的占页。
                format!("{index}{}", "x7Kq".repeat(4096)).as_bytes(),
            )
            .unwrap();
            store.insert(&write).unwrap();
        }
        store
            .conn
            .lock()
            .execute_batch("DELETE FROM error_snapshots;")
            .unwrap();
        store
            .conn
            .lock()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();

        let before_bytes = std::fs::metadata(&db_path).unwrap().len();
        let freelist: u64 = store
            .conn
            .lock()
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))
            .unwrap();
        assert!(freelist > 0, "删完应当产生空闲页，否则这条测试没有意义");

        // 一轮有上界（WAL 下还要靠 checkpoint 让页真正可回收），多跑几轮把 freelist 啃完。
        let mut rounds = 0;
        while rounds < 64 {
            rounds += 1;
            if store.reclaim_free_pages().unwrap() == 0 {
                break;
            }
        }
        let after_bytes = std::fs::metadata(&db_path).unwrap().len();

        assert!(
            rounds < 64,
            "回收必须收敛，否则维护循环会被 needs_follow_up 永久卡在空转"
        );
        assert!(
            after_bytes < before_bytes,
            "回收后文件必须真的变小：{before_bytes} -> {after_bytes}"
        );
        assert_eq!(
            store.reclaim_free_pages().unwrap(),
            0,
            "没有空闲页时应当直接返回 0，不做无谓的 vacuum"
        );

        drop(store);
        let _ = std::fs::remove_dir_all(&root);
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
        policy.max_storage_bytes = 1024 * 1024 * 1024;
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
    fn maintenance_deletes_at_most_one_bounded_batch() {
        let store = test_store_with_probe(50, 1_000);
        let mut policy = store.policy();
        policy.retention_days = 1;
        policy.max_storage_bytes = u64::MAX;
        policy.min_free_disk_bytes = 0;
        store.set_policy(policy);
        for index in 0..520 {
            insert_at(
                &store,
                &format!("old-{index}"),
                SnapshotSeverity::Warning,
                false,
                false,
                1,
            );
        }
        insert_at(
            &store,
            "pinned-batch",
            SnapshotSeverity::Warning,
            true,
            false,
            1,
        );
        insert_at(
            &store,
            "critical-batch",
            SnapshotSeverity::Critical,
            false,
            true,
            1,
        );

        let report = store.run_maintenance_at(100 * 86_400).unwrap();

        assert_eq!(report.deleted, 512);
        assert_eq!(
            store
                .query_paged(&SnapshotQuery {
                    limit: 1_000,
                    ..Default::default()
                })
                .unwrap()
                .total,
            10
        );
        assert!(store.get("pinned-batch").unwrap().is_some());
        assert!(store.get("critical-batch").unwrap().is_some());
    }

    #[test]
    fn maintenance_does_not_spin_when_only_protected_records_exceed_target() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        insert_at(
            &store,
            "critical-only",
            SnapshotSeverity::Critical,
            false,
            true,
            1,
        );
        let mut policy = store.policy();
        policy.retention_days = 1;
        policy.max_storage_bytes = 1;
        policy.min_free_disk_bytes = 0;
        store.set_policy(policy);

        let report = store.run_maintenance_at(100 * 86_400).unwrap();

        assert_eq!(report.deleted, 0);
        assert!(!report.needs_follow_up);
        assert!(store.get("critical-only").unwrap().is_some());
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
    fn capacity_thresholds_preserve_critical_diagnostics_before_disabling_capture() {
        assert_eq!(
            capture_mode_for(79, 100, 1_000, 100, true),
            CaptureMode::Full
        );
        assert_eq!(
            capture_mode_for(80, 100, 1_000, 100, true),
            CaptureMode::CriticalOnly
        );
        assert_eq!(
            capture_mode_for(90, 100, 1_000, 100, true),
            CaptureMode::MetadataOnly
        );
        assert_eq!(
            capture_mode_for(100, 100, 1_000, 100, true),
            CaptureMode::Disabled
        );
        assert_eq!(
            capture_mode_for(10, 100, 99, 100, true),
            CaptureMode::MetadataOnly
        );
        assert_eq!(
            capture_mode_for(0, 100, 1_000, 100, false),
            CaptureMode::Disabled
        );
    }

    #[test]
    fn hard_capacity_skip_never_writes_fallback() {
        let fallback = temp_path("snapshot-capacity-fallback");
        let mut policy = test_policy();
        policy.max_storage_bytes = 1;
        policy.min_free_disk_bytes = 0;
        let store =
            ErrorSnapshotStore::open_in_memory_with_fallback(fallback.clone(), policy).unwrap();

        let outcome = store
            .insert_with_fallback(&sample_write("snap-full", "trace-full"))
            .unwrap();

        assert_eq!(outcome, InsertOutcome::SkippedCapacity);
        assert!(!fallback.exists());
    }

    #[test]
    fn prospective_write_cannot_jump_over_hard_capacity() {
        let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
        let live_bytes = store.storage_status().unwrap().live_bytes;
        let mut policy = store.policy();
        policy.max_storage_bytes = live_bytes.saturating_add(32 * 1024);
        policy.min_free_disk_bytes = 0;
        store.set_policy(policy);

        let outcome = store
            .insert_with_fallback(&sample_write("snap-jump", "trace-jump"))
            .unwrap();

        assert_eq!(outcome, InsertOutcome::SkippedCapacity);
        assert!(store.get("snap-jump").unwrap().is_none());
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

    #[test]
    fn legacy_fallback_snapshot_without_response_mode_defaults_to_detection() {
        let write = sample_write("snap-legacy-mode", "trace-legacy-mode");
        let mut value = serde_json::to_value(write).unwrap();
        value.as_object_mut().unwrap().remove("response_mode");

        let restored: SnapshotWrite = serde_json::from_value(value).unwrap();

        assert_eq!(
            restored.response_mode,
            crate::admin::client_keys::ClientResponseMode::Detection
        );
    }
}
