# 自动压缩诊断监控 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在完全不改变 HTTP/SSE/重试/usage 行为的前提下，增加可运行时关闭的自动压缩安全诊断，并通过 Docker 日志、`traces.db` 和 Admin 页面定位信号未暴露、客户端未压缩或压缩不足。

**Architecture:** 请求入口在独立原子开关开启时创建请求局部诊断状态，利用现有 TraceSink 和 SSE 发送点观察上游与客户端边界，在 finalize 时生成纯计数快照。快照随现有非阻塞 trace 队列写入 SQLite；跨请求推断只在后台写事务中按 `session_hash` 查询上一条记录。

**Tech Stack:** Rust 2024、Axum、Tokio、rusqlite/WAL、Serde、React 19、TypeScript、TanStack Query、Bun test、Vite。

---

### Task 1: 配置开关与纯诊断模型

**Files:**
- Create: `src/anthropic/compaction_diagnostics.rs`
- Modify: `src/anthropic/mod.rs`
- Modify: `src/model/config.rs`
- Modify: `src/kiro/token_manager.rs`

- [ ] **Step 1: 写配置和诊断纯函数失败测试**

在 `src/model/config.rs` 测试中增加默认开启和 camelCase 往返断言；在新模块测试中覆盖 session hash、版本提取、请求形状和七种分类。目标 API：

```rust
pub(crate) struct CompactionDiagnostics { /* immutable fields + atomics */ }

impl CompactionDiagnostics {
    pub(crate) fn new(enabled: bool, headers: &HeaderMap, request: &MessagesRequest) -> Self;
    pub(crate) fn observe_upstream_request(&self, body_bytes: usize);
    pub(crate) fn observe_upstream_context(&self, percentage: f64, window_tokens: i32);
    pub(crate) fn observe_client_event_enqueued(&self, event: &SseEvent);
    pub(crate) fn finalize(&self, outcome: CompactionFinalize<'_>) -> Option<CompactionTraceData>;
}
```

- [ ] **Step 2: 运行测试并确认因缺少字段/模块失败**

Run: `cargo test model::config::tests::auto_compact_diagnostics anthropic::compaction_diagnostics --bin kiro-rs`

Expected: FAIL，错误明确指向缺少 `auto_compact_diagnostics_enabled` 或模块/类型不存在。

- [ ] **Step 3: 实现最小配置和诊断模型**

增加：

```rust
#[serde(default = "default_true")]
pub auto_compact_diagnostics_enabled: bool,
```

`MultiTokenManager` 增加 `AtomicBool`、getter 和仅运行时 setter。新诊断模块必须：关闭时返回 `Disabled`；开启时只保存数字版本、SHA-256 hash、计数和原子状态；详细 JSON 使用 `schemaVersion: 1`，不包含任何正文值。

- [ ] **Step 4: 运行目标测试并确认通过**

Run: `cargo test model::config::tests::auto_compact_diagnostics anthropic::compaction_diagnostics --bin kiro-rs`

Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add src/anthropic/compaction_diagnostics.rs src/anthropic/mod.rs src/model/config.rs src/kiro/token_manager.rs
git commit -m "feat: 增加自动压缩诊断模型与开关"
```

### Task 2: 请求/上游/SSE 边界采集且响应字节不变

**Files:**
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/compaction_diagnostics.rs`

- [ ] **Step 1: 写失败测试证明边界状态和响应不变性**

增加测试：同一 `SseEvent` 序列在诊断启用/关闭时调用现有发送函数，收集到的 `Bytes` 必须完全相等；`message_start` 成功入队后记录 input/cache 合计；发送失败不标记入队；`Event::ContextUsage` 记录 percentage/token；开关关闭后详细 JSON 为 `None`。

- [ ] **Step 2: 运行目标测试确认失败**

Run: `cargo test anthropic::handlers::tests::compaction_diagnostics --bin kiro-rs`

Expected: FAIL，缺少观察调用或快照字段。

- [ ] **Step 3: 接入 RequestTracer 与现有边界**

`RequestTracer` 增加 `CompactionDiagnostics` 字段。构造时从 provider 的原子开关读取；`TraceSink::on_diagnostic` 在错误快照判断之前记录 Kiro body 大小和字节阈值响应；所有 `Event::ContextUsage` 在交给 context 前观察；`send_sse_events` 仅在 `sender.send` 成功后观察原始事件。

ProbationBuffer 两个重试决策点记录：

```rust
tracer.observe_probation(
    probation.semantic_output_started(),
    can_retry,
    retryable,
);
```

finalize 先生成/输出安全诊断结论，再按原路径组装 TraceRecord。不得修改 `SseEvent`、不得重排 finalize 与客户返回逻辑。

- [ ] **Step 4: 运行目标测试和关键协议测试**

Run: `cargo test anthropic::handlers::tests::compaction_diagnostics anthropic::stream::tests::buffered_cc_stream_rewrites_message_start_with_upstream_context --bin kiro-rs`

Expected: PASS，响应字节断言完全相等。

- [ ] **Step 5: 提交**

```bash
git add src/anthropic/handlers.rs src/anthropic/compaction_diagnostics.rs
git commit -m "feat: 采集自动压缩信号边界"
```

### Task 3: SQLite 迁移、后台推断和查询

**Files:**
- Modify: `src/admin/trace_db.rs`

- [ ] **Step 1: 写迁移/往返/推断/锁失败测试**

增加测试覆盖：八列和两个索引重复迁移；诊断字段完整往返；`compaction_diagnosis`、`session_hash`、`high_pressure_only` 过滤；相同 session 的 85% 未缩小推断；20% 缩小但仍撞墙推断；持有 conn 锁时 `insert` 仍立即返回；队列满仍丢弃。

- [ ] **Step 2: 运行目标测试确认失败**

Run: `cargo test admin::trace_db::tests::compaction --bin kiro-rs`

Expected: FAIL，缺少列、查询字段或推断结果。

- [ ] **Step 3: 扩展 TraceRecord 和 SCHEMA**

增加可空持久化结构：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionTraceData {
    pub session_hash: Option<String>,
    pub client_version: Option<String>,
    pub diagnosis: String,
    pub request_body_bytes: u64,
    pub upstream_context_tokens: Option<u64>,
    pub upstream_context_percentage: Option<f64>,
    pub client_reported_tokens: Option<u64>,
    pub diagnostics_json: String,
}
```

`TraceRecord` 使用 `Option<CompactionTraceData>`。迁移增加八列和 `(session_hash, ts_epoch)`、`compaction_diagnosis` 索引。`write_record` 在事务中查询上一条同 session 记录并计算最终存储诊断；此查询不得移到 `insert`。

- [ ] **Step 4: 扩展 TraceQuery**

增加：

```rust
pub compaction_diagnosis: Option<String>,
pub session_hash: Option<String>,
pub high_pressure_only: bool,
```

高压力 SQL 条件为 percentage >= 80、request bytes >= 2,500,000 或 diagnosis 非 normal。

- [ ] **Step 5: 运行 trace_db 测试**

Run: `cargo test admin::trace_db::tests --bin kiro-rs`

Expected: PASS，包括原有非阻塞回归测试。

- [ ] **Step 6: 提交**

```bash
git add src/admin/trace_db.rs
git commit -m "feat: 持久化自动压缩诊断与会话推断"
```

### Task 4: Admin 配置和 API 输出

**Files:**
- Modify: `src/admin/types.rs`
- Modify: `src/admin/service.rs`
- Modify: `src/admin/handlers.rs`

- [ ] **Step 1: 写失败测试**

覆盖治理响应包含 `autoCompactDiagnosticsEnabled`；PUT 单字段可即时修改原子开关并写回 config；空请求校验包含新字段；trace JSON 返回诊断字段；query 参数解析 `compactionDiagnosis/sessionHash/highPressureOnly`。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test admin::service::tests::auto_compact admin::handlers::tests::compaction --bin kiro-rs`

Expected: FAIL，缺少请求/响应字段。

- [ ] **Step 3: 实现 Admin 配置和查询接线**

`LogGovernanceConfigResponse`、`SetLogGovernanceConfigRequest` 增加布尔字段；service 读写 MultiTokenManager 原子值并复用既有 config 原子保存；handler 将诊断结构解析为 JSON 对象返回，不返回原始 session id。

- [ ] **Step 4: 运行目标测试**

Run: `cargo test admin::service::tests::auto_compact admin::handlers::tests::compaction --bin kiro-rs`

Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add src/admin/types.rs src/admin/service.rs src/admin/handlers.rs
git commit -m "feat: 暴露自动压缩诊断治理与查询"
```

### Task 5: Admin UI 开关、筛选和会话时间线

**Files:**
- Modify: `admin-ui/src/api/credentials.ts`
- Modify: `admin-ui/src/api/traces.ts`
- Modify: `admin-ui/src/types/api.ts`
- Modify: `admin-ui/src/components/trace-log-page.tsx`
- Create: `admin-ui/src/components/auto-compact-diagnostics-ui.contract.test.ts`

- [ ] **Step 1: 写失败契约测试**

契约测试检查：治理开关提交 `autoCompactDiagnosticsEnabled`；API 发送三个新查询参数；页面有诊断原因和高压力筛选；详情展示 session hash、安全计数和“查看同会话”；不得出现 `user_id`、正文或工具参数字段。

- [ ] **Step 2: 运行测试确认失败**

Run: `bun test src/components/auto-compact-diagnostics-ui.contract.test.ts`

Expected: FAIL，页面和 API 尚未接线。

- [ ] **Step 3: 实现类型和 UI**

新增 `CompactionDiagnostics` TypeScript 类型，TraceRecord 暴露扁平筛选字段和详细对象。治理菜单添加独立 Switch；页面状态增加 diagnosis、high pressure、session hash；点击“查看同会话”设置 session 筛选并回到第一页；行内用 warning/destructive/outline Badge 区分诊断。

- [ ] **Step 4: 运行前端测试和构建**

Run: `bun test && npm run build`

Expected: 现有测试与新契约测试全部 PASS，TypeScript/Vite 构建成功。

- [ ] **Step 5: 提交**

```bash
git add admin-ui/src/api/credentials.ts admin-ui/src/api/traces.ts admin-ui/src/types/api.ts admin-ui/src/components/trace-log-page.tsx admin-ui/src/components/auto-compact-diagnostics-ui.contract.test.ts
git commit -m "feat: 增加自动压缩诊断管理界面"
```

### Task 6: 完整验证与差异审查

**Files:**
- Modify: `docs/ISSUE-auto-compact-not-triggering.md`（仅追加诊断能力说明，不把根因标为已解决）

- [ ] **Step 1: 更新问题文档**

追加开关、Docker 字段、Admin 筛选、SQLite 列和“仍需线上采样才能确认根因”的说明。

- [ ] **Step 2: 格式与静态检查**

Run: `cargo fmt --check`

Expected: PASS。

Run: `git diff --check`

Expected: PASS。

- [ ] **Step 3: 后端目标测试**

Run: `cargo test anthropic::compaction_diagnostics admin::trace_db::tests::compaction admin::service::tests::auto_compact admin::handlers::tests::compaction --bin kiro-rs`

Expected: PASS。

- [ ] **Step 4: 前端完整验证**

Run: `bun test && npm run build`（工作目录 `admin-ui`）

Expected: 全部 PASS。

- [ ] **Step 5: Rust 全量验证**

Run: `cargo test --bin kiro-rs`

Expected: 新增和相关测试 PASS；若仅出现基线已确认的 `http_client::tests::upstream_uses_one_connection_per_request`，记录为既有失败，不修改本任务范围外代码。

- [ ] **Step 6: 安全扫描和需求核对**

Run: `rg -n "user_id|authorization|bearer|api[_-]?key|request body|tool.*input" src/anthropic/compaction_diagnostics.rs admin-ui/src/components/trace-log-page.tsx`

Expected: 只有明确的输入读取/禁止说明，不存在把敏感值写入日志或数据库的代码。

- [ ] **Step 7: 最终提交**

```bash
git add -f docs/ISSUE-auto-compact-not-triggering.md docs/superpowers/plans/2026-07-29-auto-compact-diagnostics.md
git commit -m "docs: 说明自动压缩诊断使用方式"
```
