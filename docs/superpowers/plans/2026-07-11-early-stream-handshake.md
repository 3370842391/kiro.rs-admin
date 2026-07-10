# Rust SSE Early Stream Handshake Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make streaming requests immediately emit an SSE connection comment while preserving local HTTP errors, translating post-handshake upstream failures into valid stream errors, propagating cancellation, and measuring upstream versus client-visible first-byte latency separately.

**Architecture:** Add a disabled-by-default `earlyStreamHandshake` configuration flag read through `KiroProvider`. A generic pending-future stream emits `: connected` immediately and `: ping` once per second until the existing provider future completes; success is flattened into the existing SSE decoder, while failure becomes one sanitized Anthropic error event. `/v1/chat/completions` and `/v1/responses` reuse this Anthropic engine and therefore inherit the handshake through their existing translators; `/cc/v1/messages` keeps its await-first handshake. No detached task is created, so dropping the response body drops the provider future. Existing dirty workspace changes are preserved; implementation edits are made inline and are not auto-committed unless the user later requests it.

**Tech Stack:** Rust 2024, Axum 0.8, Tokio, futures streams, reqwest, rusqlite, existing React/TypeScript admin UI.

---

### Task 1: Add the guarded configuration switch

**Files:**
- Modify: `src/model/config.rs:341-382`
- Modify: `src/model/config.rs:466-523`
- Modify: `src/kiro/provider.rs:250-268`
- Modify: `config.example.json`

- [ ] **Step 1: Write failing configuration tests**

Append these tests to the existing `src/model/config.rs` test module (create a `#[cfg(test)] mod tests` at EOF only if none exists):

```rust
#[test]
fn early_stream_handshake_defaults_off() {
    let config: Config = serde_json::from_str("{}").unwrap();
    assert!(!config.early_stream_handshake);
}

#[test]
fn early_stream_handshake_accepts_camel_case_json() {
    let config: Config = serde_json::from_str(r#"{"earlyStreamHandshake":true}"#).unwrap();
    assert!(config.early_stream_handshake);
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```powershell
cargo test early_stream_handshake_ -- --nocapture
```

Expected: compilation fails because `Config` has no `early_stream_handshake` field.

- [ ] **Step 3: Implement the configuration field and provider accessor**

Add to `Config` after `stream_idle_timeout_secs`:

```rust
/// 流式请求是否在 Kiro 上游响应前立即提交 SSE，并用注释心跳保活。
/// false 保留真实上游 HTTP 状态；true 时提交后的上游错误改走 SSE error。
#[serde(default)]
pub early_stream_handshake: bool,
```

Add to `Config::default()` after `stream_idle_timeout_secs`:

```rust
early_stream_handshake: false,
```

Add to `KiroProvider` beside `stream_idle_timeout_secs()`:

```rust
pub fn early_stream_handshake(&self) -> bool {
    self.token_manager.config().early_stream_handshake
}
```

Add to `config.example.json`:

```json
"earlyStreamHandshake": false
```

- [ ] **Step 4: Run focused tests and verify GREEN**

Run:

```powershell
cargo test early_stream_handshake_ -- --nocapture
```

Expected: both tests pass.

### Task 2: Build the cancellable connection-comment stream

**Files:**
- Modify: `src/anthropic/handlers.rs:1-35`
- Modify: `src/anthropic/handlers.rs:893-1138`
- Test: `src/anthropic/handlers.rs:2047-end`

- [ ] **Step 1: Write failing tests for immediate comment, heartbeat, completion, and cancellation**

Add imports to the handler test module:

```rust
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use futures::{StreamExt, future};
```

Add tests against the wished-for `pending_call_stream` API:

```rust
#[tokio::test(start_paused = true)]
async fn pending_call_stream_emits_connected_then_ping() {
    let stream = pending_call_stream(future::pending::<Result<(), anyhow::Error>>());
    futures::pin_mut!(stream);

    assert_eq!(stream.next().await.unwrap().comment_bytes(), Some(&b": connected\n\n"[..]));
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(stream.next().await.unwrap().comment_bytes(), Some(&b": ping\n\n"[..]));
}

#[tokio::test]
async fn pending_call_stream_completes_after_connected() {
    let stream = pending_call_stream(async { Ok::<_, anyhow::Error>(7u8) });
    futures::pin_mut!(stream);

    assert!(matches!(stream.next().await, Some(PendingCallEvent::Comment(_))));
    assert!(matches!(stream.next().await, Some(PendingCallEvent::Complete(Ok(7)))));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn dropping_pending_call_stream_drops_the_provider_future() {
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
    }

    let dropped = Arc::new(AtomicBool::new(false));
    let guard = DropFlag(dropped.clone());
    let stream = pending_call_stream(async move {
        let _guard = guard;
        future::pending::<Result<(), anyhow::Error>>().await
    });
    futures::pin_mut!(stream);
    assert!(matches!(stream.next().await, Some(PendingCallEvent::Comment(_))));
    drop(stream);
    tokio::task::yield_now().await;
    assert!(dropped.load(Ordering::SeqCst));
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```powershell
cargo test pending_call_stream_ -- --nocapture
```

Expected: compilation fails because `PendingCallEvent` and `pending_call_stream` do not exist.

- [ ] **Step 3: Implement the generic pending-call stream without detached tasks**

Add imports for `Future` and `Pin`, then define near the stream constants:

```rust
const EARLY_CONNECTED_SSE: &[u8] = b": connected\n\n";
const EARLY_PING_SSE: &[u8] = b": ping\n\n";
const EARLY_PING_INTERVAL: Duration = Duration::from_secs(1);

enum PendingCallEvent<T> {
    Comment(Bytes),
    Complete(anyhow::Result<T>),
}

impl<T> PendingCallEvent<T> {
    #[cfg(test)]
    fn comment_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Comment(bytes) => Some(bytes.as_ref()),
            Self::Complete(_) => None,
        }
    }
}

fn pending_call_stream<F, T>(future: F) -> impl Stream<Item = PendingCallEvent<T>>
where
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    struct State<F> {
        future: Pin<Box<F>>,
        heartbeat: tokio::time::Interval,
        connected_sent: bool,
        completed: bool,
    }

    let heartbeat = tokio::time::interval_at(
        TokioInstant::now() + EARLY_PING_INTERVAL,
        EARLY_PING_INTERVAL,
    );
    stream::unfold(
        State {
            future: Box::pin(future),
            heartbeat,
            connected_sent: false,
            completed: false,
        },
        |mut state| async move {
            if state.completed {
                return None;
            }
            if !state.connected_sent {
                state.connected_sent = true;
                return Some((PendingCallEvent::Comment(Bytes::from_static(EARLY_CONNECTED_SSE)), state));
            }
            tokio::select! {
                result = &mut state.future => {
                    state.completed = true;
                    Some((PendingCallEvent::Complete(result), state))
                }
                _ = state.heartbeat.tick() => Some((
                    PendingCallEvent::Comment(Bytes::from_static(EARLY_PING_SSE)),
                    state,
                )),
            }
        },
    )
}
```

- [ ] **Step 4: Run the focused tests and verify GREEN**

Run:

```powershell
cargo test pending_call_stream_ -- --nocapture
```

Expected: all three tests pass. If the cancellation assertion fails because the pinned binding remains alive, put the stream in an inner scope and assert after the scope ends; do not add a background task.

### Task 3: Classify provider failures once and serialize valid SSE errors

**Files:**
- Modify: `src/anthropic/handlers.rs:252-380`
- Test: `src/anthropic/handlers.rs:2047-end`

- [ ] **Step 1: Write failing classification and SSE tests**

Add:

```rust
#[test]
fn provider_error_sse_is_sanitized_and_carries_upstream_status() {
    let bytes = provider_error_sse(
        anyhow::anyhow!("Bearer secret-token connection reset"),
        Some(429),
    );
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.starts_with("event: error\ndata: "));
    assert!(text.contains("\"upstream_status\":429"));
    assert!(!text.contains("secret-token"));
}

#[test]
fn provider_validation_error_keeps_invalid_request_classification() {
    let classified = classify_provider_error(&anyhow::anyhow!(
        "Expected toolResult blocks but found none"
    ));
    assert_eq!(classified.http_status, StatusCode::BAD_REQUEST);
    assert_eq!(classified.error_type, "invalid_request_error");
}
```

- [ ] **Step 2: Run the tests and verify RED**

Run:

```powershell
cargo test provider_error_ -- --nocapture
```

Expected: compilation fails because the classifier and SSE serializer do not exist.

- [ ] **Step 3: Extract shared classification and implement sanitized SSE serialization**

Introduce:

```rust
struct ClassifiedProviderError {
    http_status: StatusCode,
    error_type: &'static str,
    public_message: &'static str,
}

fn classify_provider_error(err: &Error) -> ClassifiedProviderError {
    let text = err.to_string();
    if text.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        return ClassifiedProviderError {
            http_status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            public_message: "Context window is full. Reduce conversation history, system prompt, or tools.",
        };
    }
    if text.contains("Input is too long") {
        return ClassifiedProviderError {
            http_status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            public_message: "Input is too long. Reduce the size of your messages.",
        };
    }
    if crate::kiro::endpoint::default_is_client_validation_error(&text) {
        return ClassifiedProviderError {
            http_status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            public_message: "Invalid message sequence: tool_use and tool_result blocks must be correctly paired and ordered.",
        };
    }
    ClassifiedProviderError {
        http_status: StatusCode::BAD_GATEWAY,
        error_type: "api_error",
        public_message: "Upstream API request failed.",
    }
}
```

Refactor `map_provider_error` to log the full internal error, call this classifier, and build the existing JSON HTTP response from the classified fields. Implement:

```rust
fn provider_error_sse(err: Error, upstream_status: Option<u16>) -> Bytes {
    let classified = classify_provider_error(&err);
    let mut error = serde_json::json!({
        "type": classified.error_type,
        "message": classified.public_message,
    });
    if let Some(status) = upstream_status {
        error["upstream_status"] = serde_json::json!(status);
    }
    SseEvent::new("error", serde_json::json!({"type": "error", "error": error}))
        .to_sse_string()
        .into()
}
```

Add a `RequestTracer::last_http_status()` helper that reads `attempts.last().and_then(|a| a.http_status)` so early error serialization can include a reliable upstream status without parsing strings.

- [ ] **Step 4: Run existing and new mapping tests**

Run:

```powershell
cargo test provider_error_ -- --nocapture
cargo test bedrock_client_validation_errors_map_to_400 -- --nocapture
cargo test generic_upstream_error_still_maps_to_502 -- --nocapture
```

Expected: all tests pass; generic client-facing messages contain no raw upstream secret text.

### Task 4: Integrate the early handshake into `/v1/messages`

**Files:**
- Modify: `src/anthropic/handlers.rs:835-962`
- Modify: `src/anthropic/handlers.rs:973-1138`
- Test: `src/anthropic/handlers.rs:2047-end`

- [ ] **Step 1: Write a failing stream-order test using a ready error future**

Extract the event-to-byte flattening into a testable `flatten_pending_call` helper, then add:

```rust
#[tokio::test]
async fn early_error_stream_sends_comment_then_error_without_message_start() {
    let stream = early_error_test_stream(anyhow::anyhow!("connection reset"), Some(502));
    futures::pin_mut!(stream);
    let first = stream.next().await.unwrap().unwrap();
    let second = stream.next().await.unwrap().unwrap();
    assert_eq!(first, Bytes::from_static(EARLY_CONNECTED_SSE));
    assert!(String::from_utf8_lossy(&second).starts_with("event: error\n"));
    assert!(!String::from_utf8_lossy(&second).contains("message_start"));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn flatten_pending_call_preserves_success_stream_order() {
    let inner: BoxByteStream = Box::pin(stream::iter(vec![
        Ok(Bytes::from_static(b"event: message_start\ndata: {}\n\n")),
        Ok(Bytes::from_static(b"event: content_block_delta\ndata: {}\n\n")),
    ]));
    let stream = flatten_pending_call_for_test(Ok(inner));
    futures::pin_mut!(stream);
    assert_eq!(stream.next().await.unwrap().unwrap(), Bytes::from_static(EARLY_CONNECTED_SSE));
    assert!(String::from_utf8_lossy(&stream.next().await.unwrap().unwrap()).contains("message_start"));
    assert!(String::from_utf8_lossy(&stream.next().await.unwrap().unwrap()).contains("content_block_delta"));
}
```

`early_error_test_stream` and `flatten_pending_call_for_test` are `#[cfg(test)]` wrappers over the same `pending_call_stream(...).map(...).flatten()` composition used in production; they must not duplicate the handshake state machine.

- [ ] **Step 2: Run the test and verify RED**

Run:

```powershell
cargo test early_error_stream_ -- --nocapture
cargo test flatten_pending_call_preserves_success_stream_order -- --nocapture
```

Expected: compilation fails because the flattening/helper path is absent.

- [ ] **Step 3: Implement the early stream composition**

Change the stream branch in `post_messages` to read `let early = provider.early_stream_handshake();` and pass owned request data into `handle_stream_request` when early mode is enabled. Keep the old await-first branch unchanged for `false`.

Add `create_early_sse_stream` with this composition shape:

```rust
type BoxByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, Infallible>> + Send>>;

let call = async move {
    provider
        .call_api_stream(&request_body, Some(tracer_for_call.as_ref()), group.as_deref())
        .await
};

pending_call_stream(call)
    .map(move |event| -> BoxByteStream {
        match event {
            PendingCallEvent::Comment(bytes) => Box::pin(stream::once(async move { Ok(bytes) })),
            PendingCallEvent::Complete(Ok(call_result)) => {
                let mut ctx = StreamContext::new_with_thinking(
                    model.clone(),
                    input_tokens,
                    thinking_enabled,
                    tool_name_map.clone(),
                    known_tool_names.clone(),
                );
                ctx.cache_usage = cache_usage;
                let initial_events = ctx.generate_initial_events();
                Box::pin(create_sse_stream(
                    call_result.response,
                    ctx,
                    initial_events,
                    hook.clone(),
                    call_result.credential_id,
                    tracer.clone(),
                    idle_timeout_secs,
                ))
            }
            PendingCallEvent::Complete(Err(err)) => {
                hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
                let upstream_status = tracer.last_http_status();
                let error_text = err.to_string();
                tracer.finalize(
                    "error",
                    last_attempt_outcome(&tracer),
                    Some(&error_text),
                    None,
                    TraceUsage::zero(),
                );
                Box::pin(stream::once(async move {
                    Ok(provider_error_sse(err, upstream_status))
                }))
            }
        }
    })
    .flatten()
```

The actual implementation should move a single owned setup object out on `Complete`, rather than clone large maps on every heartbeat. The map closure keeps `Option<EarlyStreamSetup>` and calls `.take().expect("setup consumed once")` only for `Complete`.

Return the same response headers as the old path. Do not change `/cc/v1/messages` or non-stream handling.

- [ ] **Step 4: Verify focused stream tests GREEN**

Run:

```powershell
cargo test pending_call_stream_ -- --nocapture
cargo test early_error_stream_ -- --nocapture
cargo test flatten_pending_call_preserves_success_stream_order -- --nocapture
```

Expected: all handshake, cancellation, success-order, and error-order tests pass. Early handshake remains disabled for `/cc/v1/messages`; Task 5's metric correction intentionally applies to both stream decoders so trace field semantics stay consistent across endpoints.

### Task 5: Separate upstream first byte from client-visible first content

**Files:**
- Modify: `src/anthropic/handlers.rs:124-243`
- Modify: `src/anthropic/handlers.rs:1015-1048`
- Modify: `src/anthropic/handlers.rs:1948-1985`
- Modify: `src/admin/trace_db.rs:80-140`
- Modify: `src/admin/trace_db.rs:253-292`
- Modify: `src/admin/trace_db.rs:333-365`
- Modify: `src/admin/trace_db.rs:498-533`
- Modify: `src/admin/trace_db.rs:730-748`
- Modify: `src/admin/handlers.rs:1450-1477`
- Modify: `admin-ui/src/types/api.ts:517-557`

- [ ] **Step 1: Write failing visible-event classification tests**

Add:

```rust
#[test]
fn only_non_empty_content_events_are_client_visible_first_tokens() {
    assert!(!is_client_visible_content(&SseEvent::new("message_start", serde_json::json!({}))));
    assert!(!is_client_visible_content(&SseEvent::new(
        "content_block_delta",
        serde_json::json!({"delta":{"type":"text_delta","text":""}}),
    )));
    assert!(is_client_visible_content(&SseEvent::new(
        "content_block_delta",
        serde_json::json!({"delta":{"type":"text_delta","text":"hi"}}),
    )));
    assert!(is_client_visible_content(&SseEvent::new(
        "content_block_start",
        serde_json::json!({"content_block":{"type":"tool_use","name":"Bash"}}),
    )));
}
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```powershell
cargo test only_non_empty_content_events_ -- --nocapture
```

Expected: compilation fails because `is_client_visible_content` does not exist.

- [ ] **Step 3: Implement timing fields and event classification**

Add `upstream_first_byte_at: Mutex<Option<Instant>>` to `RequestTracer`. Rename the current raw-chunk call to `mark_upstream_first_byte()`, and make `mark_first_token()` exclusively mark client-visible content.

Implement:

```rust
fn is_client_visible_content(event: &SseEvent) -> bool {
    if event.event == "content_block_start" {
        return event.data.pointer("/content_block/type").and_then(serde_json::Value::as_str)
            == Some("tool_use");
    }
    if event.event != "content_block_delta" {
        return false;
    }
    match event.data.pointer("/delta/type").and_then(serde_json::Value::as_str) {
        Some("text_delta") => event.data.pointer("/delta/text").and_then(serde_json::Value::as_str).is_some_and(|s| !s.is_empty()),
        Some("thinking_delta") => event.data.pointer("/delta/thinking").and_then(serde_json::Value::as_str).is_some_and(|s| !s.is_empty()),
        Some("input_json_delta") => event.data.pointer("/delta/partial_json").and_then(serde_json::Value::as_str).is_some_and(|s| !s.is_empty()),
        _ => false,
    }
}
```

In both normal and CC stream decoders:

```rust
tracer.mark_upstream_first_byte();
// after collecting events, before converting them to Bytes
if events.iter().any(is_client_visible_content) {
    tracer.mark_first_token();
}
```

Extend `TraceRecord` with `upstream_first_byte_ms: Option<u64>`, add nullable SQLite column `upstream_first_byte_ms`, update migration array length, INSERT placeholders/params, SELECT indexes, SCHEMA, and all test fixtures. Expose it as `upstreamFirstByteMs` in the Admin response and TypeScript `TraceRecord`.

- [ ] **Step 4: Add and run trace migration persistence tests**

Extend the existing in-memory TraceStore round-trip test so the sample contains:

```rust
first_token_ms: Some(3200),
upstream_first_byte_ms: Some(2800),
```

Assert both values survive `insert` and `query_paged`.

Run:

```powershell
cargo test only_non_empty_content_events_ -- --nocapture
cargo test trace_db -- --nocapture
```

Expected: visible-content classification and SQLite round-trip/migration tests pass.

### Task 6: Verify OpenAI compatibility with SSE comments and errors

**Files:**
- Modify: `src/openai/handlers.rs:1178-end` (tests only unless a test exposes a defect)

- [ ] **Step 1: Add comment-ignore regression tests**

Add:

```rust
#[test]
fn openai_stream_parsers_ignore_anthropic_sse_comments() {
    let input = concat!(
        ": connected\n\n",
        ": ping\n\n",
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n",
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"failed\",\"upstream_status\":502}}\n\n",
    );
    let chat = run_chat_translator(input, false);
    assert_eq!(chat.iter().filter(|v| v.get("error").is_some()).count(), 1);
    let responses = run_responses_translator(input);
    assert_eq!(responses.iter().filter(|(name, _)| name == "response.failed").count(), 1);
}
```

- [ ] **Step 2: Run the test**

Run:

```powershell
cargo test openai_stream_parsers_ignore_anthropic_sse_comments -- --nocapture
```

Expected: PASS with the existing parser. If it fails, make the minimal parser change so a frame with no `event:` and no `data:` returns `None`; do not forward comments as OpenAI chunks.

### Task 7: Full verification and local opt-in

**Files:**
- Modify after verification only: `config.json` (ignored runtime configuration)

- [ ] **Step 1: Format and inspect only intended changes**

Run:

```powershell
cargo fmt -- --check
git diff --check
git status --short
```

Expected: formatting and whitespace checks pass. Confirm pre-existing changes in workflows, admin update files, `src/http_client.rs`, and unrelated `src/kiro/provider.rs` hunks remain intact.

- [ ] **Step 2: Run all Rust tests**

Run:

```powershell
cargo test
```

Expected: exit code 0 and no failed tests.

- [ ] **Step 3: Build the release binary**

Run:

```powershell
cargo build --release
```

Expected: exit code 0 and a newly updated `target/release/kiro-rs.exe`.

- [ ] **Step 4: Opt in locally without restarting the active process**

Add to ignored `config.json`:

```json
"earlyStreamHandshake": true
```

Do not stop PID 26312 or replace the running service during implementation. Report that one controlled restart is required for the flag and new binary to take effect.

- [ ] **Step 5: Run a controlled benchmark after restart approval**

After the user authorizes restart, send three identical small streaming requests and record:

- HTTP response headers time;
- first non-empty SSE line time (expected below 100 ms locally);
- first `content_block_delta` time;
- error-stream behavior using a safe invalid-model request before the handshake and a test-only simulated provider failure for post-handshake behavior.

Expected: local validation still returns HTTP 4xx; valid early streams immediately begin with `: connected`; real content timing remains governed by Kiro; trace records show both `upstreamFirstByteMs` and `firstTokenMs`.

## Execution Notes

- The current worktree contains user-owned uncommitted changes, including the existing keep-alive optimization in `src/kiro/provider.rs`. Do not reset, checkout, or stage whole files.
- No automatic implementation commits are planned because the user did not request them and overlapping dirty files make whole-file staging unsafe.
- Apply each production change only after its focused test has failed for the expected missing behavior.
