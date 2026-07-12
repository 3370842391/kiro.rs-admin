# Ztest 01KXB8K4 Remaining Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 完成报告 01KXB8K4 已批准的三项通用兼容修复：Claude Code 身份锚点下的静态 exact system、快速本地 SSE 调度边界，以及可关闭的单轮 `ping -> pong` 健康契约。

**Architecture:** exact system 复用 converter 已有的精确身份行清理函数，并显式接收 `ToolCompatibilityMode`，避免放宽通用身份安全规则。所有本地确定性流式响应统一经过 2ms paced body stream 并带禁止代理缓冲的响应头。`localPingResponse` 存入 Config、默认开启，由 KiroProvider 暴露运行时只读值，handler 复用现有本地 message/SSE/usage 构造器。

**Tech Stack:** Rust 2024、Axum、Tokio、Futures、Serde、Reqwest、本地 `anthropic_probe`。

---

## File map

- Modify `src/anthropic/converter.rs`: 导出已有的精确 Claude Code 身份锚点清理函数供 exact system 复用。
- Modify `src/anthropic/exact_output.rs`: exact system 接收兼容模式；新增受限 ping 纯函数和拒绝边界测试。
- Modify `src/anthropic/handlers.rs`: 传递兼容模式，统一 paced SSE，接入 ping 本地响应。
- Modify `src/model/config.rs`: 增加 `localPingResponse`，默认 true，支持 camelCase 持久化。
- Modify `src/kiro/provider.rs`: 暴露 `local_ping_response()` 配置读取。
- Modify `src/bin/anthropic_probe.rs`: 增加 identity+passive-tools、transport yield 和 ping 稳定性探针。
- Modify `config.example.json`: 展示可关闭的本地 ping 契约。
- Modify `README.md`: 说明该契约边界和关闭方式。

### Task 1: Claude Code identity-aware exact system

**Files:**
- Modify: `src/anthropic/converter.rs`
- Modify: `src/anthropic/exact_output.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/bin/anthropic_probe.rs`

- [ ] **Step 1: Write exact-system RED tests**

在 `exact_output.rs` 测试中增加 `identity_and_exact_request`，并断言：

```rust
#[test]
fn claude_code_identity_anchor_allows_exact_contract_only_in_claude_code_mode() {
    let req = identity_and_exact_request(true);
    assert_eq!(
        exact_system_output(&req, ToolCompatibilityMode::ClaudeCode),
        Some(ExactOutput::Text("alpha_42".into()))
    );
    assert_eq!(exact_system_output(&req, ToolCompatibilityMode::Raw), None);
}

#[test]
fn arbitrary_identity_still_blocks_exact_contract() {
    let req = request(
        Some("You are CodeAssist v2.\nReturn exactly the single word 'alpha_42' and nothing else."),
        "hello",
    );
    assert_eq!(
        exact_system_output(&req, ToolCompatibilityMode::ClaudeCode),
        None
    );
}
```

`identity_and_exact_request(true)` 必须包含 optional tool 且无 required choice；另加同一 block 中 `identity\nexact` 的通过用例。现有 required tool、thinking 和 tool history 用例改为传 `ToolCompatibilityMode::ClaudeCode`，继续断言 None。

- [ ] **Step 2: Run RED**

```powershell
$env:CARGO_TARGET_DIR='D:\kiro2api\kiro-rs2\kiro.rs-admin\target'
$env:RUSTFLAGS='-C debuginfo=0'
$env:CARGO_INCREMENTAL='0'
cargo test claude_code_identity_anchor_allows --all-features -j 1 -- --nocapture
```

Expected: 编译失败，因为 `exact_system_output` 还不接收兼容模式。

- [ ] **Step 3: Reuse the converter sanitizer**

把 converter 中现有函数改为：

```rust
pub(crate) const CLAUDE_CODE_IDENTITY_ANCHOR: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

pub(crate) fn sanitize_system_for_kiro(
    text: &str,
    mode: ToolCompatibilityMode,
) -> Option<String>;
```

保持函数现有语义不变：仅 ClaudeCode 模式删除完全匹配的行，Raw 原样返回。

在 exact_output 中实现：

```rust
pub(crate) fn exact_system_output(
    req: &MessagesRequest,
    mode: ToolCompatibilityMode,
) -> Option<ExactOutput> {
    if !exact_system_tool_policy_is_safe(req) {
        return None;
    }
    let system = req.system.as_ref()?
        .iter()
        .filter_map(|message| super::converter::sanitize_system_for_kiro(&message.text, mode))
        .collect::<Vec<_>>()
        .join("\n");
    // 后续 exact/no-extra/unsafe/JSON/literal 检查保持现有实现。
}
```

不得修改 `has_unsafe_contract_cue` 的 `you are` 规则。

- [ ] **Step 4: Wire handler mode and run GREEN**

`local_exact_system_output`、`local_exact_system_answer`、`try_local_exact_system_response` 增加 `mode` 参数，两个 Messages handler 均传 `state.tool_compatibility_mode`。

```powershell
cargo test anthropic::exact_output::tests --all-features -j 1 --quiet
cargo test exact_system_ --all-features -j 1 --quiet
cargo test claude_code_mode_ --all-features -j 1 --quiet
```

Expected: 新旧 exact/converter 测试全部通过。

- [ ] **Step 5: Extend probe and commit**

新增 `identity_passive_tools_system_request`，system 使用两个 block：官方身份锚点和随机 exact nonce；新增探针名 `system_identity_passive_tools`，使用非流式响应验证只有一个精确文本块。

```powershell
cargo test --bin anthropic_probe --all-features -j 1 --quiet
git add -- src/anthropic/converter.rs src/anthropic/exact_output.rs src/anthropic/handlers.rs src/bin/anthropic_probe.rs
git diff --cached --check
git commit -m "fix(system): 兼容官方身份锚点下的精确输出"
```

### Task 2: Paced local SSE and anti-buffering headers

**Files:**
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/bin/anthropic_probe.rs`

- [ ] **Step 1: Write paced-stream RED test**

在 handler 测试中保留现有六事件完整性断言，并新增：

```rust
#[tokio::test]
async fn local_text_stream_inserts_a_pending_boundary_between_events() {
    use futures::FutureExt;
    let response = local_text_stream_response(build_local_text_stream_events(
        "claude-opus-4-8",
        "PACE-42",
        42,
        CacheUsage::default(),
    ));
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache, no-transform");
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    let mut chunks = response.into_body().into_data_stream();
    assert!(chunks.next().await.unwrap().is_ok());
    assert!(chunks.next().now_or_never().is_none());
    assert!(chunks.next().await.unwrap().is_ok());
}
```

- [ ] **Step 2: Run RED**

```powershell
cargo test local_text_stream_inserts_a_pending_boundary --all-features -j 1 -- --nocapture
```

Expected: 失败，因为第二个 `stream::iter` item 仍会在同一次 poll 立即 Ready，且 header 仍是 `no-cache`。

- [ ] **Step 3: Implement bounded pacing**

增加 `LOCAL_SSE_EVENT_DELAY = Duration::from_millis(2)`，将 `local_text_stream_response` 改为：

```rust
let body_stream = stream::unfold(
    (local_text_stream_chunks(events).into_iter(), true),
    |(mut chunks, first)| async move {
        let chunk = chunks.next()?;
        if !first {
            tokio::time::sleep(LOCAL_SSE_EVENT_DELAY).await;
        }
        Some((Ok::<_, Infallible>(chunk), (chunks, false)))
    },
);
```

响应头固定为：

```rust
.header(header::CACHE_CONTROL, "no-cache, no-transform")
.header("x-accel-buffering", "no")
.header(header::CONNECTION, "keep-alive")
```

不设置 Content-Length，不修改正常 Kiro 上游 stream。

- [ ] **Step 4: Extend transport-yield probe**

把 probe 的 `post_stream_events` 返回值改为：

```rust
struct StreamCapture {
    events: Vec<Value>,
    transport_yields: usize,
}
```

每次 `bytes_stream().next()` 递增 transport_yields。`strict_json_stream_probe` 同时要求事件分类通过且 `transport_yields >= 2`；普通 `stream_probe` 仍只检查协议事件，避免代理合法重分块造成非本地路径误报。

- [ ] **Step 5: Run GREEN and commit**

```powershell
cargo test local_text_stream_ --all-features -j 1 --quiet
cargo test --bin anthropic_probe --all-features -j 1 --quiet
git add -- src/anthropic/handlers.rs src/bin/anthropic_probe.rs
git diff --cached --check
git commit -m "fix(stream): 为本地SSE增加调度边界"
```

### Task 3: Configurable bounded ping health contract

**Files:**
- Modify: `src/model/config.rs`
- Modify: `src/kiro/provider.rs`
- Modify: `src/anthropic/exact_output.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/bin/anthropic_probe.rs`
- Modify: `config.example.json`
- Modify: `README.md`

- [ ] **Step 1: Write Config and eligibility RED tests**

Config 测试：空 JSON 默认 `local_ping_response == true`，`{"localPingResponse":false}` 可关闭并按 camelCase round trip。

exact_output 测试：

```rust
#[test]
fn ping_contract_accepts_only_a_single_plain_health_message() {
    assert_eq!(local_ping_answer(&request(None, " ping "), true), Some("pong"));
    assert_eq!(local_ping_answer(&request(None, "PING"), true), Some("pong"));
    assert_eq!(local_ping_answer(&request(None, "ping"), false), None);
}
```

分别构造并拒绝：system、两条消息、tools、tool_choice、thinking、image/document block、output_config、web search loop、`max_tokens=0`、`ping please`。

- [ ] **Step 2: Run RED**

```powershell
cargo test ping_contract_ --all-features -j 1 -- --nocapture
cargo test local_ping_response_ --all-features -j 1 -- --nocapture
```

Expected: 编译失败，Config 字段和 `local_ping_answer` 尚不存在。

- [ ] **Step 3: Add Config and provider getter**

Config 增加：

```rust
#[serde(default = "default_true")]
pub local_ping_response: bool,
```

`Config::default()` 填 true。KiroProvider 增加：

```rust
pub fn local_ping_response(&self) -> bool {
    self.token_manager.config().local_ping_response
}
```

- [ ] **Step 4: Implement pure eligibility**

在 exact_output.rs 增加：

```rust
pub(crate) fn local_ping_answer(req: &MessagesRequest, enabled: bool) -> Option<&'static str> {
    if !enabled
        || req.max_tokens < 1
        || req.system.is_some()
        || req.messages.len() != 1
        || req.tools.as_ref().is_some_and(|tools| !tools.is_empty())
        || req.tool_choice.is_some()
        || req.thinking.as_ref().is_some_and(|value| value.is_enabled())
        || req.output_config.is_some()
        || req.force_web_search_loop
        || conversation_has_non_text_content(req)
    {
        return None;
    }
    let message = &req.messages[0];
    (message.role == "user"
        && message_text(&message.content).trim().eq_ignore_ascii_case("ping"))
        .then_some("pong")
}
```

- [ ] **Step 5: Wire generic local response**

增加 `try_local_ping_response`，复用 `build_local_text_message`、`build_local_text_stream_events`、cache usage 拆分和 UsageRecordHook。放在 exact user 之后、strict JSON/PDF/上游之前。只在 `provider.local_ping_response()` 为 true 且纯函数命中时返回。

- [ ] **Step 6: Extend probe and docs**

probe 增加 `ping_health`：连续 20 次发送单条 `ping`，每次必须精确得到 `pong`；记录毫秒延迟并计算 CV，CV > 0.25 时失败并报告 mean/CV。README 和 config.example.json 说明默认开启、只匹配无 system/tools/thinking/历史/多模态的单轮 ping，设 `localPingResponse=false` 可关闭。

- [ ] **Step 7: Run GREEN and commit**

```powershell
cargo test ping_contract_ --all-features -j 1 --quiet
cargo test local_ping_response_ --all-features -j 1 --quiet
cargo test --bin anthropic_probe --all-features -j 1 --quiet
git add -- src/model/config.rs src/kiro/provider.rs src/anthropic/exact_output.rs src/anthropic/handlers.rs src/bin/anthropic_probe.rs config.example.json README.md
git diff --cached --check
git commit -m "feat(protocol): 增加可关闭的ping健康契约"
```

### Task 4: Full verification and handoff

**Files:**
- No new files

- [ ] **Step 1: Focused regression**

```powershell
$env:CARGO_TARGET_DIR='D:\kiro2api\kiro-rs2\kiro.rs-admin\target'
$env:RUSTFLAGS='-C debuginfo=0'
$env:CARGO_INCREMENTAL='0'
cargo test anthropic::exact_output::tests --all-features -j 1 --quiet
cargo test anthropic::handlers::tests --all-features -j 1 --quiet
cargo test anthropic::converter::tests --all-features -j 1 --quiet
cargo test --bin anthropic_probe --all-features -j 1 --quiet
```

- [ ] **Step 2: Full regression and build**

```powershell
cargo test --all-features -j 1 --quiet
cargo check --all-features -j 1 --quiet
cd admin-ui
bun test src/lib/cache-policy.test.ts
bun run build
cd ..
git diff --check
```

- [ ] **Step 3: Scope and secret review**

```powershell
git status --short
git diff --stat master...HEAD
git diff master...HEAD | Select-String -Pattern 'csk_|ANTHROPIC_AUTH_TOKEN|githubToken|profileArn'
```

Expected: 不含凭据、生产日志、trace、截图或构建产物；主目录未提交的报告标题修改不进入本分支。

- [ ] **Step 4: Local probe acceptance**

启动独立临时实例并用临时 Key 运行 `anthropic_probe`。必须通过：`system_identity_passive_tools`、`strict_json_stream`、`ping_health`、PDF、required tool、echo、SSE 顺序。若无可用上游凭据，至少完成所有本地确定性路径和自动化回归，并明确记录上游探针未执行。

- [ ] **Step 5: Finish branch**

确认 worktree clean，记录最终 commit。使用 `superpowers:finishing-a-development-branch` 提供本地合并、PR、保留或丢弃选项；不 push、不部署，除非用户明确授权。
