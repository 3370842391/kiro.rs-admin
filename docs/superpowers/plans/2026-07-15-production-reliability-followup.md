# 生产协议与多模态可靠性收口 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复结构化输出误拒绝、图片软预算与格式处理、工具 Schema 恢复、120 秒流中断及错误诊断，同时保留一秒心跳与普通对话语义。

**Architecture:** 三条独立实现线分别负责结构化输出、图片链路、工具/流可靠性，在独立 worktree 中按 TDD 完成并提交；集成分支逐个 cherry-pick，解决共享 handler 冲突后补齐日志与管理端分类。所有透明重试都受“尚未交付语义输出且最多一次”约束。

**Tech Stack:** Rust 2024、Axum 0.8、Tokio、reqwest、serde_json、image、SQLite、React 19、TypeScript、Bun。

---

### Task 1: 结构化输出唯一 JSON 归一化

**Files:**
- Modify: `src/anthropic/structured_output.rs`
- Modify: `src/anthropic/exact_output.rs`
- Test: `src/anthropic/structured_output.rs`

目标 API：

```rust
pub(crate) fn extract_single_json_bounded(
    text: &str,
    max_text_bytes: usize,
    max_json_bytes: usize,
) -> Option<String>;

pub(crate) fn validate_output_json(
    text: &str,
    format: &OutputFormat,
) -> Result<serde_json::Value, StructuredOutputError>;
```

`validate_output_json` 必须先尝试整段 parse；失败后调用 bounded extractor；两个入口最终都进入同一个 `validate_and_repair` Schema 检查，但 `Repaired` 结果仍按现有契约拒绝。

- [ ] **Step 1: 写 RED 测试**：增加围栏 JSON、说明文字包裹唯一 JSON、多 JSON、Schema 不匹配和 1 MiB 边界测试；围栏/唯一候选当前必须失败。
- [ ] **Step 2: 运行 RED**：`cargo test --locked --no-default-features anthropic::structured_output::tests -- --nocapture`，确认新成功用例因整段 `serde_json::from_str` 失败。
- [ ] **Step 3: 最小实现**：先走精确 JSON，再复用有界唯一 JSON 提取；候选必须通过原 Schema，返回规范化 JSON。
- [ ] **Step 4: 运行 GREEN**：同一命令全部通过，并运行 `cargo test --locked --no-default-features structured_output -- --nocapture`。
- [ ] **Step 5: 提交**：`git commit -m "fix(output): 恢复唯一结构化JSON"`。

### Task 2: 图片软预算和独立硬上限

**Files:**
- Modify: `src/kiro/image_budget.rs`
- Modify: `src/model/config.rs`
- Modify: `src/kiro/provider.rs`
- Modify: `src/admin/types.rs`
- Modify: `src/admin/service.rs`
- Modify: `admin-ui/src/types/api.ts`
- Modify: `admin-ui/src/lib/image-budget.ts`
- Modify: `admin-ui/src/components/image-budget-dialog.tsx`
- Test: Rust 对应模块与 `admin-ui/src/lib/image-budget.test.ts`

策略类型必须扩展为：

```rust
pub struct ImageBudgetPolicy {
    pub enabled: bool,
    pub total_base64_budget_bytes: usize,
    pub hard_base64_limit_bytes: usize,
    pub history_max_dimension: u32,
    pub history_jpeg_quality: u8,
    pub retry_history_max_dimension: u32,
    pub retry_history_jpeg_quality: u8,
}
```

配置 JSON 字段固定为 `imageHardBase64LimitBytes`，默认 `8 * 1024 * 1024`，并满足 `256 KiB <= soft <= hard <= 32 MiB`。`PreparedKiroBodies.primary_body` 的选择规则固定为：普通体未超 hard 时保持普通体；普通体超 hard 且激进体未超 hard 时使用激进体；两者都超 hard 才返回 `Exceeded`。

- [ ] **Step 1: 写 RED 测试**：覆盖普通体 944 KiB、软预算 800 KiB、硬上限 8 MiB 时仍生成可发送 primary/retry；普通超硬而激进未超时选择激进 primary；两体都超硬才 `Exceeded`；当前轮哈希不变。
- [ ] **Step 2: 运行 RED**：`cargo test --locked --no-default-features image_budget -- --nocapture`，确认当前普通体超过软预算立即返回 `Exceeded`。
- [ ] **Step 3: 实现策略**：新增 `hard_base64_limit_bytes`，软目标仅决定是否准备激进体；根据普通/激进体与硬上限选择 primary 和 threshold retry。
- [ ] **Step 4: 配置和 UI**：新增 `imageHardBase64LimitBytes`，校验 `soft <= hard`，支持热更新、持久化和管理端编辑。
- [ ] **Step 5: 运行 GREEN**：Rust 图片/配置/管理服务测试与 `bun test src/lib/image-budget.test.ts` 全部通过。
- [ ] **Step 6: 提交**：`git commit -m "fix(image): 将图片总量改为软预算"`。

### Task 3: 图片格式验证和无损归一化

**Files:**
- Modify: `src/image_resize.rs`
- Modify: `src/anthropic/converter.rs`
- Modify: `src/anthropic/handlers.rs`
- Test: `src/image_resize.rs`、`src/anthropic/converter.rs`

新增的图片预检返回类型必须携带安全元数据而不携带 Base64：

```rust
pub struct ValidatedImage {
    pub format: String,
    pub data_base64: String,
    pub normalized: bool,
}

pub enum ImageValidationError {
    InvalidBase64,
    UnsupportedFormat,
    DecodeFailed,
}

pub fn validate_image_for_upstream(
    declared_format: &str,
    data_base64: &str,
) -> Result<ValidatedImage, ImageValidationError>;
```

magic bytes 与声明一致且可解码时允许原样通过；声明不一致时修正 format；需要标准化的静态图编码为 PNG。错误对外只包含消息序号、图片序号和错误类别。

- [ ] **Step 1: 写 RED 测试**：声明 PNG/真实 JPEG、可解码特殊 PNG、截断 PNG、当前轮图片和 tool_result 嵌套图片分别覆盖。
- [ ] **Step 2: 运行 RED**：确认当前 converter 只相信 media type，损坏图会进入 Kiro 请求。
- [ ] **Step 3: 最小实现**：magic bytes 修正格式；需要时无损重编码标准 PNG；损坏图返回带消息/图片索引的 typed conversion error，禁止记录内容。
- [ ] **Step 4: 运行 GREEN**：`cargo test --locked --no-default-features image_resize -- --nocapture` 与 converter 图片测试通过。
- [ ] **Step 5: 提交**：`git commit -m "fix(image): 预检并归一化上游图片"`。

### Task 4: 工具 Schema 校验前原子缓冲与恢复

**Files:**
- Modify: `src/anthropic/tool_attempt.rs`
- Modify: `src/anthropic/stream.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/tool_schema.rs`
- Test: 上述模块内测试

重试判定必须保持以下不变量：

```rust
retry = attempt_index == 0
    && termination == AttemptTermination::Eof
    && !semantic_output_started
    && !tool_forwarded
    && matches!(failure, Some(AttemptFailure::InvalidToolSchema { .. }));
```

`ProbationBuffer::push_all` 在同一批事件包含工具 Schema terminal error 时不得先提交该批工具事件。retry body 只允许把公开的 required/type 约束追加到失败工具 description，不得读取或猜测工具参数值。

- [ ] **Step 1: 写 RED 测试**：首轮 `Edit` 缺 `file_path`、第二轮合法时只交付第二轮；`AskUserQuestion.questions` 错型；已有正文后非法工具不重试；两轮非法只发一次 error。
- [ ] **Step 2: 运行 RED**：确认当前 complete tool block 先把 probation 标为 committed，attempt 保持 1。
- [ ] **Step 3: 最小实现**：完整工具块通过 Schema 前不提交；允许首轮未提交 Schema failure 生成差异化 retry body；不猜 required 值。
- [ ] **Step 4: 运行 GREEN**：tool_attempt、tool_schema、handlers 相关测试全部通过。
- [ ] **Step 5: 提交**：`git commit -m "fix(tool): 在提交前恢复非法工具参数"`。

### Task 5: 120 秒流竞态和零输出安全重试

**Files:**
- Modify: `src/http_client.rs`
- Modify: `src/kiro/provider.rs`
- Modify: `src/anthropic/tool_attempt.rs`
- Modify: `src/anthropic/handlers.rs`
- Test: 对应模块内测试

超时重试不变量：

```rust
let retryable_transport = matches!(
    termination,
    AttemptTermination::ReadError(_) | AttemptTermination::IdleTimeout
);
let retry = attempt_index == 0
    && retryable_transport
    && !semantic_output_started
    && !tool_forwarded;
```

provider 构建业务 HTTP client 时不再把 `streamIdleTimeoutSecs` 同时传给 reqwest `read_timeout`；绝对 720 秒超时保留，流层 watchdog 继续读取运行时配置。

- [ ] **Step 1: 写 RED 测试**：底层 read timeout 不得与 watchdog 相同；零字节 ReadError/IdleTimeout 首轮可重试；部分输出、第二轮和 ClientClosed 不重试；非流式未交付可重试。
- [ ] **Step 2: 运行 RED**：确认当前 `ReadError/IdleTimeout` 不在重试集合且 provider 配置相同 120 秒 read timeout。
- [ ] **Step 3: 最小实现**：由应用 watchdog 主导流空闲分类；只扩展未提交 attempt 的一次重试，不改变部分流行为。
- [ ] **Step 4: 运行 GREEN**：相关单测和一秒 connected/ping 测试通过。
- [ ] **Step 5: 提交**：`git commit -m "fix(stream): 恢复零输出超时请求"`。

### Task 6: 安全诊断、日志过滤与管理端分类

**Files:**
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/error_snapshot.rs`
- Modify: `src/admin/trace_db.rs`
- Modify: `src/main.rs`
- Modify: `admin-ui/src/components/error-snapshot-page.tsx`
- Modify: `admin-ui/src/components/error-snapshot-dialog.tsx`
- Modify: `admin-ui/src/lib/error-snapshot-utils.ts`
- Modify: `admin-ui/src/lib/error-snapshot-utils.test.ts`
- Test: Rust 快照/trace 测试与相关前端测试

管理端分类函数固定为：

```ts
export type SnapshotDisposition = 'recovered' | 'client_disconnected' | 'final_error'

export function snapshotDisposition(record: ErrorSnapshotSummary): SnapshotDisposition {
  if (record.recovered) return 'recovered'
  if (record.errorType === 'client_disconnected') return 'client_disconnected'
  return 'final_error'
}
```

tracing filter helper在用户没有显式覆盖模块时追加 `h2=info,hyper=info,reqwest=info`；显式 `h2=trace` 等用户指令优先。图片本地错误使用 `image_budget_exceeded`，保存 count/history/current/before/after/soft/hard 数值字段，不保存正文和 Base64。

- [ ] **Step 1: 写 RED 测试**：图片本地 400 保存 `image_budget_exceeded` 和字节统计；timeout 保存安全 error chain 分类；recovered/client disconnected/final error 分组互斥；默认 h2/hyper/reqwest 不输出 frame DEBUG。
- [ ] **Step 2: 运行 RED**：确认当前本地 400 只有通用文案、管理端未分组。
- [ ] **Step 3: 实现**：只记录计数、字节、类型、哈希和有界安全摘要；调整模块过滤与 UI 分类，不读取正文。
- [ ] **Step 4: 运行 GREEN**：快照、trace、前端测试和构建通过。
- [ ] **Step 5: 提交**：`git commit -m "feat(logging): 完善可靠性错误分类"`。

### Task 7: 集成、回归和 8991 验收

**Files:**
- Modify: `docs/analysis/2026-07-15-8991-production-two-hour-error-analysis.md`
- Modify: `docs/2026-07-14-completed-work-summary.md`

- [ ] **Step 1: 集成提交**：逐个 cherry-pick 三条实现线，冲突按本设计的重试不变量解决。
- [ ] **Step 2: 完整验证**：运行 `cargo fmt -- --check`、`cargo test -j 1 --bin kiro-rs --locked --no-default-features`、`cargo check --all-targets --locked --no-default-features`、`bun test`、`bun run build`、`git diff --check`。
- [ ] **Step 3: 安全扫描**：扫描 Key、Authorization、Cookie、客户正文 fixture 和图片 Base64，确认无敏感材料进入 Git。
- [ ] **Step 4: 部署 8991**：只构建隔离测试镜像，不修改 8990；保留 DEBUG 但过滤底层 frame 噪声。
- [ ] **Step 5: 实网验收**：结构化输出、工具、944 KiB 图片、损坏图片、零输出超时和一秒 ping 各执行回归；观察错误快照至少 30 分钟。
- [ ] **Step 6: 本地提交并合并**：验证通过后提交集成结果并合并回本地 `master`，不推 GitHub，除非用户另行要求。
