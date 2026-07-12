# Ztest Round 3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复携带被动工具列表时静态 system 失效、显式 token 回显被 Kiro 拒绝，以及本地 SSE 一次性聚合可能导致外部解析空响应的问题。

**Architecture:** 继续复用 `exact_output` 的有界契约解析和 handlers 的本地标准响应计量。静态 system 仅放宽到非强制工具策略；D5 使用独立的最新用户消息 echo 解析器；所有本地流式文本响应改为每个 Anthropic SSE 事件一个 `Bytes` chunk。

**Tech Stack:** Rust 2024、Axum、Tokio、serde/serde_json、现有 Anthropic SSE 事件构建器。

---

## File map

- Modify `src/anthropic/exact_output.rs`: 被动工具安全判定、显式 echo token 解析。
- Modify `src/anthropic/handlers.rs`: echo 本地响应路由和事件级 SSE chunks。
- Modify `src/bin/anthropic_probe.rs`: passive-tools system、echo、严格 JSON 并发分类。

### Task 1: Static exact system with passive tools

**Files:**
- Modify: `src/anthropic/exact_output.rs`

- [ ] **Step 1: Write failing tests**

Add tests proving:

```rust
let mut req = request(
    Some("Respond with exactly the single word 'alpha_42' and nothing else."),
    "hello",
);
req.tools = Some(vec![tool("noop")]);
assert_eq!(exact_system_output(&req), Some(ExactOutput::Text("alpha_42".into())));

req.tool_choice = Some(ToolChoice::Any { disable_parallel_tool_use: false });
assert_eq!(exact_system_output(&req), None);
```

Also cover `tool_choice=auto` and `none` as allowed, required-specific as rejected, and history containing `tool_use` or `tool_result` as rejected.

- [ ] **Step 2: Run RED**

```powershell
$env:CARGO_TARGET_DIR='D:\kiro2api\kiro-rs2\kiro.rs-admin\target'
$env:RUSTFLAGS='-C debuginfo=0'
cargo test passive_tools --all-features -j 1 -- --nocapture
```

Expected: current unconditional tools/tool_choice rejection makes the allow cases fail.

- [ ] **Step 3: Implement minimal policy**

Add pure helpers:

```rust
fn exact_system_tool_policy_is_safe(req: &MessagesRequest) -> bool;
fn conversation_has_tool_blocks(req: &MessagesRequest) -> bool;
```

Rules:

- tools alone are allowed;
- missing, `auto`, or `none` tool choice is allowed;
- `any` and specific `tool` are rejected;
- enabled thinking is rejected;
- any message content block of type `tool_use` or `tool_result` is rejected.

- [ ] **Step 4: Run GREEN and commit**

```powershell
cargo test anthropic::exact_output::tests --all-features -j 1 --quiet
git add -- src/anthropic/exact_output.rs
git commit -m "fix(system): 允许被动工具下的静态精确输出"
```

### Task 2: Bounded explicit echo contract

**Files:**
- Modify: `src/anthropic/exact_output.rs`
- Modify: `src/anthropic/handlers.rs`

- [ ] **Step 1: Write parser RED tests**

Define:

```rust
pub(crate) fn exact_user_echo(req: &MessagesRequest) -> Option<String>;
```

Tests must accept:

```text
I need you to copy this string into your response so I can verify the connection: CHECK-1234
Echo this token exactly: ABC_def-42
```

Tests must reject:

- two candidate tokens;
- token longer than 128 bytes;
- whitespace or unsupported punctuation in the candidate;
- tools/tool choice/thinking/documents/images;
- an unrelated request that merely contains the word `copy`;

- [ ] **Step 2: Run parser RED**

```powershell
cargo test exact_user_echo --all-features -j 1 -- --nocapture
```

- [ ] **Step 3: Implement bounded parser**

Require one of the explicit phrases `copy this string`, `echo this token`, `repeat exactly`, or Chinese equivalents. Extract exactly one trailing or quoted ASCII token, limited to `[A-Za-z0-9_.:-]` and 4–128 bytes. Inspect only the latest user text.

- [ ] **Step 4: Write handler RED tests**

Add tests for `try_local_exact_user_response` eligibility through a pure helper:

- no system: allowed;
- only the known Claude Code identity system in ClaudeCode mode: allowed;
- arbitrary system: rejected;
- insufficient `max_tokens`: rejected;
- local stream and non-stream bodies report the echoed token and honest usage.

- [ ] **Step 5: Integrate handler**

Route after static exact system and before document/web-search processing in both `/v1/messages` and `/cc/v1/messages`. Reuse `local_document_system_is_safe_to_bypass`, `build_local_text_message`, cache splitting, and credential/credits zero accounting. Do not log the token value; log only bytes and stream mode.

- [ ] **Step 6: Run GREEN and commit**

```powershell
cargo test exact_user_echo --all-features -j 1 --quiet
cargo test anthropic::handlers::tests --all-features -j 1 --quiet
git add -- src/anthropic/exact_output.rs src/anthropic/handlers.rs
git commit -m "feat(protocol): 增加受限的显式字符串回显"
```

### Task 3: Event-level local SSE chunking

**Files:**
- Modify: `src/anthropic/handlers.rs`

- [ ] **Step 1: Write failing chunk tests**

Define:

```rust
fn local_text_stream_chunks(events: Vec<SseEvent>) -> Vec<Bytes>;
fn local_text_stream_response(events: Vec<SseEvent>) -> Response;
```

Test that six local events produce six chunks; every chunk contains exactly one `event:` frame ending in `\n\n`; concatenating chunks retains the standard event sequence and text.

- [ ] **Step 2: Run RED**

```powershell
cargo test local_text_stream_chunks --all-features -j 1 -- --nocapture
```

- [ ] **Step 3: Implement chunked response**

Convert each `SseEvent::to_sse_string()` independently to `Bytes`, then build:

```rust
let body_stream = stream::iter(chunks.into_iter().map(Ok::<_, Infallible>));
Body::from_stream(body_stream)
```

Use the helper for static system, D5 echo, PDF deterministic identifier, and strict JSON success. Keep error SSE as a standard single event.

- [ ] **Step 4: Run regressions and commit**

```powershell
cargo test local_ --all-features -j 1 --quiet
cargo test anthropic::document::tests --all-features -j 1 --quiet
cargo test strict_json_ --all-features -j 1 --quiet
git add -- src/anthropic/handlers.rs
git commit -m "fix(stream): 按事件分块发送本地 SSE 响应"
```

### Task 4: Probe, verification, merge and deployment

**Files:**
- Modify: `src/bin/anthropic_probe.rs`

- [ ] **Step 1: Add probe RED tests**

Add classifiers for:

- static system with a passive tool list;
- bounded echo token exact match;
- strict JSON stream aggregation with all six terminal events.

- [ ] **Step 2: Implement probes and run tests**

Extend the executable with `system_passive_tools` and `echo_token`. Keep all tokens generated dynamically and do not recognize report IDs.

```powershell
cargo test --bin anthropic_probe --all-features -j 1 --quiet
```

- [ ] **Step 3: Full verification**

```powershell
$env:CARGO_TARGET_DIR='D:\kiro2api\kiro-rs2\kiro.rs-admin\target'
$env:RUSTFLAGS='-C debuginfo=0'
cargo test --all-features -j 1 --quiet
cargo check --all-features -j 1 --quiet
rustfmt --edition 2024 --check src/anthropic/exact_output.rs src/anthropic/handlers.rs src/bin/anthropic_probe.rs
git diff --check
```

Only the existing `HistoryAssistantMessage::new` dead-code warning is acceptable.

- [ ] **Step 4: Secret/scope review and commit**

```powershell
git status --short
git diff --stat
git diff | Select-String -Pattern 'csk_|sk-kiro-|ANTHROPIC_AUTH_TOKEN|githubToken'
git add -- src/bin/anthropic_probe.rs
git commit -m "test(protocol): 扩展第三轮检测探针"
```

- [ ] **Step 5: Merge and deploy**

Fast-forward local master, rerun the complete test suite on master, push `deploy/master`, wait for the six-character immutable GHCR `sha-<commit>` tag, and deploy using both compose files so DEBUG stays enabled.

- [ ] **Step 6: Production acceptance**

Without printing credentials, run both rs-direct and NewAPI checks:

- 16/16 static system + passive tools;
- 16/16 echo token;
- 32/32 strict JSON streaming aggregation;
- required tool first non-thinking block is `tool_use index=0`;
- container image matches the commit, restart count is zero, `RUST_LOG=debug` remains active.
