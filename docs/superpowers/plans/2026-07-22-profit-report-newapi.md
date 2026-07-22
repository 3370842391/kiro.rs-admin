# RS NewAPI Profit Report Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a server-configured RS profit report that joins NewAPI billing logs to RS traces, uses real upstream Credits, and breaks results down by client Key and group.

**Architecture:** RS emits a correlation ID in `X-Oneapi-Request-Id`; the trace database already stores that ID, Key ID, model, status, and real Credits. A dedicated admin profit module fetches NewAPI `/api/log/` pages, joins by `upstream_request_id`, resolves Key metadata, and returns aggregate rows. A new React tab edits redacted server settings and renders the report.

**Tech Stack:** Rust 2024, Axum, Reqwest, Serde, SQLite trace store, React 19, TypeScript, TanStack Query, Bun tests.

---

### Task 1: Add pure profit domain calculations

**Files:**
- Create: `src/admin/profit.rs`
- Modify: `src/admin/mod.rs`
- Test: `src/admin/profit.rs` (unit tests in the module)

- [ ] **Step 1: Write failing tests for the cost and aggregation rules**

Add tests that construct two joined rows and assert that `45.0 / 2000.0` is used as the default Credit price, fractional Credits are preserved, negative/missing Credits are counted as incomplete rather than replaced with one request, and group totals separate `0.05` and `0.08` rows.

```rust
#[test]
fn report_uses_fractional_credits_and_default_price() {
    let report = aggregate_rows(vec![joined("g05", "key-a", 0.25, 1.0)], ProfitConfig::default());
    assert!((report.cost - 0.005625).abs() < 1e-9);
    assert_eq!(report.missing_cost, 0);
}

#[test]
fn missing_credits_are_not_fallback_billed() {
    let report = aggregate_rows(vec![joined("g08", "key-b", 0.0, 2.0)], ProfitConfig::default());
    assert_eq!(report.missing_cost, 1);
    assert_eq!(report.cost, 0.0);
}

#[test]
fn group_rows_remain_separate() {
    let report = aggregate_rows(vec![
        joined("ratio-005", "key-a", 1.0, 1.0),
        joined("ratio-008", "key-b", 1.0, 1.0),
    ], ProfitConfig::default());
    assert_eq!(report.by_group.len(), 2);
}
```

- [ ] **Step 2: Run the focused test and verify it fails for missing types/functions**

Run: `cargo test admin::profit::tests -- --nocapture`

Expected: FAIL because `ProfitConfig`, `aggregate_rows`, and the report types do not exist yet.

- [ ] **Step 3: Implement the minimal domain types and formulas**

Define `ProfitConfig`, `ProfitRow`, `ProfitReport`, `ProfitGroupStat`, and `aggregate_rows`. Use `credit_price = 0.0225` and `quota_per_unit = 500000.0` defaults. Compute `revenue = quota as f64 / quota_per_unit`, `cost = credits * credit_price` only for finite positive Credits, and count missing Credits separately.

- [ ] **Step 4: Run the focused test and verify it passes**

Run: `cargo test admin::profit::tests -- --nocapture`

Expected: PASS with all three tests green.

- [ ] **Step 5: Commit the domain module**

```powershell
git add src/admin/profit.rs src/admin/mod.rs
git commit -m "feat: add accurate profit aggregation domain"
```

### Task 2: Persist redacted NewAPI profit settings

**Files:**
- Modify: `src/model/config.rs`
- Modify: `src/admin/service.rs`
- Modify: `src/admin/types.rs`
- Modify: `src/admin/handlers.rs`
- Modify: `src/admin/router.rs`
- Test: `src/admin/router.rs` and `src/admin/service.rs`

- [ ] **Step 1: Write failing config round-trip and redaction tests**

Test that old JSON without profit fields loads defaults, that `PUT /config/profit` persists `profitNewapiBase`, `profitNewapiUser`, `profitCreditPrice`, and `profitQuotaPerUnit`, and that an empty token in an update retains the existing token. Assert GET returns `tokenConfigured` but never the token value.

- [ ] **Step 2: Run the focused tests and verify the new endpoints fail**

Run: `cargo test admin::router::tests -- --nocapture`

Expected: FAIL because the routes and response types are absent.

- [ ] **Step 3: Add camelCase config fields with safe defaults**

Add optional/defaulted fields to `Config`:

```rust
#[serde(default)]
pub profit_newapi_base: Option<String>,
#[serde(default)]
pub profit_newapi_token: Option<String>,
#[serde(default)]
pub profit_newapi_user: Option<String>,
#[serde(default = "default_profit_credit_price")]
pub profit_credit_price: f64,
#[serde(default = "default_profit_quota_per_unit")]
pub profit_quota_per_unit: f64,
```

Implement `AdminService::get_profit_config` and `set_profit_config` using the existing read-latest-config/write-back pattern. Reject non-HTTP(S) NewAPI URLs, non-finite/non-positive prices, and quota units below 1. Preserve the stored token when the update payload omits it.

- [ ] **Step 4: Add authenticated routes and redacted response types**

Add `GET /config/profit` and `PUT /config/profit`. The GET response contains `newapiBase`, `newapiUser`, `creditPrice`, `quotaPerUnit`, and `tokenConfigured`; it has no token field. The PUT response returns the same redacted shape.

- [ ] **Step 5: Run tests and commit**

Run: `cargo test admin::router::tests admin::service::tests -- --nocapture`

Expected: PASS. Commit with `git add src/model/config.rs src/admin/service.rs src/admin/types.rs src/admin/handlers.rs src/admin/router.rs && git commit -m "feat: persist redacted NewAPI profit config"`.

### Task 3: Emit a stable correlation ID for NewAPI joins

**Files:**
- Modify: `src/anthropic/middleware.rs`
- Modify: `src/anthropic/router.rs`
- Modify: `src/anthropic/handlers.rs`
- Test: `src/anthropic/middleware.rs`

- [ ] **Step 1: Write a failing middleware test**

Add an Axum `oneshot` test that calls an authenticated `/v1/messages` test route and asserts the response contains a valid `X-Oneapi-Request-Id`; assert the same ID is injected into the request extensions/header used by `RequestTracer`.

- [ ] **Step 2: Run the test and verify it fails**

Run: `cargo test anthropic::middleware::tests::oneapi_request_id -- --nocapture`

Expected: FAIL because no response correlation header is currently emitted.

- [ ] **Step 3: Implement request/response correlation middleware**

Add a small middleware that generates a UUID, replaces any client-supplied value, injects it into the internal request headers, runs the handler, and sets `X-Oneapi-Request-Id` on the response before returning it. Mount it on both `/v1` and `/cc/v1` authenticated routes. Update `RequestTracer::new` to reuse the internal header when present, falling back to a UUID for direct unit calls.

- [ ] **Step 4: Run the test and commit**

Run: `cargo test anthropic::middleware::tests::oneapi_request_id -- --nocapture`

Expected: PASS. Commit with `git add src/anthropic/middleware.rs src/anthropic/router.rs src/anthropic/handlers.rs && git commit -m "feat: expose trace id for NewAPI billing joins"`.

### Task 4: Fetch NewAPI logs and join them to RS traces

**Files:**
- Modify: `src/admin/profit.rs`
- Modify: `src/admin/handlers.rs`
- Modify: `src/admin/router.rs`
- Modify: `src/admin/trace_db.rs`
- Test: `src/admin/profit.rs`

- [ ] **Step 1: Write failing NewAPI parser, pagination, and join tests**

Use a local Axum test server that returns two `/api/log/` pages. Assert the client sends `Authorization` and `New-Api-User`, stops at the short page, parses `quota`, `token_name`, `model_name`, and `upstream_request_id`, and joins only exact trace IDs. Assert rows without a trace are returned in `unmatched` instead of being billed.

- [ ] **Step 2: Run the focused tests and verify they fail**

Run: `cargo test admin::profit::tests::newapi -- --nocapture`

Expected: FAIL because the HTTP client and trace query helper are absent.

- [ ] **Step 3: Implement bounded NewAPI pagination and exact trace lookup**

Create a `reqwest::Client` with a 30-second timeout and no proxy override. Request `/api/log/?p=N&page_size=100&type=2&start_timestamp=...&end_timestamp=...`; stop on a short page or reported total. Treat non-200, malformed JSON, and `success=false` as a complete report error.

Add a trace-store method that fetches records by a set of trace IDs within the requested time window. Resolve Key metadata from `ClientKeyManager::list`; map `key_id=0` to `system` and missing IDs to `unknown-key`.

- [ ] **Step 4: Implement the admin report endpoint**

Add `POST /profit/report` accepting `{minutes}` with bounds `1..=10080`. Load the persisted NewAPI settings, reject missing credentials with HTTP 400, fetch logs, join exact IDs, call `aggregate_rows`, and return the full report. Do not mutate usage logs or customer request handling.

- [ ] **Step 5: Run focused tests and commit**

Run: `cargo test admin::profit::tests -- --nocapture`

Expected: PASS, including pagination, exact join, missing cost, and group aggregation. Commit with `git add src/admin/profit.rs src/admin/handlers.rs src/admin/router.rs src/admin/trace_db.rs && git commit -m "feat: join NewAPI billing with RS credits"`.

### Task 5: Add the admin API client and profit page

**Files:**
- Create: `admin-ui/src/api/profit.ts`
- Create: `admin-ui/src/components/profit-page.tsx`
- Modify: `admin-ui/src/types/api.ts`
- Modify: `admin-ui/src/App.tsx`
- Test: `admin-ui/src/components/profit-page.test.tsx`

- [ ] **Step 1: Write failing UI contract tests**

Assert the page renders NewAPI address/user/Token status, default Credit price `0.0225`, time range buttons, KPI labels, group rows, negative-profit warning styling, and the save behavior that sends no token field when the input is blank.

- [ ] **Step 2: Run the focused UI test and verify it fails**

Run: `bun test admin-ui/src/components/profit-page.test.tsx`

Expected: FAIL because the API module, page, and tab are absent.

- [ ] **Step 3: Implement API helpers and typed responses**

Add typed `getProfitConfig`, `updateProfitConfig`, and `runProfitReport` functions using the existing authenticated Axios helper. Define `ProfitConfigView`, `ProfitReport`, and `ProfitGroupStat` in `types/api.ts` with `tokenConfigured` and no plaintext token in the response type.

- [ ] **Step 4: Implement the page and register a Profit tab**

Build the form and KPI/table UI with existing Card, Input, Button, Badge, and Table components. Use TanStack Query for config loading and invalidate the report after saving. Render `profit < 0` with the existing destructive/warning variant. Add the tab to `App.tsx` without changing the default Overview tab.

- [ ] **Step 5: Run UI tests and build**

Run: `bun test admin-ui/src/components/profit-page.test.tsx && bun run build`

Expected: PASS and a successful Vite/TypeScript build. Commit with `git add admin-ui/src/api/profit.ts admin-ui/src/components/profit-page.tsx admin-ui/src/components/profit-page.test.tsx admin-ui/src/types/api.ts admin-ui/src/App.tsx && git commit -m "feat: add profit report admin page"`.

### Task 6: Full verification and compatibility checks

**Files:**
- Modify: `docs/superpowers/specs/2026-07-22-profit-report-newapi-design.md` only if verification reveals a contract mismatch.

- [ ] **Step 1: Format and run all Rust tests**

Run: `cargo fmt --all -- --check` and `cargo test --all-targets --all-features`.

Expected: both commands pass without changing customer API behavior.

- [ ] **Step 2: Run lint and frontend checks**

Run: `cargo clippy --all-targets --all-features -- -D warnings` and `bun test && bun run build` from `admin-ui`.

Expected: no warnings promoted to errors and successful frontend build.

- [ ] **Step 3: Exercise the report with a local mock NewAPI**

Start a local mock returning one matched `upstream_request_id` and one unmatched row. Confirm the report shows real fractional Credits, cost `credits × 0.0225`, separate groups, and `unmatched=1`; confirm a blank NewAPI token returns HTTP 400 rather than making a request.

- [ ] **Step 4: Review the final diff and commit the verification notes**

Run `git diff master --stat`, `git diff --check`, and `git status --short`. If all checks pass, commit any final documentation adjustment with `git commit -m "test: verify NewAPI profit report"`.

