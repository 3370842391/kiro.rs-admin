# Ztest Round 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make required tool calls start with a structured tool block, execute unambiguous static exact-output system contracts, and recover strict JSON output once before exposing malformed content.

**Architecture:** Add a focused `exact_output` module for contract recognition and JSON extraction. Keep event ordering in `stream.rs`; route local/static and strict-buffered responses from `handlers.rs`; reuse the existing Anthropic response builders and `BufferedStreamContext`.

**Tech Stack:** Rust 2024, Axum, Tokio, serde/serde_json, existing Kiro event decoder and Anthropic SSE state machine.

---

## File map

- Create `src/anthropic/exact_output.rs`: pure exact-system/strict-JSON recognition and JSON extraction.
- Modify `src/anthropic/mod.rs`: register the module.
- Modify `src/anthropic/converter.rs`: expose required-tool policy detection.
- Modify `src/anthropic/stream.rs`: required-tool preamble buffering and lazy first block.
- Modify `src/anthropic/handlers.rs`: generic local response, non-stream tool normalization, strict JSON buffering/retry.
- Modify `src/bin/anthropic_probe.rs`: first-block tool, exact-system, and strict-JSON probes.

### Task 1: Exact-output contract parser

**Files:**
- Create: `src/anthropic/exact_output.rs`
- Modify: `src/anthropic/mod.rs`

- [ ] **Step 1: Register the module and write failing tests**

Add `pub(crate) mod exact_output;` to `src/anthropic/mod.rs`. Define tests against:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExactOutput { Text(String), Json(String) }

pub(crate) fn exact_system_output(req: &MessagesRequest) -> Option<ExactOutput>;
pub(crate) fn strict_json_requested(req: &MessagesRequest) -> bool;
pub(crate) fn extract_single_json(text: &str) -> Option<String>;
pub(crate) fn append_strict_json_retry_instruction(request_body: &str) -> Option<String>;
```

Tests must cover:

```rust
#[test]
fn parses_static_literal_and_json() {
    assert_eq!(
        exact_system_output(&request_with_system(
            "Respond with exactly the single word 'alpha_42' and nothing else. No explanation."
        )),
        Some(ExactOutput::Text("alpha_42".into()))
    );
    assert_eq!(
        exact_system_output(&request_with_system(
            "Respond with exactly this JSON object, no markdown, no extra text:\n{\"a\": 330, \"b\": 360}"
        )),
        Some(ExactOutput::Json("{\"a\":330,\"b\":360}".into()))
    );
}

#[test]
fn rejects_identity_dynamic_and_ambiguous_contracts() {
    assert_eq!(exact_system_output(&request_with_system("You are CodeAssist v2.")), None);
    assert_eq!(exact_system_output(&request_with_system("Return exactly the current date.")), None);
    assert_eq!(exact_system_output(&request_with_system("Return exactly 'a' or 'b'.")), None);
}

#[test]
fn extracts_one_balanced_json_only() {
    assert_eq!(extract_single_json("prefix ```json\n{\"a\":1}\n```"), Some("{\"a\":1}".into()));
    assert_eq!(extract_single_json("{\"a\":1"), None);
    assert_eq!(extract_single_json("{\"a\":1} {\"b\":2}"), None);
}
```

- [ ] **Step 2: Run RED**

```powershell
$env:CARGO_TARGET_DIR='D:\kiro2api\kiro-rs2\kiro.rs-admin\target'
cargo test anthropic::exact_output::tests --all-features -j 1 -- --nocapture
```

Expected: missing functions or failing assertions.

- [ ] **Step 3: Implement bounded parsing**

Use limits `MAX_LITERAL_BYTES=128` and `MAX_JSON_BYTES=8192`. Require both an exact cue (`exactly`, `single word`, `exactly this JSON`, `只返回`, `仅返回`) and a no-extra cue (`nothing else`, `no extra text`, `no explanation`, `no markdown`, `不要解释`).

The JSON scanner must track object/array nesting, quoted strings and escapes; accept exactly one complete value; parse with `serde_json` and compactly reserialize. Text targets must be one quoted ASCII token containing only alphanumeric or `-_.:`. Reject tools, tool choice, enabled thinking, identities, templates, multiple targets and over-limit output.

`strict_json_requested` inspects only the latest user text and requires exactly-one-JSON plus no-extra cues. `append_strict_json_retry_instruction` parses the Kiro request as `serde_json::Value`, appends a generic correction only to `/conversationState/currentMessage/userInputMessage/content`, and reserializes.

- [ ] **Step 4: Run GREEN and commit**

```powershell
cargo test anthropic::exact_output::tests --all-features -j 1 --quiet
git add -- src/anthropic/mod.rs src/anthropic/exact_output.rs
git commit -m "feat(protocol): 增加精确输出契约解析"
```

### Task 2: Required tool must be content block zero

**Files:**
- Modify: `src/anthropic/converter.rs`
- Modify: `src/anthropic/stream.rs`

- [ ] **Step 1: Write failing state-machine tests**

Create a required-specific `StreamContext`, feed narration then a complete native `ToolUseEvent`, finish, and assert the sequence begins:

```rust
assert_eq!(events[0].event, "message_start");
assert_eq!(events[1].data["content_block"]["type"], "tool_use");
assert_eq!(events[1].data["index"], 0);
assert!(!events.iter().any(|e| e.data["content_block"]["type"] == "text"));
```

Add reverse tests: Auto retains text-before-tool; required textual `<invoke>` still recovers at finish; required with no valid tool returns `upstream_tool_choice_error`.

- [ ] **Step 2: Run RED**

```powershell
cargo test anthropic::stream::tests::required_ --all-features -j 1 -- --nocapture
```

Expected: text index 0, tool index 1.

- [ ] **Step 3: Implement required-tool buffering**

Expose:

```rust
impl ToolChoicePolicy {
    pub(crate) fn is_required(&self) -> bool {
        matches!(self, Self::RequiredAny { .. } | Self::RequiredSpecific { .. })
    }
}
```

Add `required_tool_preamble: String` and `required_tool_preamble_released: bool` to `StreamContext`. In `generate_initial_events`, required mode returns after `message_start` without creating text. In `process_assistant_response`, after XML/identity filtering and before block creation:

```rust
if self.tool_choice_policy.is_required()
    && !self.saw_upstream_tool_use
    && !self.required_tool_preamble_released
{
    self.required_tool_preamble.push_str(content);
    return Vec::new();
}
```

On the first native tool fragment, discard the buffered narration and log only byte count. At finish, if no native tool was observed, release the buffer through `create_text_delta_events` before existing invoke drain/required validation.

- [ ] **Step 4: Run GREEN and commit**

```powershell
cargo test anthropic::stream::tests --all-features -j 1 --quiet
git add -- src/anthropic/converter.rs src/anthropic/stream.rs
git commit -m "fix(tool): 让强制工具调用成为首个内容块"
```

### Task 3: Exact system local response

**Files:**
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/exact_output.rs`

- [ ] **Step 1: Write failing response tests**

Rename PDF-specific response builders to generic builders and test both static text and JSON:

```rust
let body = build_local_text_message("claude-opus-4-8", "alpha_42", 20, &CacheUsage::default());
assert_eq!(body["content"][0]["text"], "alpha_42");

let events = build_local_text_stream_events(
    "claude-opus-4-8", "{\"a\":330}", 20, CacheUsage::default()
);
assert_eq!(events[2].data["delta"]["text"], "{\"a\":330}");
assert_eq!(events.last().unwrap().event, "message_stop");
```

Add helper tests proving strict static system returns locally, identity/dynamic system returns `None`, and insufficient `max_tokens` falls back.

- [ ] **Step 2: Run RED**

```powershell
cargo test anthropic::handlers::tests::exact_system_ --all-features -j 1 -- --nocapture
```

- [ ] **Step 3: Generalize and integrate**

Rename `build_local_document_message` to `build_local_text_message` and its stream equivalent; update PDF callers. Add `ExactOutput::as_str()` and `try_local_exact_system_response` immediately after provider/thinking setup and before document/web-search routing.

The helper must reuse current token/cache splitting, check `max_tokens`, record credential 0/credits 0, emit standard JSON or SSE, and log only output kind/byte count.

- [ ] **Step 4: Run GREEN and commit**

```powershell
cargo test anthropic::handlers::tests --all-features -j 1 --quiet
cargo test anthropic::document::tests --all-features -j 1 --quiet
git add -- src/anthropic/exact_output.rs src/anthropic/handlers.rs
git commit -m "feat(system): 执行静态精确输出契约"
```

### Task 4: Non-stream required tool consistency

**Files:**
- Modify: `src/anthropic/handlers.rs`

- [ ] **Step 1: Write a failing pure test**

```rust
let content = vec![
    json!({"type":"text","text":"I will call it."}),
    json!({"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}),
];
let filtered = normalize_required_tool_content(
    content,
    &ToolChoicePolicy::RequiredAny { disable_parallel_tool_use: false },
);
assert_eq!(filtered.len(), 1);
assert_eq!(filtered[0]["type"], "tool_use");
```

Also assert Auto retains both blocks and Required without a tool retains text for the existing validator to reject.

- [ ] **Step 2: Run RED, implement, and run GREEN**

```powershell
cargo test normalize_required_tool_content --all-features -j 1 -- --nocapture
```

Implement:

```rust
fn normalize_required_tool_content(content: Vec<Value>, policy: &ToolChoicePolicy) -> Vec<Value> {
    let has_tool = content.iter().any(|block| block["type"] == "tool_use");
    if policy.is_required() && has_tool {
        content.into_iter().filter(|block| block["type"] != "text").collect()
    } else {
        content
    }
}
```

Call it after `normalize_non_stream_content_blocks`, before thinking/tool validators.

- [ ] **Step 3: Commit**

```powershell
cargo test anthropic::handlers::tests --all-features -j 1 --quiet
git add -- src/anthropic/handlers.rs
git commit -m "fix(tool): 统一非流式强制工具输出"
```

### Task 5: Strict JSON buffered attempt and one retry

**Files:**
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/exact_output.rs`

- [ ] **Step 1: Write failing extraction/retry tests**

Add pure event text extraction tests:

```rust
assert_eq!(strict_json_from_events(&text_events("Working... {\"a\":1}")), Some("{\"a\":1}".into()));
assert_eq!(strict_json_from_events(&text_events("Working... {\"a\":")), None);
```

Add a provider-independent two-attempt fixture: first emits truncated JSON, second emits valid JSON. Assert two calls and final SSE containing only valid JSON. Add a two-failure fixture asserting `upstream_json_protocol_error`.

- [ ] **Step 2: Run RED**

```powershell
cargo test strict_json_ --all-features -j 1 -- --nocapture
```

- [ ] **Step 3: Implement a buffered attempt collector**

Use:

```rust
struct BufferedAttempt {
    events: Vec<SseEvent>,
    credential_id: u64,
    usage: TraceUsage,
    credits: f64,
    terminal_error: Option<String>,
}
```

`collect_buffered_attempt` calls `provider.call_api`, reads complete bytes, decodes frames with `EventStreamDecoder`, feeds `BufferedStreamContext`, finishes events, and returns usage without writing client bytes.

- [ ] **Step 4: Implement strict JSON routing**

Before ordinary stream/non-stream dispatch, only when `strict_json_requested` and there are no documents/tools/tool choice/thinking/web search:

1. collect original attempt;
2. accept unique valid JSON extracted from visible text;
3. otherwise append the generic correction and collect exactly one retry;
4. on success return generic local JSON/SSE using original client input/cache once and final output once;
5. on second failure return HTTP 502 JSON error or a standard SSE `error` event;
6. trace attempt classification/credential/credits without JSON values.

- [ ] **Step 5: Run GREEN and commit**

```powershell
cargo test strict_json_ --all-features -j 1 --quiet
cargo test buffered --all-features -j 1 --quiet
git add -- src/anthropic/exact_output.rs src/anthropic/handlers.rs
git commit -m "fix(json): 缓冲并恢复严格 JSON 输出"
```

### Task 6: Probe, verification, merge and deploy

**Files:**
- Modify: `src/bin/anthropic_probe.rs`

- [ ] **Step 1: Add probe classifier tests**

Require the first non-thinking content block to be `tool_use` for required tool responses. Add exact-system literal and strict-JSON field classifiers.

- [ ] **Step 2: Implement and run probe tests**

Extend probe output with `system_exact` and `strict_json`; make tool probe reject text-first responses.

```powershell
cargo test --bin anthropic_probe --all-features -j 1 --quiet
```

- [ ] **Step 3: Full verification**

```powershell
$env:CARGO_TARGET_DIR='D:\kiro2api\kiro-rs2\kiro.rs-admin\target'
cargo test --all-features -j 1 --quiet
cargo check --all-features -j 1
rustfmt --edition 2024 --check src/anthropic/exact_output.rs src/anthropic/converter.rs src/anthropic/stream.rs src/anthropic/handlers.rs src/bin/anthropic_probe.rs
git diff --check
```

Expected: zero failures; only the existing `HistoryAssistantMessage::new` warning is acceptable.

- [ ] **Step 4: Scope/secret review and commit**

```powershell
git status --short
git diff --stat
git diff | Select-String -Pattern 'csk_|sk-kiro-|ANTHROPIC_AUTH_TOKEN|githubToken'
git add -- src/bin/anthropic_probe.rs
git commit -m "test(protocol): 扩展第二轮本地检测探针"
```

- [ ] **Step 5: Merge and deploy**

Fast-forward into local `master`; rerun full tests on master; push `deploy/master`; wait for immutable `sha-<commit>` GHCR image; deploy with `docker-compose.yml` plus `docker-compose.debug.yml`. Keep DEBUG enabled and run rs-direct plus NewAPI raw SSE probes before requesting a new Ztest report.
