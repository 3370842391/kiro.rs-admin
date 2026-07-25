# Kiro Content Filter Terminal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 识别 Kiro `CONTENT_FILTERED` 终态并返回不可重试的 HTTP 400，避免误报空响应 502。

**Architecture:** 在 Kiro EventStream 模型层增加 `MetadataEvent`，由共享的 `AttemptObservation` 生成 `ContentFiltered` 失败类型。流式、非流式和严格 JSON 入口继续复用现有收尾逻辑，只在统一错误映射处区分 400 与 502。

**Tech Stack:** Rust、Axum、Serde、Tokio、现有 AWS EventStream 解析器与 Cargo 测试。

---

### Task 1: 解析 metadataEvent

**Files:**
- Create: `src/kiro/model/events/metadata.rs`
- Modify: `src/kiro/model/events/mod.rs`
- Modify: `src/kiro/model/events/base.rs`

- [ ] **Step 1: Write the failing parser tests**

在 `base.rs` 测试中断言 `metadataEvent` 映射为 `EventType::Metadata`，并用包含
`stopReason=CONTENT_FILTERED` 的 `Frame` 断言得到 `Event::Metadata`。

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_event_type_from_str -- --nocapture`

Expected: FAIL，因为 `EventType::Metadata` 尚不存在。

- [ ] **Step 3: Implement the event model**

新增：

```rust
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    #[serde(default, alias = "stop_reason")]
    pub stop_reason: String,
}
```

将其接入 `EventType::from_str`、`as_str` 和 `Event::from_frame`。

- [ ] **Step 4: Run parser tests**

Run: `cargo test kiro::model::events -- --nocapture`

Expected: PASS。

### Task 2: 分类内容过滤且禁止重试

**Files:**
- Modify: `src/anthropic/tool_attempt.rs`

- [ ] **Step 1: Write failing classification tests**

覆盖：无输出时得到 `AttemptFailure::ContentFiltered`；已有正文或完整工具时不返回该
失败；该失败的 `ToolAttemptState::should_retry()` 为 false。

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test content_filtered -- --nocapture`

Expected: FAIL，因为 `ContentFiltered` 尚未定义。

- [ ] **Step 3: Implement minimal classification**

为 `AttemptObservation` 增加内容过滤标记，在没有语义输出时生成
`AttemptFailure::ContentFiltered`。其稳定公开错误为：

```text
invalid_request_error
Request was blocked by upstream content filtering
```

不要把该类型加入重试允许列表。

- [ ] **Step 4: Run classification tests**

Run: `cargo test content_filtered -- --nocapture`

Expected: PASS。

### Task 3: 映射流式、非流式和严格 JSON 响应

**Files:**
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/stream.rs`

- [ ] **Step 1: Write failing response tests**

覆盖非流式 `ContentFiltered` 为 HTTP 400；流式 start gate 为 HTTP 400；严格 JSON
在首轮过滤后停止，调用次数为 1；普通 `EmptyResponse` 仍为 502 并允许一次重试。

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test content_filter -- --nocapture`

Expected: FAIL，当前公共响应映射固定为 502。

- [ ] **Step 3: Implement status mapping**

增加仅针对 `ContentFiltered` 的 HTTP 400 映射，把它加入严格 JSON 的终止失败集合。
流式 `StreamContext` 通过共享 observation 得到失败，不修改 SSE Ping 或首字门控。

- [ ] **Step 4: Run focused tests**

Run: `cargo test content_filter -- --nocapture`

Expected: PASS。

### Task 4: 回归验证

**Files:**
- Verify only

- [ ] **Step 1: Format check**

Run: `cargo fmt --all -- --check`

Expected: exit 0。

- [ ] **Step 2: Focused protocol suite**

Run: `cargo test tool_attempt -- --nocapture`

Expected: PASS。

- [ ] **Step 3: Full Rust suite**

Run: `cargo test --locked`

Expected: PASS，无失败测试。

- [ ] **Step 4: Review diff**

Run: `git diff --check` and `git status --short`

Expected: 无 whitespace 错误，只包含本计划文件、事件模型和 Anthropic 错误映射变更。
