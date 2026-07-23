# Error Snapshot Safety Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep rare protocol/internal failure evidence while ensuring routine upstream failures, snapshot growth, and maintenance can never stall Kiro RS request handling.

**Architecture:** A capture-policy function separates routine provider outcomes from diagnostic failures before payload encoding. Snapshot writes use a fixed per-record budget and atomic capacity mode; bounded SQLite maintenance runs on Tokio's blocking pool and uses reusable-page metrics instead of physical file length as live usage.

**Tech Stack:** Rust 2024, Tokio, Axum, rusqlite/SQLite WAL, parking_lot, zstd, React/TypeScript admin contracts.

---

### Task 1: Capture policy and safe defaults

**Files:**
- Modify: `src/anthropic/error_snapshot.rs`
- Modify: `src/model/config.rs`
- Test: inline `#[cfg(test)]` modules in both files

- [ ] **Step 1: Add failing capture-policy tests**

Add tests that finalize snapshots with `auth_failed`, `quota_exhausted`, `account_throttled`,
`transient`, `network_error`, and `bad_request` outcomes and assert the snapshot store remains
empty, both for final failure and recovered success. Retain positive tests for
`upstream_tool_protocol_error`, `sse_state_error`, and interrupted streams.

```rust
#[test]
fn routine_upstream_auth_failure_stays_trace_only() {
    let (store, ctx) = test_context(true);
    ctx.record_attempt_status(0, Some(403), outcome::AUTH_FAILED);
    let id = ctx.finalize(SnapshotFinalState::error(outcome::AUTH_FAILED, Some(403))).unwrap();
    assert!(id.is_none());
    assert!(store.query_paged(&SnapshotQuery::default()).unwrap().records.is_empty());
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```powershell
cargo test anthropic::error_snapshot::tests::routine_upstream_auth_failure_stays_trace_only -- --exact
```

Expected: FAIL because current `finalize` stores `auth_failed` snapshots.

- [ ] **Step 3: Implement the explicit policy function**

Add a pure helper and call it before encoding payloads:

```rust
fn is_routine_trace_only_error(error_type: &str) -> bool {
    matches!(
        error_type,
        outcome::AUTH_FAILED
            | outcome::QUOTA_EXHAUSTED
            | outcome::ACCOUNT_THROTTLED
            | outcome::TRANSIENT
            | outcome::NETWORK_ERROR
            | outcome::BAD_REQUEST
    )
}
```

Return `Ok(None)` for routine outcomes unless a critical protocol diagnostic is present.

- [ ] **Step 4: Change and test safe defaults**

Set retention to 7 days, maximum storage to 5 GiB, recovered capture to false, and minimum free
space to 10 GiB. Update the existing camelCase round-trip test with those exact values.

- [ ] **Step 5: Run focused tests**

Run:

```powershell
cargo test anthropic::error_snapshot::tests model::config::tests::error_snapshot_defaults_are_safe_and_round_trip_in_camel_case
```

Expected: all selected tests PASS.

### Task 2: Bounded but useful diagnostic payloads and secret-safe logs

**Files:**
- Modify: `src/anthropic/error_snapshot.rs`
- Modify: `src/kiro/provider.rs`
- Test: inline tests in both files

- [ ] **Step 1: Add failing payload-budget tests**

Construct a request larger than 16 MiB with distinct head/tail markers plus protocol diagnostics.
Assert encoded original-byte totals never exceed `MAX_SNAPSHOT_DIAGNOSTIC_BYTES`, protocol data is
present, and the truncated envelope contains both markers, original length, and SHA-256.

- [ ] **Step 2: Run payload test and verify RED**

Run:

```powershell
cargo test anthropic::error_snapshot::tests::oversized_snapshot_preserves_diagnostics_and_head_tail_within_budget -- --exact
```

Expected: FAIL because there is currently only a per-part 16 MiB limit and no total budget.

- [ ] **Step 3: Implement prioritized budgeting**

Add:

```rust
pub const MAX_SNAPSHOT_DIAGNOSTIC_BYTES: usize = 16 * 1024 * 1024;
const RESERVED_DIAGNOSTIC_BYTES: usize = 2 * 1024 * 1024;

fn apply_snapshot_budget(payloads: Vec<RawSnapshotPayload>) -> Vec<RawSnapshotPayload>;
fn sampled_payload(raw: RawSnapshotPayload, budget: usize) -> RawSnapshotPayload;
```

Encode `ToolDiagnostics`, `InternalError`, and `StreamTail` first. Sanitize body payloads, retain
UTF-8-safe head/tail samples within remaining budget, and emit a JSON envelope containing
`truncated`, `original_bytes`, and `sha256`.

- [ ] **Step 4: Add failing authorization-redaction test**

Extract header rendering to a pure helper and assert `authorization`, `proxy-authorization`,
`x-api-key`, and token-like headers return `[REDACTED]`, while content-type remains visible.

- [ ] **Step 5: Implement provider header redaction and run tests**

Use the helper in the DEBUG header loop and run:

```powershell
cargo test anthropic::error_snapshot::tests kiro::provider::tests::debug_header_value_redacts_credentials
```

Expected: PASS with no secret value in formatted log fields.

### Task 3: Capacity admission and bounded SQLite maintenance

**Files:**
- Modify: `src/admin/error_snapshot_db.rs`
- Modify: `src/admin/handlers.rs`
- Modify: `admin-ui/src/types/api.ts`
- Test: inline Rust tests and TypeScript contract test

- [ ] **Step 1: Add failing capacity tests**

Cover full, critical-reserved, metadata-only, and disabled modes using fixed live-byte/free-space
probes. Add a fallback test proving hard-cap admission returns `SkippedCapacity` without writing a
fallback file.

- [ ] **Step 2: Add capacity state and outcomes**

Extend the enums and status contract:

```rust
pub enum CaptureMode { Full, CriticalOnly, MetadataOnly, Disabled }
pub enum InsertOutcome { Inserted(String), Existing(String), Fallback(String), SkippedCapacity }

pub struct StorageStatus {
    pub allocated_bytes: u64,
    pub live_bytes: u64,
    pub reusable_bytes: u64,
    pub skipped_capacity: u64,
    pub capture_mode: CaptureMode,
    // existing fields remain for compatibility
}
```

Compute SQLite live pages as `(page_count - freelist_count) * page_size`, include WAL/fallback
bytes, and store current mode/counters atomically. Re-evaluate under the existing connection mutex
before a write so concurrent captures cannot bypass the hard limit.

- [ ] **Step 3: Add a failing bounded-maintenance test**

Insert more than 512 eligible old records plus pinned and critical records. One maintenance batch
must delete no more than 512 and must preserve pinned/critical rows.

- [ ] **Step 4: Replace the unbounded loop**

Select at most 512 candidate IDs and delete them in one transaction. Remove automatic
`wal_checkpoint(TRUNCATE)` and `incremental_vacuum`. Return `needs_follow_up=true` when live bytes
remain above the 70% target so the scheduler can queue another bounded batch.

- [ ] **Step 5: Run storage and UI contract tests**

Run:

```powershell
cargo test admin::error_snapshot_db::tests
cd admin-ui; bun test src/components/error-snapshot-ui.contract.test.ts
```

Expected: all tests PASS.

### Task 4: Async maintenance isolation

**Files:**
- Create: `src/admin/error_snapshot_maintenance.rs`
- Modify: `src/admin/mod.rs`
- Modify: `src/main.rs`
- Modify: `src/admin/error_snapshot_db.rs`
- Test: inline Tokio tests in `src/admin/error_snapshot_maintenance.rs`

- [ ] **Step 1: Add a failing single-thread heartbeat test**

The test starts a deliberately blocking maintenance closure and a short Tokio heartbeat. It asserts
the heartbeat completes before maintenance, demonstrating that maintenance is offloaded.

- [ ] **Step 2: Implement blocking-pool scheduling**

Wrap maintenance and bounded trace-link reconciliation in `tokio::task::spawn_blocking`. Guard
with an atomic running flag. Schedule follow-up batches after a short async yield while
`needs_follow_up` is true; return to the hourly interval when healthy.

- [ ] **Step 3: Run scheduler and regression tests**

Run:

```powershell
cargo test error_snapshot_maintenance
cargo test admin::error_snapshot_db::tests anthropic::error_snapshot::tests
```

Expected: all selected tests PASS and the heartbeat test completes promptly.

### Task 5: Full verification, commit, merge, and production rollout

**Files:**
- Verify all modified Rust/TypeScript/docs files
- Build: versioned Docker image from repository root

- [ ] **Step 1: Format and run the complete test suite**

Run:

```powershell
cargo fmt --all -- --check
cargo test --no-fail-fast
cd admin-ui; bun test; bun run build
```

Expected: zero failures.

- [ ] **Step 2: Build release artifacts**

Run:

```powershell
cargo build --release
docker build -t kiro-rs-admin:error-snapshot-safety -f Dockerfile .
```

Expected: both builds exit 0.

- [ ] **Step 3: Review and commit only task files**

Run `git status --short`, `git diff --stat`, `git diff --check`, explicitly stage the modified
source/tests/docs files, review `git diff --cached`, and commit with a Chinese subject:

```text
fix(snapshot): 防止错误快照阻塞服务
```

- [ ] **Step 4: Merge locally into master**

From the main worktree, verify it is clean, merge `fix/error-snapshot-safety` with `--no-ff`, and
run focused tests on the merged result. Do not push.

- [ ] **Step 5: Roll out and verify production**

Tag the built image with a unique local version, update only `/opt/kiro-rs-admin`, retain the prior
image/config reference, and verify authenticated `/api/admin/credentials`, `/admin`, gateway
traffic, CPU, memory, and snapshot storage. Keep production snapshot capture disabled initially.
