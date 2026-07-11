# New API Visible Heartbeat Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep the immediate SSE comment handshake while changing the one-second wait heartbeat into a valid Anthropic `ping` data event that New API records and relays.

**Architecture:** The pending provider stream remains cancellation-bound to the response body. Only the heartbeat frame bytes change; `: connected` stays a comment, provider success still gates `message_start`, and provider failure still emits the existing sanitized SSE error. Rust metrics continue to mark only non-empty model content.

**Tech Stack:** Rust, Axum SSE byte streams, Tokio time, Futures streams, Cargo tests.

---

### Task 1: Specify the New API-visible heartbeat frame

**Files:**
- Modify: `src/anthropic/handlers.rs:1110-1112`
- Test: `src/anthropic/handlers.rs:2360-2383`

- [ ] **Step 1: Change the pending-stream test first**

Update `pending_call_stream_emits_connected_then_ping` so the first item must remain `: connected\n\n`, while the item after one second must equal:

```rust
b"event: ping\ndata: {\"type\":\"ping\"}\n\n"
```

Also assert the connected frame has no `data:` line and the heartbeat does, documenting the New API scanner boundary.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```powershell
cargo test anthropic::handlers::tests::pending_call_stream_emits_connected_then_ping -- --exact
```

Expected: FAIL because production still returns `: ping\n\n`.

- [ ] **Step 3: Implement the minimal heartbeat change**

Change only the heartbeat constant:

```rust
const EARLY_PING_SSE: &[u8] = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
```

Do not alter `EARLY_CONNECTED_SSE`, the one-second interval, provider future ownership, success ordering, or error mapping.

- [ ] **Step 4: Run the focused test and verify GREEN**

Run the same focused command. Expected: one test passes.

### Task 2: Protect OpenAI compatibility and metric semantics

**Files:**
- Modify: `src/openai/handlers.rs:1278-1296`
- Test: `src/openai/handlers.rs:1278-1296`
- Test: `src/anthropic/handlers.rs:2490-2510`

- [ ] **Step 1: Extend the compatibility fixture**

Replace the old `: ping\n\n` fixture with:

```text
event: ping
data: {"type":"ping"}

```

Rename the test to `openai_stream_parsers_ignore_anthropic_handshake_and_ping`. Keep the assertions that exactly one upstream error becomes exactly one Chat error and one Responses `response.failed`; add assertions that no emitted payload contains a `ping` type.

- [ ] **Step 2: Verify the OpenAI compatibility test**

Run:

```powershell
cargo test openai::handlers::tests::openai_stream_parsers_ignore_anthropic_handshake_and_ping -- --exact
```

Expected: PASS, proving both translators already ignore the standard Anthropic ping.

- [ ] **Step 3: Verify heartbeat is excluded from Rust first-token metrics**

Run:

```powershell
cargo test anthropic::handlers::tests::only_non_empty_content_events_are_client_visible_first_tokens -- --exact
```

Expected: PASS; `ping` is not a client-visible content event and does not mark `first_token_ms`.

### Task 3: Full verification and live protocol check

**Files:**
- No additional source changes.

- [ ] **Step 1: Run formatting/diff validation**

```powershell
git diff --check
```

Expected: exit code 0. Existing repository-wide rustfmt drift is out of scope.

- [ ] **Step 2: Run the complete Rust suite**

```powershell
cargo test
```

Expected: all tests pass with zero failures.

- [ ] **Step 3: Build the release binary**

```powershell
cargo build --release
```

Expected: exit code 0 and an updated `target/release/kiro-rs.exe`.

- [ ] **Step 4: Perform a local stream timing check**

With `earlyStreamHandshake=true`, issue a pending or naturally slow `/v1/messages` stream and record frames. Expected order:

```text
: connected
event: ping
data: {"type":"ping"}
event: message_start
...
event: message_stop
```

The first body should remain immediate, New API-compatible `data:` should arrive at about one second when upstream is still pending, and model content timing must remain separate.

- [ ] **Step 5: Review only task-owned diffs**

```powershell
git diff -- src/anthropic/handlers.rs src/openai/handlers.rs
git status --short
```

Confirm `Cargo.lock` and `admin-ui/src/components/image-update-dialog.tsx` remain untouched and unstaged.
