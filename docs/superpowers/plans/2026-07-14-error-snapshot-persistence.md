# 错误快照持久化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为所有失败、中断和重试恢复请求保存可按 trace ID 定位的完整脱敏现场，并在 Admin UI 中提供查询、查看、下载、pin/unpin、清理和容量治理能力。

**Architecture:** 保留 `traces.db` 作为轻量请求链路主索引，新增独立的 `error_snapshots.db` 保存 zstd 压缩后的脱敏 payload；`RequestTracer` 继续作为请求关联主线，并把 provider、非流式解析器和流式状态机产生的诊断事件汇聚到请求级 `ErrorSnapshotContext`。快照先提交，随后把 `snapshot_id` 写进 trace；数据库失败时把同一份已脱敏、已压缩 envelope 原子写入 fallback 目录，后台任务再幂等导入。

**Tech Stack:** Rust 2024、Axum 0.8、Tokio、rusqlite/SQLite WAL、serde_json、zstd、SHA-256、fs2、React 19、TypeScript 6、TanStack Query、Axios、Bun、Docker BuildKit、8991 隔离测试容器。

---

## 实施边界与文件职责

- `src/common/error_snapshot.rs`：存储层和协议层共享的 payload kind 与编码分片类型，不包含业务逻辑。
- `src/anthropic/error_snapshot.rs`：请求内采集、触发判定、字段级脱敏、base64 摘要、UTF-8 安全分片、SHA-256 与 zstd 编码；不直接拼 SQL。
- `src/admin/error_snapshot_db.rs`：快照数据库 schema、事务写入、分页查询、按需解压、pin/delete、生命周期、磁盘压力、fallback 写入与导入；不理解 Anthropic/Kiro 协议。
- `src/admin/trace_db.rs`：仅增加 `snapshot_id` 轻量关联和诊断事件接口，不保存大正文。
- `src/anthropic/handlers.rs`：在请求入口创建快照上下文，在已有成功/失败/中断收口点调用幂等 `finalize`，记录非流式 body 和流式尾部。
- `src/kiro/provider.rs`：在每次真实上游请求前后通过 `TraceSink` 上报出站 JSON、HTTP 错误体和网络错误，不访问 SQLite。
- `src/admin/{types,service,handlers,router,middleware}.rs`：运行时治理配置与 Admin API。
- `src/main.rs`、`src/anthropic/{middleware,router}.rs`：初始化共享 store、注入业务/Admin 路由、启动维护任务。
- `admin-ui/src/api/error-snapshots.ts`：快照 HTTP API 和下载 Blob。
- `admin-ui/src/hooks/use-error-snapshots.ts`：分页、详情、payload、存储状态和 mutation hooks。
- `admin-ui/src/components/error-snapshot-page.tsx`：快照列表、筛选、容量状态和治理入口。
- `admin-ui/src/components/error-snapshot-dialog.tsx`：懒加载详情、复制、下载、pin/unpin、删除。
- `admin-ui/src/components/trace-log-page.tsx`：有 `snapshotId` 的 trace 提供“查看快照”入口，并扩展日志治理配置。
- `admin-ui/src/App.tsx`：新增“错误快照”顶级页签，确保孤立或尚未回链的快照也可浏览。
- `scripts/error-snapshot-smoke.sh`：8991 黑盒验收，不输出 API Key 或快照正文。

## 不可破坏的约束

1. 不保存 Authorization、Proxy-Authorization、x-api-key、API Key、access/refresh/id token、client secret、Cookie、password、credential、secret 字段值。
2. 图片、PDF、data URI 和严格识别出的超长 base64 只保存长度与 SHA-256；普通正文中的 `token`、`key` 单词和短字符串不做全局替换。
3. 正常单跳成功请求不压缩、不写大 BLOB；快照失败不得改变原 HTTP 状态、SSE 事件或错误正文。
4. `critical` 和手动 pin 永不参加自动清理；低于 100GB 空闲空间时只能降级新快照，不得删除它们。
5. 单个未压缩分片上限 16 MiB；解压必须先检查声明长度并设置硬上限，禁止无界分配。
6. 生产 8990 不在本计划中部署；完成后只在独立 8991 验收。

### Task 1: 增加依赖与持久配置模型

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/model/config.rs`

- [ ] **Step 1: 先写配置默认值和 camelCase 往返失败测试**

在 `src/model/config.rs` 现有 `#[cfg(test)] mod tests` 中加入：

```rust
#[test]
fn error_snapshot_defaults_are_safe_and_round_trip_in_camel_case() {
    let defaulted: Config = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(defaulted.error_snapshot_enabled);
    assert_eq!(defaulted.error_snapshot_retention_days, 90);
    assert_eq!(defaulted.error_snapshot_max_storage_gb, 200);
    assert!(defaulted.error_snapshot_capture_recovered);
    assert!(defaulted.error_snapshot_capture_bodies);
    assert_eq!(defaulted.error_snapshot_min_free_disk_gb, 100);

    let custom: Config = serde_json::from_value(serde_json::json!({
        "errorSnapshotEnabled": false,
        "errorSnapshotRetentionDays": 30,
        "errorSnapshotMaxStorageGb": 64,
        "errorSnapshotCaptureRecovered": false,
        "errorSnapshotCaptureBodies": false,
        "errorSnapshotMinFreeDiskGb": 32
    }))
    .unwrap();
    let encoded = serde_json::to_value(custom).unwrap();
    assert_eq!(encoded["errorSnapshotEnabled"], false);
    assert_eq!(encoded["errorSnapshotRetentionDays"], 30);
    assert_eq!(encoded["errorSnapshotMaxStorageGb"], 64);
    assert_eq!(encoded["errorSnapshotCaptureRecovered"], false);
    assert_eq!(encoded["errorSnapshotCaptureBodies"], false);
    assert_eq!(encoded["errorSnapshotMinFreeDiskGb"], 32);
}
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `cargo test error_snapshot_defaults_are_safe_and_round_trip_in_camel_case`

Expected: FAIL，提示 `Config` 不存在 `error_snapshot_*` 字段。

- [ ] **Step 3: 增加依赖和六个配置字段**

在 `Cargo.toml` 增加：

```toml
zstd = "0.13"
fs2 = "0.4"

[dev-dependencies]
tower = { version = "0.5", features = ["util"] }
```

在 `Config` 中紧跟现有 trace/usage 治理字段加入：

```rust
#[serde(default = "default_true")]
pub error_snapshot_enabled: bool,
#[serde(default = "default_error_snapshot_retention_days")]
pub error_snapshot_retention_days: u32,
#[serde(default = "default_error_snapshot_max_storage_gb")]
pub error_snapshot_max_storage_gb: u64,
#[serde(default = "default_true")]
pub error_snapshot_capture_recovered: bool,
#[serde(default = "default_true")]
pub error_snapshot_capture_bodies: bool,
#[serde(default = "default_error_snapshot_min_free_disk_gb")]
pub error_snapshot_min_free_disk_gb: u64,
```

加入默认函数并写入 `impl Default for Config`：

```rust
fn default_error_snapshot_retention_days() -> u32 { 90 }
fn default_error_snapshot_max_storage_gb() -> u64 { 200 }
fn default_error_snapshot_min_free_disk_gb() -> u64 { 100 }

// Config::default()
error_snapshot_enabled: true,
error_snapshot_retention_days: default_error_snapshot_retention_days(),
error_snapshot_max_storage_gb: default_error_snapshot_max_storage_gb(),
error_snapshot_capture_recovered: true,
error_snapshot_capture_bodies: true,
error_snapshot_min_free_disk_gb: default_error_snapshot_min_free_disk_gb(),
```

- [ ] **Step 4: 运行配置测试并刷新 lockfile**

Run: `cargo test error_snapshot_defaults_are_safe_and_round_trip_in_camel_case`

Expected: PASS；`Cargo.lock` 包含 `zstd`、`zstd-safe`、`zstd-sys` 和 `fs2`。

- [ ] **Step 5: 提交配置切片**

```bash
git add -- Cargo.toml Cargo.lock src/model/config.rs
git commit -m "feat(logging): 增加错误快照配置"
```

### Task 2: 实现脱敏、分片、哈希与压缩编码

**Files:**
- Create: `src/common/error_snapshot.rs`
- Modify: `src/common/mod.rs`
- Create: `src/anthropic/error_snapshot.rs`
- Modify: `src/anthropic/mod.rs`

- [ ] **Step 1: 写秘密字段、base64、UTF-8 分片和压缩往返测试**

在新文件末尾建立测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_auth_fields_but_preserves_customer_text_and_tool_json() {
        let value = serde_json::json!({
            "headers": {"Authorization": "Bearer secret", "anthropic-version": "2023-06-01"},
            "refreshToken": "refresh-secret",
            "messages": [{"role": "user", "content": "explain token and key rotation"}],
            "tool": {"name": "lookup", "input": {"key": "ordinary-business-key"}}
        });
        let sanitized = sanitize_json(value);
        assert_eq!(sanitized["headers"]["Authorization"], "[REDACTED]");
        assert_eq!(sanitized["refreshToken"], "[REDACTED]");
        assert_eq!(sanitized["messages"][0]["content"], "explain token and key rotation");
        assert_eq!(sanitized["tool"]["input"]["key"], "ordinary-business-key");
    }

    #[test]
    fn replaces_known_binary_and_long_strict_base64_with_digest() {
        let raw = vec![0x5a; 8192];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        let sanitized = sanitize_json(serde_json::json!({
            "source": {"type": "base64", "media_type": "application/pdf", "data": encoded},
            "shortToolValue": "YWJj"
        }));
        assert_eq!(sanitized["source"]["data"]["redacted_base64"], true);
        assert_eq!(sanitized["source"]["data"]["original_bytes"], 8192);
        assert_eq!(sanitized["source"]["data"]["sha256"].as_str().unwrap().len(), 64);
        assert_eq!(sanitized["shortToolValue"], "YWJj");
    }

    #[test]
    fn chunks_utf8_without_cutting_characters_and_round_trips_zstd() {
        let input = "错误现场-".repeat(2_000_000);
        let chunks = split_utf8(input.as_bytes(), MAX_UNCOMPRESSED_PART_BYTES);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|part| std::str::from_utf8(part).is_ok()));
        let rebuilt = chunks.concat();
        assert_eq!(rebuilt, input.as_bytes());

        let encoded = encode_payload(SnapshotPayloadKind::ClientRequest, None, "application/json", input.as_bytes()).unwrap();
        let decoded = decode_payload_parts(&encoded, input.len()).unwrap();
        assert_eq!(decoded, input.as_bytes());
    }

    #[test]
    fn rejects_decompression_larger_than_declared_limit() {
        let input = vec![b'x'; 1024];
        let encoded = encode_payload(SnapshotPayloadKind::InternalError, None, "text/plain", &input).unwrap();
        let error = decode_payload_parts(&encoded, 128).unwrap_err();
        assert!(error.to_string().contains("解压上限"));
    }
}
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `cargo test anthropic::error_snapshot::tests -- --nocapture`

Expected: FAIL，模块和函数尚不存在。

- [ ] **Step 3: 定义快照采集与编码类型**

在 `src/common/mod.rs` 增加 `pub mod error_snapshot;`，在 `src/anthropic/mod.rs` 增加 `pub(crate) mod error_snapshot;`。共享类型放入 `src/common/error_snapshot.rs`：

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotPayloadKind {
    ClientRequest,
    KiroRequest,
    UpstreamResponse,
    ToolDiagnostics,
    StreamTail,
    InternalError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodedPayloadPart {
    pub seq: u32,
    pub kind: SnapshotPayloadKind,
    pub attempt: Option<u32>,
    pub codec: String,
    pub content_type: String,
    pub part_index: u32,
    pub part_count: u32,
    pub original_bytes: u64,
    pub sha256: String,
    pub data: Vec<u8>,
}
```

采集和编码模块导入共享类型，并定义以下稳定接口：

```rust
pub use crate::common::error_snapshot::{EncodedPayloadPart, SnapshotPayloadKind};

pub const MAX_UNCOMPRESSED_PART_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_DECOMPRESSED_PAYLOAD_BYTES: usize = 128 * 1024 * 1024;
const LONG_BASE64_THRESHOLD: usize = 4096;
const ZSTD_LEVEL: i32 = 3;

pub fn sanitize_json(mut value: serde_json::Value) -> serde_json::Value;
pub fn split_utf8(input: &[u8], limit: usize) -> Vec<Vec<u8>>;
pub fn encode_payload(
    kind: SnapshotPayloadKind,
    attempt: Option<u32>,
    content_type: &str,
    input: &[u8],
) -> anyhow::Result<Vec<EncodedPayloadPart>>;
pub fn decode_payload_parts(parts: &[EncodedPayloadPart], max_output: usize) -> anyhow::Result<Vec<u8>>;
```

秘密字段匹配使用 ASCII 大小写不敏感的规范名集合：

```rust
fn is_secret_field(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().replace(['-', '_'], "").as_str(),
        "authorization" | "proxyauthorization" | "xapikey" | "apikey" |
        "adminapikey" | "accesstoken" | "refreshtoken" | "idtoken" |
        "clientsecret" | "cookie" | "setcookie" | "password" |
        "credential" | "credentials" | "secret"
    )
}
```

仅当字段本身是秘密字段时替换；`tool.input.key`、普通正文中的 `token/key` 保持不变。已知二进制条件为父对象 `type == "base64"` 且字段名为 `data`，或字符串长度不小于 4096、长度可被 4 整除并能通过严格 base64 解码。摘要对象固定为：

```rust
serde_json::json!({
    "redacted_base64": true,
    "original_bytes": decoded.len(),
    "sha256": hex::encode(sha2::Sha256::digest(&decoded)),
})
```

分片时从 `min(start + limit, len)` 向前寻找 UTF-8 字符边界；每片独立 zstd level 3 压缩，所有片共享完整逻辑 payload 的 SHA-256，`original_bytes` 保存当前分片解压后的字节数，`seq` 在调用方合并 payload 时按逻辑 payload 重新编号。

- [ ] **Step 4: 运行编码测试**

Run: `cargo test anthropic::error_snapshot::tests -- --nocapture`

Expected: 4 tests PASS；中文长文本无 UTF-8 截断，超限解压被拒绝。

- [ ] **Step 5: 提交编码切片**

```bash
git add -- src/common/mod.rs src/common/error_snapshot.rs src/anthropic/mod.rs src/anthropic/error_snapshot.rs
git commit -m "feat(logging): 实现错误快照脱敏压缩"
```

### Task 3: 建立独立快照数据库和基础 CRUD

**Files:**
- Create: `src/admin/error_snapshot_db.rs`
- Modify: `src/admin/mod.rs`

- [ ] **Step 1: 写 schema、原子写入、分页不读取 BLOB 和按需解压测试**

测试使用 `ErrorSnapshotStore::open_in_memory(policy())`，构造一条两个 payload 的 `SnapshotWrite`：

```rust
#[test]
fn inserts_snapshot_and_payloads_atomically_and_lists_without_blob_data() {
    let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
    let write = sample_write("snap-1", "trace-1");
    store.insert(&write).unwrap();

    let page = store.query_paged(&SnapshotQuery { limit: 50, ..Default::default() }).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.records[0].snapshot_id, "snap-1");
    assert_eq!(page.records[0].payload_count, 2);

    let detail = store.get("snap-1").unwrap().unwrap();
    assert_eq!(detail.payloads.len(), 2);
    assert!(detail.payloads.iter().all(|p| p.compressed_bytes > 0));

    let payload = store.read_payload("snap-1", 0).unwrap().unwrap();
    assert_eq!(payload.content_type, "application/json");
    assert_eq!(payload.data, br#"{"request":"完整"}"#);
}

#[test]
fn duplicate_trace_id_is_idempotent() {
    let store = ErrorSnapshotStore::open_in_memory(test_policy()).unwrap();
    let first = sample_write("snap-1", "trace-1");
    let second = sample_write("snap-2", "trace-1");
    assert_eq!(store.insert(&first).unwrap(), InsertOutcome::Inserted("snap-1".into()));
    assert_eq!(store.insert(&second).unwrap(), InsertOutcome::Existing("snap-1".into()));
}
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `cargo test admin::error_snapshot_db::tests -- --nocapture`

Expected: FAIL，模块尚不存在。

- [ ] **Step 3: 定义 store、查询和返回类型**

在 `src/admin/mod.rs` 增加：

```rust
pub mod error_snapshot_db;
pub use error_snapshot_db::{ErrorSnapshotStore, SharedErrorSnapshotStore};
```

新模块公开以下接口：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotSeverity { Critical, Error, Warning, Info }

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
    pub payloads: Vec<crate::common::error_snapshot::EncodedPayloadPart>,
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
    pub kind: crate::common::error_snapshot::SnapshotPayloadKind,
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
    conn: parking_lot::Mutex<rusqlite::Connection>,
    db_path: Option<std::path::PathBuf>,
    fallback_dir: Option<std::path::PathBuf>,
    policy: parking_lot::RwLock<ErrorSnapshotPolicy>,
    disk_pressure: std::sync::atomic::AtomicBool,
}

pub type SharedErrorSnapshotStore = std::sync::Arc<ErrorSnapshotStore>;
```

- [ ] **Step 4: 创建 schema 和事务 API**

使用以下表结构；数据库打开时执行 `journal_mode=WAL`、`synchronous=NORMAL`、`foreign_keys=ON` 和 2 秒 `busy_timeout`，新文件在建表前设置 `auto_vacuum=INCREMENTAL`：

```sql
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
```

实现：

```rust
pub fn open(path: PathBuf, fallback_dir: PathBuf, policy: ErrorSnapshotPolicy) -> rusqlite::Result<Self>;
pub fn open_in_memory(policy: ErrorSnapshotPolicy) -> rusqlite::Result<Self>;
pub fn policy(&self) -> ErrorSnapshotPolicy;
pub fn set_policy(&self, policy: ErrorSnapshotPolicy);
pub fn insert(&self, write: &SnapshotWrite) -> anyhow::Result<InsertOutcome>;
pub fn query_paged(&self, query: &SnapshotQuery) -> anyhow::Result<SnapshotPage>;
pub fn get(&self, id: &str) -> anyhow::Result<Option<SnapshotDetail>>;
pub fn read_payload(&self, id: &str, logical_seq: u32) -> anyhow::Result<Option<DecodedPayload>>;
pub fn set_pinned(&self, id: &str, pinned: bool) -> anyhow::Result<bool>;
pub fn delete(&self, id: &str) -> anyhow::Result<bool>;
```

`migrate` 使用 `PRAGMA user_version`，首版设为 1；重复打开数据库必须幂等，发现未来更高版本时拒绝写入并返回明确错误。列表 SQL 只查 `error_snapshots`；详情只查 payload 元数据；同一逻辑 payload 的所有分片共享 `seq`，`payload_count` 使用不同 `seq` 的数量，摘要的 `original_bytes/compressed_bytes` 按分片求和。`read_payload` 在锁内只复制压缩分片，释放 SQLite mutex 后再逐片解压、校验完整 SHA-256 并重组，避免大 payload 解压阻塞新错误写入。

- [ ] **Step 5: 运行数据库测试**

Run: `cargo test admin::error_snapshot_db::tests -- --nocapture`

Expected: PASS；重复 `trace_id` 返回既有快照 ID，不产生第二条记录。

- [ ] **Step 6: 提交数据库基础切片**

```bash
git add -- src/admin/mod.rs src/admin/error_snapshot_db.rs
git commit -m "feat(logging): 建立错误快照数据库"
```

### Task 4: 实现 fallback、90 天保留、200GB 上限和磁盘压力降级

**Files:**
- Modify: `src/admin/error_snapshot_db.rs`

- [ ] **Step 1: 写生命周期和 fallback RED 测试**

加入可注入文件大小/空闲空间的测试 seam：

```rust
trait StorageProbe: Send + Sync {
    fn available_bytes(&self, path: &std::path::Path) -> std::io::Result<u64>;
    fn tree_bytes(&self, paths: &[std::path::PathBuf]) -> std::io::Result<u64>;
}
```

测试固定容量，不创建真实巨型文件：

```rust
#[test]
fn cleanup_never_deletes_pinned_or_critical_records() {
    let store = test_store_with_probe(50, 1_000);
    insert_at(&store, "warning-old", SnapshotSeverity::Warning, false, false, 1);
    insert_at(&store, "error-old", SnapshotSeverity::Error, false, false, 2);
    insert_at(&store, "pinned", SnapshotSeverity::Warning, true, false, 3);
    insert_at(&store, "critical", SnapshotSeverity::Critical, false, true, 4);
    let report = store.run_maintenance_at(100 * 86_400).unwrap();
    assert!(report.deleted >= 2);
    assert!(store.get("pinned").unwrap().is_some());
    assert!(store.get("critical").unwrap().is_some());
}

#[test]
fn low_free_space_enters_metadata_only_mode() {
    let store = test_store_with_probe(10_000, 99);
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
}
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `cargo test admin::error_snapshot_db::tests -- --nocapture`

Expected: 新增 3 个测试 FAIL，缺少维护、探针和 fallback API。

- [ ] **Step 3: 实现原子 fallback envelope**

fallback 文件名固定为 `<snapshot_id>.snapshot.zst`。内部 JSON envelope 的 payload `data` 使用 base64 表示“已脱敏、已 zstd 压缩的数据库 BLOB”，不是原始附件；整个 JSON 再 zstd 压缩。写入流程固定为：

```rust
let temp = dir.join(format!(".{}.{}.tmp", write.snapshot_id, uuid::Uuid::new_v4()));
let final_path = dir.join(format!("{}.snapshot.zst", write.snapshot_id));
std::fs::write(&temp, zstd::stream::encode_all(serialized.as_slice(), 3)?)?;
std::fs::rename(&temp, &final_path)?;
```

`insert_with_fallback` 先以 25ms、75ms、150ms 三次有限重试处理 `DatabaseBusy/DatabaseLocked`；仍失败时写 fallback，并仅记录 `snapshot_id`、`trace_id` 和错误类型，不打印正文。此任务同时增加：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureMode { Full, MetadataOnly }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FallbackImportReport { pub imported: usize, pub existing: usize, pub failed: usize }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceReport { pub deleted: usize, pub imported: usize, pub disk_pressure: bool, pub total_bytes: u64 }

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

pub fn open_in_memory_with_fallback(
    fallback_dir: PathBuf,
    policy: ErrorSnapshotPolicy,
) -> rusqlite::Result<Self>;
pub fn insert_with_fallback(&self, write: &SnapshotWrite) -> anyhow::Result<InsertOutcome>;
pub fn import_fallback(&self) -> anyhow::Result<FallbackImportReport>;
pub fn import_fallback_dir(&self, dir: &Path) -> anyhow::Result<FallbackImportReport>;
pub fn run_maintenance(&self) -> anyhow::Result<MaintenanceReport>;
pub fn run_maintenance_at(&self, now_epoch: i64) -> anyhow::Result<MaintenanceReport>;
pub fn capture_mode(&self) -> CaptureMode;
pub fn storage_status(&self) -> anyhow::Result<StorageStatus>;
pub fn recent_trace_links(&self, since_epoch: i64) -> anyhow::Result<Vec<(String, String)>>;
```

真实 `StorageProbe` 使用 `fs2::available_space` 和递归文件元数据求和；`ErrorSnapshotStore` 在本任务增加 `storage_probe: Arc<dyn StorageProbe>` 字段，生产构造器注入真实 probe，测试构造器注入固定值。fallback 序列化使用私有 `FallbackPayloadPart { meta, data_b64 }`，其中 `data_b64` 由 `STANDARD.encode(&part.data)` 生成；导入时严格解码并重新校验 compressed length。成功插入或命中既有 `trace_id` 后删除 fallback 文件；无法解码或校验的文件移动到 `error-snapshot-fallback/corrupt/`，不自动删除。

- [ ] **Step 4: 实现维护顺序和磁盘压力状态**

维护流程固定为：

```text
导入 fallback
→ 删除过期 warning
→ 删除过期 error
→ 删除过期 info
→ 若总占用仍高于 max_storage，按最旧 warning/error/info 继续删除
→ WAL checkpoint(TRUNCATE)
→ PRAGMA incremental_vacuum(4096)
→ 重新测量 DB/WAL/SHM/fallback 和可用空间
→ available < min_free 时设置 MetadataOnly，否则 Full
```

所有删除 SQL 必须包含 `pinned = 0 AND retention_exempt = 0 AND severity <> 'critical'`。`StorageStatus` 返回 `db_bytes`、`wal_bytes`、`shm_bytes`、`fallback_bytes`、`total_bytes`、`available_bytes`、`max_storage_bytes`、`min_free_disk_bytes`、`disk_pressure`、`records`、`pinned_records`、`critical_records`。

- [ ] **Step 5: 运行维护测试**

Run: `cargo test admin::error_snapshot_db::tests -- --nocapture`

Expected: PASS；pin/critical 保留，低空间进入 metadata-only，fallback 导入幂等。

- [ ] **Step 6: 提交生命周期切片**

```bash
git add -- src/admin/error_snapshot_db.rs
git commit -m "feat(logging): 增加快照容量治理与兜底"
```

### Task 5: 给 traces.db 增加 snapshotId 关联

**Files:**
- Modify: `src/admin/trace_db.rs`
- Modify: `src/admin/handlers.rs`
- Modify: `admin-ui/src/types/api.ts`

- [ ] **Step 1: 写旧库迁移和查询关联测试**

在 `src/admin/trace_db.rs` 测试中加入：

```rust
#[test]
fn migrates_snapshot_id_and_round_trips_it() {
    let store = TraceStore::open_in_memory().unwrap();
    let mut rec = sample_record("trace-with-snapshot", "error", 7, TraceKeySource::ClientKey);
    rec.snapshot_id = Some("snap-7".into());
    store.insert(&rec);
    let out = store.query(&TraceQuery { limit: 10, ..Default::default() });
    assert_eq!(out[0].snapshot_id.as_deref(), Some("snap-7"));
}

#[test]
fn links_existing_trace_idempotently() {
    let store = TraceStore::open_in_memory().unwrap();
    let rec = sample_record("trace-link", "error", 7, TraceKeySource::ClientKey);
    store.insert(&rec);
    assert!(store.link_snapshot("trace-link", "snap-link"));
    assert!(store.link_snapshot("trace-link", "snap-link"));
    assert_eq!(store.query(&TraceQuery { limit: 10, ..Default::default() })[0].snapshot_id.as_deref(), Some("snap-link"));
}
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `cargo test admin::trace_db::tests -- --nocapture`

Expected: FAIL，`TraceRecord.snapshot_id` 和 `link_snapshot` 不存在。

- [ ] **Step 3: 实现 trace 关联**

给 `TraceRecord` 增加：

```rust
#[serde(default)]
pub snapshot_id: Option<String>,
```

给 `SCHEMA` 增加 `snapshot_id TEXT` 和索引：

```sql
CREATE INDEX IF NOT EXISTS idx_traces_snapshot ON traces(snapshot_id);
```

迁移列数组增加 `("snapshot_id", "TEXT")`；insert/query 列表同步绑定。新增：

```rust
pub fn link_snapshot(&self, trace_id: &str, snapshot_id: &str) -> bool {
    self.conn.lock()
        .execute(
            "UPDATE traces SET snapshot_id = ?1 WHERE trace_id = ?2 AND (snapshot_id IS NULL OR snapshot_id = ?1)",
            rusqlite::params![snapshot_id, trace_id],
        )
        .map(|changed| changed > 0)
        .unwrap_or_else(|error| {
            tracing::warn!(%error, %trace_id, %snapshot_id, "回链错误快照失败");
            false
        })
}
```

`list_traces` JSON 增加 `"snapshotId": r.snapshot_id`，前端 `TraceRecord` 增加 `snapshotId?: string | null`。

- [ ] **Step 4: 运行 trace 测试**

Run: `cargo test admin::trace_db::tests -- --nocapture`

Expected: PASS，老库缺列时幂等补齐。

- [ ] **Step 5: 提交 trace 关联切片**

```bash
git add -- src/admin/trace_db.rs src/admin/handlers.rs admin-ui/src/types/api.ts
git commit -m "feat(logging): 关联请求链路与错误快照"
```

### Task 6: 注入共享 store 并扩展运行时日志治理

**Files:**
- Modify: `src/main.rs`
- Modify: `src/anthropic/middleware.rs`
- Modify: `src/anthropic/router.rs`
- Modify: `src/admin/middleware.rs`
- Modify: `src/admin/service.rs`
- Modify: `src/admin/types.rs`

- [ ] **Step 1: 写治理请求校验测试**

把范围校验抽成纯函数并先写测试：

```rust
#[test]
fn validates_error_snapshot_governance_ranges() {
    assert!(validate_log_governance_request(&SetLogGovernanceConfigRequest {
        error_snapshot_retention_days: Some(90),
        error_snapshot_max_storage_gb: Some(200),
        error_snapshot_min_free_disk_gb: Some(100),
        ..Default::default()
    }).is_ok());

    assert!(validate_log_governance_request(&SetLogGovernanceConfigRequest {
        error_snapshot_retention_days: Some(0),
        ..Default::default()
    }).is_err());
    assert!(validate_log_governance_request(&SetLogGovernanceConfigRequest {
        error_snapshot_max_storage_gb: Some(0),
        ..Default::default()
    }).is_err());
    assert!(validate_log_governance_request(&SetLogGovernanceConfigRequest {
        error_snapshot_min_free_disk_gb: Some(0),
        ..Default::default()
    }).is_err());
}
```

为 `SetLogGovernanceConfigRequest` 派生 `Default`，便于精确构造 patch 测试。

- [ ] **Step 2: 运行测试确认 RED**

Run: `cargo test validates_error_snapshot_governance_ranges`

Expected: FAIL，新字段和校验函数不存在。

- [ ] **Step 3: 扩展治理响应和 patch**

在响应和请求中加入 camelCase 对应字段：

```rust
pub error_snapshot_enabled: bool,
pub error_snapshot_retention_days: u32,
pub error_snapshot_max_storage_gb: u64,
pub error_snapshot_capture_recovered: bool,
pub error_snapshot_capture_bodies: bool,
pub error_snapshot_min_free_disk_gb: u64,
```

请求字段全部为 `Option`。校验范围固定为：保留天数 `1..=3650`，最大存储 `1..=900` GB，最小空闲 `1..=900` GB，并要求 `maxStorageGb + minFreeDiskGb <= 1000`，与当前 1TB 服务器容量匹配且避免配置成不可实现状态。

在 `ErrorSnapshotPolicy` 增加精确换算构造器：

```rust
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
```

- [ ] **Step 4: 把 store 注入业务和 Admin 状态**

`AppState` 增加 `pub error_snapshot_store: Option<SharedErrorSnapshotStore>` 和 `with_error_snapshot_store`；`create_router` 增加一个参数并在嵌入式构造器传 `None`。`AdminState` 增加必选 `error_snapshot_store: SharedErrorSnapshotStore`。`AdminService` 的 `with_log_governance` 改为：

```rust
pub fn with_log_governance(
    mut self,
    trace_store: Option<SharedTraceStore>,
    usage_recorder: Option<SharedRecorder>,
    error_snapshot_store: Option<SharedErrorSnapshotStore>,
) -> Self
```

getter 从 store 当前 policy 返回运行时值；setter 先校验，再调用 `store.set_policy(ErrorSnapshotPolicy { enabled, retention_days, max_storage_bytes, capture_recovered, capture_bodies, min_free_disk_bytes })`，最后按现有 `Config::load/save` 方式持久化六个字段。

- [ ] **Step 5: 在 main 初始化数据库和维护任务**

在 `cache_dir` 确定后创建 policy：

```rust
let snapshot_policy = admin::error_snapshot_db::ErrorSnapshotPolicy::from_config(&config);
let error_snapshot_store = match admin::ErrorSnapshotStore::open(
    cache_dir.join("error_snapshots.db"),
    cache_dir.join("error-snapshot-fallback"),
    snapshot_policy,
) {
    Ok(store) => std::sync::Arc::new(store),
    Err(error) => {
        tracing::error!(%error, "打开 error_snapshots.db 失败，使用内存索引和磁盘 fallback");
        std::sync::Arc::new(
            admin::ErrorSnapshotStore::open_in_memory_with_fallback(
                cache_dir.join("error-snapshot-fallback"),
                admin::error_snapshot_db::ErrorSnapshotPolicy::from_config(&config),
            ).expect("内存错误快照 store 初始化失败")
        )
    }
};
```

后台任务启动 60 秒后每小时执行 `run_maintenance()`；每次启动先调用 `import_fallback()`，再遍历 `recent_trace_links(Utc::now().timestamp() - 7 * 86_400)` 并逐条调用 `trace_store.link_snapshot(trace_id, snapshot_id)`。维护失败只记录不含正文的 ERROR。

- [ ] **Step 6: 运行治理和全量 Rust 编译测试**

Run: `cargo test validates_error_snapshot_governance_ranges`

Expected: PASS。

Run: `cargo check --all-targets`

Expected: PASS，所有新增构造参数已补齐。

- [ ] **Step 7: 提交运行时注入切片**

```bash
git add -- src/main.rs src/anthropic/middleware.rs src/anthropic/router.rs src/admin/middleware.rs src/admin/service.rs src/admin/types.rs
git commit -m "feat(logging): 接入错误快照运行时治理"
```

### Task 7: 建立请求级 ErrorSnapshotContext 和幂等收口

**Files:**
- Modify: `src/anthropic/error_snapshot.rs`
- Modify: `src/anthropic/handlers.rs`

- [ ] **Step 1: 写触发规则、恢复成功和正常成功测试**

```rust
#[test]
fn pure_success_does_not_create_snapshot() {
    let ctx = sample_context(test_store(), true, true);
    ctx.record_attempt_status(0, Some(200), "success");
    assert_eq!(ctx.finalize(SnapshotFinalState::success()).unwrap(), None);
}

#[test]
fn failed_request_creates_error_snapshot() {
    let store = test_store();
    let ctx = sample_context(store.clone(), true, true);
    ctx.record_internal_error("upstream_tool_protocol_error", "tool JSON truncated");
    let id = ctx.finalize(SnapshotFinalState::error("upstream_tool_protocol_error", Some(502))).unwrap().unwrap();
    assert!(store.get(&id).unwrap().is_some());
}

#[test]
fn recovered_request_is_warning_when_capture_recovered_is_enabled() {
    let store = test_store();
    let ctx = sample_context(store.clone(), true, true);
    ctx.record_attempt_status(0, Some(500), "transient");
    ctx.record_attempt_status(1, Some(200), "success");
    let id = ctx.finalize(SnapshotFinalState::success()).unwrap().unwrap();
    let detail = store.get(&id).unwrap().unwrap();
    assert!(detail.recovered);
    assert_eq!(detail.severity, SnapshotSeverity::Warning);
}

#[test]
fn critical_protocol_error_is_retention_exempt() {
    let store = test_store();
    let ctx = sample_context(store.clone(), true, true);
    ctx.record_internal_error("tool_use_truncated", "incomplete JSON");
    let id = ctx.finalize(SnapshotFinalState::error("tool_use_truncated", Some(502))).unwrap().unwrap();
    let detail = store.get(&id).unwrap().unwrap();
    assert_eq!(detail.severity, SnapshotSeverity::Critical);
    assert!(detail.retention_exempt);
}
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `cargo test anthropic::error_snapshot::tests -- --nocapture`

Expected: 新增触发测试 FAIL。

- [ ] **Step 3: 实现上下文和固定触发规则**

`ErrorSnapshotContext` 使用小型 mutex draft 和 `AtomicBool finalized`：

```rust
pub struct ErrorSnapshotContext {
    store: SharedErrorSnapshotStore,
    trace_id: String,
    snapshot_id: String,
    draft: parking_lot::Mutex<SnapshotDraft>,
    finalized: std::sync::atomic::AtomicBool,
}

struct SnapshotDraft {
    headers: serde_json::Value,
    client_request: serde_json::Value,
    payloads: Vec<RawSnapshotPayload>,
    attempts: Vec<AttemptObservation>,
    protocol_errors: Vec<(String, String)>,
    stream_tail: StreamTail,
    final_credential_id: u64,
    endpoint: Option<String>,
}

struct RawSnapshotPayload {
    kind: SnapshotPayloadKind,
    attempt: Option<u32>,
    content_type: String,
    data: Vec<u8>,
}

struct AttemptObservation {
    attempt: u32,
    http_status: Option<u16>,
    outcome: String,
}

#[derive(Default)]
struct StreamTail {
    bytes: Vec<u8>,
}

pub struct SnapshotFinalState {
    pub final_status: String,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub http_status: Option<u16>,
    pub interrupted_after_bytes: Option<u64>,
}

impl SnapshotFinalState {
    pub fn success() -> Self;
    pub fn error(error_type: &str, http_status: Option<u16>) -> Self;
    pub fn interrupted(error_type: &str, sent_bytes: u64) -> Self;
}

impl ErrorSnapshotContext {
    pub fn new(
        store: SharedErrorSnapshotStore,
        trace_id: String,
        key: &KeyContext,
        headers: &HeaderMap,
        request: &MessagesRequest,
    ) -> Self;
    pub fn record_kiro_request(&self, attempt: u32, credential_id: u64, endpoint: &str, body: &str);
    pub fn record_upstream_response(&self, attempt: u32, status: u16, body: &str);
    pub fn record_upstream_body(&self, attempt: u32, body: &[u8]);
    pub fn record_network_error(&self, attempt: u32, message: &str);
    pub fn record_internal_error(&self, error_type: &str, message: &str);
    pub fn record_stream_chunk(&self, chunk: &[u8]);
    pub fn record_attempt_status(&self, attempt: u32, status: Option<u16>, outcome: &str);
    pub fn set_outbound_metadata(&self, model: &str, endpoint: Option<&str>);
    pub fn finalize(&self, state: SnapshotFinalState) -> anyhow::Result<Option<String>>;
}
```

严重级别固定映射：

```rust
fn classify_severity(error_type: &str, recovered: bool) -> SnapshotSeverity {
    if matches!(error_type,
        "tool_use_truncated" |
        "tool_result_mismatch" |
        "upstream_tool_protocol_error" |
        "upstream_thinking_protocol_error" |
        "sse_state_error" |
        "utf8_decode_error" |
        "snapshot_integrity_error"
    ) {
        SnapshotSeverity::Critical
    } else if recovered {
        SnapshotSeverity::Warning
    } else {
        SnapshotSeverity::Error
    }
}
```

HTTP 400/401/403/408/409/422/429/5xx、网络错误、超时、客户端断开、解析/工具/thinking/structured output/PDF/WebSearch 错误均触发；最终成功且没有失败 attempt/内部错误时不触发。`captureRecovered=false` 时恢复成功不写快照。

`captureBodies=false` 或 `CaptureMode::MetadataOnly` 时不编码 client request 和完整 Kiro request；仍保留请求尺寸/hash、工具 ID 配对诊断、上游错误体、内部错误与 256 KiB 流尾，并把 `omitted_due_to_disk_pressure` 按实际原因写入。HeaderMap 在构造时转成 JSON，秘密 header 立即替换为 `[REDACTED]`，正文和工具 JSON 到最终触发时才执行深度脱敏与压缩。

构造 context 时同时生成不修改原请求的工具诊断：收集所有 assistant `tool_use.id`、user `tool_result.tool_use_id`、出现顺序和工具名；ID 仅允许 ASCII 字母、数字、下划线和连字符。诊断 JSON 明确列出 `invalid_ids`、`duplicate_tool_use_ids`、`unmatched_tool_results`、`missing_tool_results` 和 `block_order`，从而能直接定位 `tool/get_weather/1` 一类问题。

加入对应测试：

```rust
#[test]
fn tool_diagnostics_reports_invalid_duplicate_and_unmatched_ids() {
    let request = request_with_tool_blocks(&[
        ("tool/get_weather/1", "get_weather"),
        ("duplicate", "get_weather"),
        ("duplicate", "get_weather"),
    ], &["missing-result"]);
    let diagnostics = analyze_tool_links(&request);
    assert_eq!(diagnostics.invalid_ids, vec!["tool/get_weather/1"]);
    assert_eq!(diagnostics.duplicate_tool_use_ids, vec!["duplicate"]);
    assert_eq!(diagnostics.unmatched_tool_results, vec!["missing-result"]);
    assert!(diagnostics.missing_tool_results.contains(&"tool/get_weather/1".to_string()));
}
```

- [ ] **Step 4: 让 RequestTracer 持有上下文并先写快照再写 trace**

`RequestTracer` 增加：

```rust
snapshot: Option<std::sync::Arc<ErrorSnapshotContext>>,
finalized: std::sync::atomic::AtomicBool,
reasoning_effort: parking_lot::Mutex<Option<String>>,
```

在 `new` 中先生成 `trace_id`，再用同一 ID 创建 context。`finalize` 用 `swap(true, Ordering::AcqRel)` 保证只执行一次；先调用 snapshot `finalize` 得到 `snapshot_id`，失败仅 ERROR 日志，然后把 ID 放进 `TraceRecord` 并写 `traces.db`。增加：

```rust
pub fn set_reasoning_effort(&self, value: Option<String>);
pub fn record_protocol_error(&self, error_type: &str, message: &str);
pub fn record_stream_chunk(&self, chunk: &[u8]);
```

`TraceSink::on_attempt` 在 push 之前同时调用 `snapshot.record_attempt_status(attempt.attempt, attempt.http_status, &attempt.outcome)`，因此最终成功但曾有失败 attempt 时能稳定判定 `recovered=true`。

- [ ] **Step 5: 把 tracer 提前到请求入口并覆盖早退路径**

在 `post_messages` 和 `post_messages_cc` 中，创建 `UsageRecordHook` 后立即创建一个 tracer；移除 strict-json/stream/non-stream 分支各自的重复构造。所有早退点按实际状态调用：

```rust
tracer.finalize("error", Some("service_unavailable"), Some("Kiro API provider not configured"), None, TraceUsage::zero());
tracer.finalize("error", Some("document_error"), Some(&error.to_string()), None, TraceUsage::zero());
tracer.finalize("error", Some("request_conversion_error"), Some(&message), None, TraceUsage::zero());
tracer.finalize("success", None, None, None, TraceUsage::zero()); // 本地确定性成功响应
```

WebSearch 和 mixed WebSearch 路径以最终 `Response::status()` 决定 success/error；正常成功不会写大快照，但仍保持现有 trace 行为。

- [ ] **Step 6: 运行触发和 handler 现有测试**

Run: `cargo test error_snapshot -- --nocapture`

Expected: PASS。

Run: `cargo test anthropic::handlers::tests -- --nocapture`

Expected: 现有 handler 测试全部 PASS。

- [ ] **Step 7: 提交请求上下文切片**

```bash
git add -- src/anthropic/error_snapshot.rs src/anthropic/handlers.rs
git commit -m "feat(logging): 采集请求级错误现场"
```

### Task 8: 从 provider 采集每跳出站请求、错误响应和网络失败

**Files:**
- Modify: `src/admin/trace_db.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/kiro/provider.rs`

- [ ] **Step 1: 写借用式诊断事件转发测试**

```rust
#[test]
fn request_tracer_forwards_provider_diagnostics_to_snapshot_context() {
    let (tracer, snapshot_store) = test_tracer_with_snapshot();
    tracer.on_diagnostic(TraceDiagnosticEvent::UpstreamRequest {
        attempt: 0,
        credential_id: 7,
        endpoint: "ide",
        body: r#"{"conversationState":{"currentMessage":"hello"}}"#,
    });
    tracer.on_diagnostic(TraceDiagnosticEvent::UpstreamResponse {
        attempt: 0,
        credential_id: 7,
        endpoint: "ide",
        status: 400,
        body: r#"{"message":"Invalid tool use format."}"#,
    });
    tracer.finalize("error", Some("bad_request"), Some("Invalid tool use format."), None, TraceUsage::zero());
    let detail = snapshot_store.query_paged(&SnapshotQuery { limit: 10, ..Default::default() }).unwrap().records.remove(0);
    assert!(detail.payload_count >= 3); // client + Kiro + upstream
}
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `cargo test request_tracer_forwards_provider_diagnostics_to_snapshot_context`

Expected: FAIL，诊断事件类型和 trait 方法不存在。

- [ ] **Step 3: 扩展 TraceSink，不破坏无快照调用方**

```rust
pub enum TraceDiagnosticEvent<'a> {
    UpstreamRequest { attempt: u32, credential_id: u64, endpoint: &'a str, body: &'a str },
    UpstreamResponse { attempt: u32, credential_id: u64, endpoint: &'a str, status: u16, body: &'a str },
    NetworkError { attempt: u32, credential_id: u64, endpoint: &'a str, message: &'a str },
}

pub trait TraceSink: Send + Sync {
    fn on_attempt(&self, attempt: TraceAttempt);
    fn on_diagnostic(&self, _event: TraceDiagnosticEvent<'_>) {}
}
```

`RequestTracer::on_diagnostic` 只在 snapshot context 存在时复制 body/message；快照关闭时默认方法无额外分配。

- [ ] **Step 4: 在 provider 精确上报真实 attempt**

在选定 credential、endpoint、machine ID 后且调用 `execute_api_request_with_proxy_failover` 前上报 `UpstreamRequest`。网络错误分支先上报 `NetworkError` 再 `emit_attempt`。读取非 2xx body 后立刻上报 `UpstreamResponse`，随后执行既有 402/429/4xx/5xx 分类逻辑。成功响应不在 provider 消费 body，由 handler 负责采集。

对备用 endpoint bucket 的每次真实请求使用实际 endpoint 名称和同一轮 attempt；诊断 payload 的 `attempt` 与 `TraceAttempt` 的展示序号最终按接收顺序重排，禁止覆盖同轮多 endpoint 记录。

- [ ] **Step 5: 运行 provider 和转发测试**

Run: `cargo test request_tracer_forwards_provider_diagnostics_to_snapshot_context`

Expected: PASS。

Run: `cargo test kiro::provider::tests -- --nocapture`

Expected: PASS，重试和 content-length fallback 行为不变。

- [ ] **Step 6: 提交 provider 采集切片**

```bash
git add -- src/admin/trace_db.rs src/anthropic/handlers.rs src/kiro/provider.rs
git commit -m "feat(logging): 记录上游重试完整现场"
```

### Task 9: 采集非流式响应、SSE 尾部和协议错误

**Files:**
- Modify: `src/anthropic/error_snapshot.rs`
- Modify: `src/anthropic/handlers.rs`

- [ ] **Step 1: 写滚动流尾和协议标记测试**

```rust
#[test]
fn stream_tail_keeps_latest_256_kib_on_utf8_boundary() {
    let mut tail = StreamTail::default();
    tail.push("开头".repeat(100_000).as_bytes());
    tail.push(b"FINAL_EVENT");
    let bytes = tail.into_bytes();
    assert!(bytes.len() <= STREAM_TAIL_MAX_BYTES);
    assert!(std::str::from_utf8(&bytes).is_ok());
    assert!(bytes.ends_with(b"FINAL_EVENT"));
}

#[test]
fn protocol_error_forces_snapshot_even_after_http_200() {
    let store = test_store();
    let ctx = sample_context(store.clone(), true, true);
    ctx.record_attempt_status(0, Some(200), "success");
    ctx.record_internal_error("upstream_tool_protocol_error", "tool_use JSON incomplete");
    let id = ctx.finalize(SnapshotFinalState::error("upstream_tool_protocol_error", Some(502))).unwrap().unwrap();
    assert!(store.get(&id).unwrap().unwrap().retention_exempt);
}
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `cargo test stream_tail_keeps_latest_256_kib_on_utf8_boundary -- --nocapture`

Run: `cargo test protocol_error_forces_snapshot_even_after_http_200 -- --nocapture`

Expected: FAIL，`StreamTail` 尚不存在。

- [ ] **Step 3: 实现有界流尾**

定义 `STREAM_TAIL_MAX_BYTES = 256 * 1024`。`StreamTail::push` 追加后从头丢弃超出部分，并向后移动到 UTF-8 边界；若 chunk 不是合法 UTF-8，则保存十六进制摘要对象和原始长度，而不是损坏字节。快照最终把尾部编码为 `SnapshotPayloadKind::StreamTail`。

- [ ] **Step 4: 在四个上游 body 消费点记录数据**

在以下现有位置调用 `tracer.record_stream_chunk(&body)` 或 `tracer.record_stream_chunk(&chunk)`：

1. `collect_buffered_attempt` 的 `response.bytes()` 后调用 `record_upstream_body(attempt_index, &body)`；
2. 普通流式 `body_stream.next()` 成功 chunk 后；
3. 非流式 `collect_non_stream_attempt` 的 `response.bytes()` 后调用 `record_upstream_body(attempt_index, &body_bytes)`；
4. `/cc/v1` 缓冲流的 `body_stream.next()` 成功 chunk 后。

非流式完整 body 作为 `upstream_response` 保存；流式只保存最后 256 KiB。对 decoder feed、frame decode、Event decode、tool JSON accumulator、thinking signature、空 assistant、strict JSON 第二次恢复失败等分支，在现有 `finalize` 前调用 `record_protocol_error`，错误类型使用现有公开错误码，不新增会改变客户端响应的映射。

把 `tracing::debug!("Kiro request body: {}", request_body)` 替换为结构化安全日志：`trace_id`、body 字节数、SHA-256、模型、stream；正文只进入错误快照。为此给 `RequestTracer` 增加只读 `trace_id(&self) -> &str`。即使后续临时开启本 crate DEBUG，也不再把完整请求体写入 Docker 环形日志。

- [ ] **Step 5: 补齐客户端断开和 idle timeout 收口**

两套流式循环遇到 `sender.closed()` 时调用：

```rust
tracer.record_protocol_error("client_disconnected", "client closed response stream");
tracer.finalize(
    "interrupted",
    Some("client_disconnected"),
    Some("client closed response stream"),
    Some(received_bytes),
    stream_trace_usage(&ctx),
);
```

Idle timeout、read error、EOF 但无语义输出分别使用 `stream_idle_timeout`、`stream_read_error`、`upstream_empty_response`。所有 `finalize` 幂等，重试分支不会重复插入。

- [ ] **Step 6: 运行流式/非流式回归**

Run: `cargo test anthropic::handlers::tests -- --nocapture`

Expected: PASS，包括 strict JSON、工具截断、空响应、UTF-8 和流式事件现有测试。

Run: `cargo test error_snapshot -- --nocapture`

Expected: PASS。

- [ ] **Step 7: 提交协议现场切片**

```bash
git add -- src/anthropic/error_snapshot.rs src/anthropic/handlers.rs
git commit -m "feat(logging): 保存流式与协议错误尾部"
```

### Task 10: 增加 Admin 快照 API

**Files:**
- Modify: `src/admin/handlers.rs`
- Modify: `src/admin/router.rs`
- Modify: `src/admin/types.rs`

- [ ] **Step 1: 写查询解析和下载安全头测试**

```rust
#[test]
fn parses_snapshot_filters_and_caps_page_size() {
    let params = HashMap::from([
        ("severity".into(), "critical".into()),
        ("recovered".into(), "true".into()),
        ("pinned".into(), "false".into()),
        ("limit".into(), "9999".into()),
    ]);
    let query = parse_snapshot_query(&params).unwrap();
    assert_eq!(query.severity, Some(SnapshotSeverity::Critical));
    assert_eq!(query.recovered, Some(true));
    assert_eq!(query.pinned, Some(false));
    assert_eq!(query.limit, 500);
}

#[test]
fn download_response_is_attachment_and_nosniff() {
    let response = snapshot_download_response("snap-1", br#"{"safe":true}"#.to_vec());
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    assert!(response.headers()[header::CONTENT_DISPOSITION].to_str().unwrap().contains("attachment"));
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
}
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `cargo test parses_snapshot_filters_and_caps_page_size -- --nocapture`

Run: `cargo test download_response_is_attachment_and_nosniff -- --nocapture`

Expected: FAIL，handler helper 不存在。

- [ ] **Step 3: 实现九个 Admin 端点**

路由固定为：

```rust
.route("/error-snapshots", get(list_error_snapshots))
.route("/error-snapshots/storage", get(error_snapshot_storage))
.route("/error-snapshots/cleanup", post(cleanup_error_snapshots))
.route("/error-snapshots/{id}", get(get_error_snapshot).delete(delete_error_snapshot))
.route("/error-snapshots/{id}/payload/{seq}", get(get_error_snapshot_payload))
.route("/error-snapshots/{id}/download", get(download_error_snapshot))
.route("/error-snapshots/{id}/pin", post(pin_error_snapshot))
.route("/error-snapshots/{id}/unpin", post(unpin_error_snapshot))
```

所有路由位于现有 Admin 鉴权层内。查询支持 `traceId/model/errorType/httpStatus/credentialId/severity/recovered/pinned/from/to/limit/offset`。不存在返回 404；非法 severity/数字返回 400；数据库错误返回 500，但响应和日志都不包含 payload 正文。

下载 JSON 包结构固定为：

```json
{
  "metadata": {},
  "payloads": [
    {"seq": 0, "kind": "client_request", "attempt": null, "contentType": "application/json", "sha256": "0000000000000000000000000000000000000000000000000000000000000000", "content": {}}
  ]
}
```

每个 payload 按需解压并再次校验 SHA-256；任一校验失败终止下载并返回 500 `snapshot_integrity_error`，同时记录不含正文的 critical 日志。

- [ ] **Step 4: 运行 Admin helper 和 Rust 编译测试**

Run: `cargo test parses_snapshot_filters_and_caps_page_size -- --nocapture`

Run: `cargo test download_response_is_attachment_and_nosniff -- --nocapture`

Expected: PASS。

Run: `cargo check --all-targets`

Expected: PASS。

- [ ] **Step 5: 提交 Admin API 切片**

```bash
git add -- src/admin/handlers.rs src/admin/router.rs src/admin/types.rs
git commit -m "feat(admin): 增加错误快照管理接口"
```

### Task 11: 增加前端类型、API、hooks 和纯函数测试

**Files:**
- Modify: `admin-ui/package.json`
- Modify: `admin-ui/src/types/api.ts`
- Create: `admin-ui/src/api/error-snapshots.ts`
- Create: `admin-ui/src/hooks/use-error-snapshots.ts`
- Create: `admin-ui/src/lib/error-snapshot-utils.ts`
- Create: `admin-ui/src/lib/error-snapshot-utils.test.ts`

- [ ] **Step 1: 写 Bun 纯函数测试**

`admin-ui/package.json` 增加脚本 `"test": "bun test"`。测试文件：

```ts
import { describe, expect, test } from 'bun:test'
import { buildSnapshotParams, formatBytes, severityLabel } from './error-snapshot-utils'

describe('error snapshot helpers', () => {
  test('omits empty filters and keeps booleans', () => {
    expect(buildSnapshotParams({ severity: '', recovered: false, pinned: true, limit: 50, offset: 0 }))
      .toEqual({ recovered: 'false', pinned: 'true', limit: '50', offset: '0' })
  })

  test('formats storage and severity consistently', () => {
    expect(formatBytes(1024 ** 3)).toBe('1.00 GB')
    expect(severityLabel('critical')).toBe('严重')
    expect(severityLabel('warning')).toBe('警告')
  })
})
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `cd admin-ui && bun test src/lib/error-snapshot-utils.test.ts`

Expected: FAIL，helper 文件不存在。

- [ ] **Step 3: 定义前端 API 类型**

在 `admin-ui/src/types/api.ts` 增加：

```ts
export type SnapshotSeverity = 'critical' | 'error' | 'warning' | 'info'
export type SnapshotPayloadKind =
  | 'client_request' | 'kiro_request' | 'upstream_response'
  | 'tool_diagnostics' | 'stream_tail' | 'internal_error'

export interface ErrorSnapshotSummary {
  snapshotId: string
  traceId: string
  ts: string
  model: string
  isStream: boolean
  keyId: number
  keySource: 'masterApiKey' | 'clientKey'
  finalCredentialId: number
  endpoint: string | null
  httpStatus: number | null
  finalStatus: string
  errorType: string
  severity: SnapshotSeverity
  errorMessage: string | null
  recovered: boolean
  pinned: boolean
  retentionExempt: boolean
  omittedDueToDiskPressure: boolean
  payloadCount: number
  originalBytes: number
  compressedBytes: number
  createdAt: number
  updatedAt: number
}
export interface ErrorSnapshotPayloadMeta { seq: number; kind: SnapshotPayloadKind; attempt: number | null; contentType: string; originalBytes: number; compressedBytes: number; sha256: string; partCount: number }
export interface ErrorSnapshotDetail extends ErrorSnapshotSummary { payloads: ErrorSnapshotPayloadMeta[] }
export interface ErrorSnapshotPayload extends ErrorSnapshotPayloadMeta { content: unknown }
export interface ErrorSnapshotQuery { traceId?: string; model?: string; errorType?: string; httpStatus?: number; credentialId?: number; severity?: SnapshotSeverity | ''; recovered?: boolean; pinned?: boolean; from?: string; to?: string; limit?: number; offset?: number }
export interface ErrorSnapshotPage { records: ErrorSnapshotSummary[]; total: number }
export interface ErrorSnapshotStorageStatus { dbBytes: number; walBytes: number; shmBytes: number; fallbackBytes: number; totalBytes: number; availableBytes: number; maxStorageBytes: number; minFreeDiskBytes: number; diskPressure: boolean; records: number; pinnedRecords: number; criticalRecords: number }
```

字段必须完整展开，不使用 `any`；`ErrorSnapshotSummary` 包含设计文档第 9.1 节全部字段。

- [ ] **Step 4: 实现 API 和 Query hooks**

`error-snapshots.ts` 使用与 `traces.ts` 相同的 Axios 鉴权 interceptor，实现 list/detail/payload/storage/download/pin/unpin/delete/cleanup。`buildSnapshotParams` 只省略 `undefined` 和空字符串，必须保留 `false` 与 `0`。

hooks query key 固定为：

```ts
['errorSnapshots', query]
['errorSnapshots', 'detail', id]
['errorSnapshots', 'payload', id, seq]
['errorSnapshots', 'storage']
```

mutation 成功后统一 invalidate `['errorSnapshots']` 和 `['traces']`；payload hook 仅在详情对话框已打开且选择了 seq 时启用。

- [ ] **Step 5: 运行前端测试和类型构建**

Run: `cd admin-ui && bun test src/lib/error-snapshot-utils.test.ts`

Expected: PASS。

Run: `cd admin-ui && bun run build`

Expected: PASS。

- [ ] **Step 6: 提交前端数据层切片**

```bash
git add -- admin-ui/package.json admin-ui/src/types/api.ts admin-ui/src/api/error-snapshots.ts admin-ui/src/hooks/use-error-snapshots.ts admin-ui/src/lib/error-snapshot-utils.ts admin-ui/src/lib/error-snapshot-utils.test.ts
git commit -m "feat(admin): 接入错误快照前端数据层"
```

### Task 12: 实现快照列表、详情对话框和日志治理 UI

**Files:**
- Create: `admin-ui/src/components/error-snapshot-ui.contract.test.ts`
- Create: `admin-ui/src/components/error-snapshot-page.tsx`
- Create: `admin-ui/src/components/error-snapshot-dialog.tsx`
- Modify: `admin-ui/src/components/trace-log-page.tsx`
- Modify: `admin-ui/src/api/credentials.ts`
- Modify: `admin-ui/src/App.tsx`

- [ ] **Step 1: 写 UI 接线契约测试并确认 RED**

创建：

```ts
import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

describe('error snapshot UI wiring', () => {
  test('adds a top-level snapshot page and trace drill-down', async () => {
    const app = await readFile('src/App.tsx', 'utf8')
    const trace = await readFile('src/components/trace-log-page.tsx', 'utf8')
    expect(app).toContain('key: "snapshots"')
    expect(app).toContain('<ErrorSnapshotPage />')
    expect(trace).toContain('rec.snapshotId')
    expect(trace).toContain('查看错误快照')
  })

  test('exposes all six governance controls', async () => {
    const trace = await readFile('src/components/trace-log-page.tsx', 'utf8')
    for (const field of [
      'errorSnapshotEnabled', 'errorSnapshotRetentionDays',
      'errorSnapshotMaxStorageGb', 'errorSnapshotCaptureRecovered',
      'errorSnapshotCaptureBodies', 'errorSnapshotMinFreeDiskGb',
    ]) expect(trace).toContain(field)
  })
})
```

Run: `cd admin-ui && bun test src/components/error-snapshot-ui.contract.test.ts`

Expected: FAIL，快照页、trace 下钻和治理控件尚未接线。

- [ ] **Step 2: 实现 ErrorSnapshotDialog**

对话框宽度 `sm:max-w-6xl max-h-[92vh]`，内部使用本地按钮页签而不是新增 UI 库。固定页签顺序：概览、客户端请求、Kiro 请求、上游响应、工具诊断、流式尾部、内部错误。打开页签时才请求对应 payload；JSON 用 `JSON.stringify(content, null, 2)`，文本保持原样，容器使用 `whitespace-pre-wrap break-all overflow-auto`。

顶部操作：复制当前 payload、下载完整快照、pin/unpin、删除。删除继续使用现有 `useConfirm()`；下载 Blob 后创建临时 `<a download>`，点击后 `URL.revokeObjectURL`。任何 API 错误只显示摘要，不把响应正文写进 console。

- [ ] **Step 3: 实现 ErrorSnapshotPage**

页面包含：

- 存储卡：当前总占用/200GB 上限、可用磁盘/100GB 下限、fallback 大小、记录数、pin/critical 数；磁盘压力时显示红色告警。
- 筛选：trace ID、model、error type、HTTP status、credential、severity、recovered、pinned、时间范围。
- 表格：时间、severity、error type、HTTP、model、credential、recovered、payload 数/压缩率、pin、操作。
- 分页：每页 50 条；筛选变化重置到第 0 页。
- 操作：查看、pin/unpin、删除、立即清理、刷新。

列表不请求任何 payload BLOB。

- [ ] **Step 4: 从 trace 和顶级导航接入**

`ExpandedDetail` 在 `rec.snapshotId` 存在时显示“查看错误快照”按钮并打开同一个 `ErrorSnapshotDialog`。`App.tsx` 增加 lazy import、`Tab` 值 `snapshots`、导航项“错误快照”和 `ShieldAlert` 图标，hash 为 `#/snapshots`。

- [ ] **Step 5: 扩展 GovernanceButton**

先给 `LogGovernanceConfig` 增加六个快照字段，再在现有 trace/usage 设置下方增加：快照启用、捕获恢复请求、保存正文三个开关，以及保留天数、最大存储 GB、最小空闲 GB 三个输入。前端范围与后端一致；每个 patch 只提交被修改字段。文案明确：关闭“保存正文”仍保存元数据、工具诊断、上游错误和流尾；关闭快照不影响 `traces.db`。

- [ ] **Step 6: 运行前端测试和生产构建**

Run: `cd admin-ui && bun test`

Expected: PASS。

Run: `cd admin-ui && bun run build`

Expected: PASS，无 TypeScript 错误。

- [ ] **Step 7: 提交 UI 切片**

```bash
git add -- admin-ui/src/components/error-snapshot-ui.contract.test.ts admin-ui/src/components/error-snapshot-page.tsx admin-ui/src/components/error-snapshot-dialog.tsx admin-ui/src/components/trace-log-page.tsx admin-ui/src/api/credentials.ts admin-ui/src/App.tsx
git commit -m "feat(admin): 增加错误快照管理页面"
```

### Task 13: 增加 8991 黑盒验收脚本

**Files:**
- Create: `scripts/error-snapshot-smoke.sh`
- Create: `scripts/error-snapshot-smoke.test.ts`

- [ ] **Step 1: 写脚本契约 RED 测试**

```ts
import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

describe('error snapshot smoke contract', () => {
  test('requires env keys and never echoes them', async () => {
    const script = await readFile('scripts/error-snapshot-smoke.sh', 'utf8')
    expect(script).toContain(': "${TEST_API_KEY:?')
    expect(script).toContain(': "${TEST_ADMIN_KEY:?')
    expect(script).toContain('set +x')
    expect(script).not.toContain('echo "$TEST_API_KEY"')
    expect(script).not.toContain('echo "$TEST_ADMIN_KEY"')
  })

  test('checks redaction, trace linkage and downloaded payload', async () => {
    const script = await readFile('scripts/error-snapshot-smoke.sh', 'utf8')
    expect(script).toContain('/api/admin/error-snapshots')
    expect(script).toContain('/download')
    expect(script).toContain('snapshotId')
    expect(script).toContain('[REDACTED]')
    expect(script).toContain('redacted_base64')
  })
})
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `bun test scripts/error-snapshot-smoke.test.ts`

Expected: FAIL，smoke 脚本不存在。

- [ ] **Step 3: 实现不泄密的 smoke 脚本**

脚本要求 `TEST_API_KEY`、`TEST_ADMIN_KEY`，可选 `TEST_BASE_URL` 默认 `https://rs-test.43-225-196-10.sslip.io`。使用 `mktemp -d`，trap 删除目录并 unset 两个 key；开头 `set -Eeuo pipefail` 和 `set +x`。

固定验收流程：

1. 发送含 `tool_use.id = "tool/get_weather/1"`、Authorization-like 工具字段、短 base64 字符串和一段 8KiB PDF base64 的续轮请求；
2. 接受 400/422/502 作为预期失败；
3. 轮询 `/api/admin/traces?onlyFailed=true&limit=20` 找到最新 trace 和 `snapshotId`；
4. 查询快照详情和下载包；
5. 验证下载包含客户普通文本、工具 Schema、`[REDACTED]`、`redacted_base64`；
6. 验证下载不包含测试 API Key、Admin Key、`tool-secret-value` 或原始 PDF base64；
7. pin、unpin，再删除该测试快照；
8. 请求 `/storage`，确认 `diskPressure` 是布尔值且容量字段存在。

所有 curl 使用 `--silent --show-error`，禁止 `-v`；输出只打印 HTTP 状态、trace ID、snapshot ID 和 PASS/FAIL。

- [ ] **Step 4: 运行脚本契约和 shell 语法检查**

Run: `bun test scripts/error-snapshot-smoke.test.ts`

Expected: PASS。

Run: `bash -n scripts/error-snapshot-smoke.sh`

Expected: exit 0。

- [ ] **Step 5: 提交验收脚本**

```bash
git update-index --chmod=+x scripts/error-snapshot-smoke.sh
git add -- scripts/error-snapshot-smoke.sh scripts/error-snapshot-smoke.test.ts
git commit -m "test(logging): 增加错误快照黑盒验收"
```

### Task 14: 全量验证、8991 部署和正式日志降噪验收

**Files:**
- Modify: `README.md`

- [ ] **Step 1: 补充运维说明**

在 README 的服务器测试构建章节增加：数据库路径 `config/error_snapshots.db`、fallback 路径、默认 90 天/200GB/100GB、Admin 页入口、备份时需要同时包含 DB/WAL/SHM/fallback，以及快照验证完成后的正式日志建议：

```text
RUST_LOG=info,h2=warn,hyper=warn,hyper_util=warn,reqwest=warn,rustls=warn
```

明确关闭全量 DEBUG 前必须先在 8991 成功抓到非法工具 ID、截断 tool JSON、空响应和流式中断四类快照。

- [ ] **Step 2: 运行格式、静态检查和全部测试**

Run: `cargo fmt --all -- --check`

Expected: PASS。

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: PASS。

Run: `cargo test --all-features`

Expected: PASS。

Run: `cd admin-ui && bun test && bun run build`

Expected: PASS。

Run: `bun test scripts/test-builder-contract.test.ts scripts/error-snapshot-smoke.test.ts`

Expected: PASS。

- [ ] **Step 3: 检查仓库和敏感信息**

Run: `git diff --check`

Expected: 无输出。

Run: `rg -n "csk_|sk-admin-|Bearer [A-Za-z0-9_-]{12,}|refreshToken\"\s*:\s*\"[^[]" src admin-ui scripts README.md`

Expected: 不命中真实密钥；测试 fixture 只允许明显的固定假值。

Run: `git status --short`

Expected: 仅 README 尚未提交，或工作区干净。

- [ ] **Step 4: 提交文档和最终验证点**

```bash
git add -- README.md
git commit -m "docs(logging): 补充错误快照运维说明"
```

- [ ] **Step 5: 推送验证分支并部署指定 commit 到 8991**

```bash
git push deploy HEAD:refs/heads/fix/error-snapshot-persistence
ssh -p 18792 root@43.225.196.10 "cd /opt/kiro-rs-test && ./scripts/test-deploy.sh deploy/fix/error-snapshot-persistence"
```

Expected: 构建输出给出准确 `commit=<sha>`、`image=kiro-rs-test:<sha>`、健康 URL；生产容器 `kiro-rs-admin` 和 8990 未被替换。

- [ ] **Step 6: 在 8991 运行真实错误快照验收**

在本地终端临时设置测试容器的 API/Admin Key，不在命令历史或输出中展开，然后运行：

```bash
TEST_BASE_URL=https://rs-test.43-225-196-10.sslip.io \
TEST_API_KEY="$TEST_API_KEY" \
TEST_ADMIN_KEY="$TEST_ADMIN_KEY" \
./scripts/error-snapshot-smoke.sh
```

Expected: 输出 `error snapshot smoke: PASS`；Admin UI 能按 trace ID 打开同一 snapshot，下载包无认证密钥和原始附件 base64。

- [ ] **Step 7: 补充四类人工故障验收**

依次制造并记录 snapshot ID：

1. `tool/get_weather/1` 非法历史工具 ID；
2. 上游提前结束的 `tool_use` JSON；
3. `upstream returned no assistant content`；
4. SSE 中途断开或 idle timeout。

每条记录检查 client request、Kiro request、upstream response/stream tail、attempt 链和最终错误类型。对一条记录执行 pin 后运行 cleanup，确认仍存在；unpin 后手动删除。临时把测试配置容量调小触发 cleanup 和 metadata-only，再恢复 200GB/100GB，禁止在生产 8990 做容量实验。

- [ ] **Step 8: 验证正常客户路径无回归**

在 8991 连续验证：普通文本非流式、普通文本流式、Claude Code 工具调用续轮、PDF 文本提取、strict JSON、thinking 请求、缓存 warm 续轮。确认：HTTP/SSE 协议、tool_use 参数、token 拆分、cache_read/cache_creation、对话连续性与实施前一致；正常单跳成功请求在快照列表中不存在大 payload。

- [ ] **Step 9: 验证后再关闭测试容器全量 DEBUG**

把 8991 的 `RUST_LOG` 改为建议 INFO 过滤器，重建测试容器，再重复非法工具 ID 和截断 tool JSON；两条问题仍可只靠 Admin 快照完整定位才算通过。生产 8990 的日志级别和镜像保持不变，等待用户单独授权部署。

## 完成判定

- Rust、Bun、Admin UI build、shell contract 全部通过。
- 错误快照可通过 trace ID 查询，列表不加载 BLOB，详情按页签懒加载。
- 下载包保留客户文本、工具 Schema/JSON、Kiro 请求、上游错误和流尾，但没有认证秘密或原始图片/PDF base64。
- SQLite 写失败可进入 fallback，重启/维护后幂等导入。
- 90 天、200GB、100GB、pin、critical、incremental vacuum 和 WAL checkpoint 均有自动化测试或 8991 实测证据。
- 快照系统故障不改变原客户响应；正常成功请求无大快照，token、缓存和对话行为无回归。
- 8991 在 INFO 日志下仍能定位四类目标故障；生产 8990 未修改。
