# Key Supplier Webhook Automation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a secure supplier webhook receiver, idempotent automatic/manual Kiro API Key purchasing, credential import, and an Admin UI notification/configuration page.

**Architecture:** A focused `admin::key_supplier` module owns configuration views, the supplier HTTP client, SQLite event state, and orchestration while reusing `MultiTokenManager` for credential validation and persistence. Public webhook ingestion is token-authenticated and fast; all management endpoints remain behind the existing Admin middleware. The React page uses React Query polling and never receives supplier or Kiro keys in plaintext.

**Tech Stack:** Rust 2024, Axum 0.8, Tokio, Reqwest, Rusqlite, Serde, React 19, TypeScript, TanStack Query, Sonner, Bun.

---

### Task 1: Persist and Validate Supplier Configuration

**Files:**
- Modify: `src/model/config.rs`
- Create: `src/admin/key_supplier/config.rs`
- Create: `src/admin/key_supplier/mod.rs`

- [ ] **Step 1: Write failing configuration tests**

Add tests that deserialize an old config with no supplier fields, verify secure defaults, reject a non-HTTP base URL, reject `min_purchase > max_purchase`, and verify that the API response exposes only `api_key_configured` and `webhook_token_configured` booleans.

```rust
#[test]
fn old_config_defaults_supplier_automation_to_disabled() {
    let config: Config = serde_json::from_str("{}").unwrap();
    assert!(!config.key_supplier_auto_purchase);
    assert_eq!(config.key_supplier_min_purchase, 1);
    assert_eq!(config.key_supplier_max_purchase, 1);
}

#[test]
fn supplier_config_rejects_inverted_purchase_range() {
    let error = validate_supplier_config(&SupplierRuntimeConfig {
        min_purchase: 5,
        max_purchase: 2,
        ..Default::default()
    }).unwrap_err();
    assert!(error.to_string().contains("最小购买量"));
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run: `cargo test key_supplier --lib`

Expected: FAIL because supplier config fields/module do not exist.

- [ ] **Step 3: Add config fields and focused config types**

Add camelCase-compatible fields to `Config` with serde defaults. Define `SupplierRuntimeConfig`, `SupplierConfigView`, and `SupplierConfigUpdate`; normalize trailing slashes, allow only HTTP(S), constrain counts to `1..=10_000`, RPM/priority to existing credential limits, API Region through `kiro::region`, and generate a 64-character hex webhook token when absent.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplierRuntimeConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub public_base_url: Option<String>,
    pub webhook_token: Option<String>,
    pub auto_purchase: bool,
    pub min_purchase: u32,
    pub max_purchase: u32,
    pub api_region: String,
    pub rpm_limit: u32,
    pub priority: u32,
    pub groups: Vec<String>,
    pub source_channel: String,
    pub nickname_prefix: String,
}
```

- [ ] **Step 4: Run config tests and full config compatibility tests**

Run: `cargo test key_supplier --lib`

Expected: PASS, including old-config compatibility.

- [ ] **Step 5: Commit**

```bash
git add src/model/config.rs src/admin/key_supplier/config.rs src/admin/key_supplier/mod.rs
git commit -m "feat: add key supplier configuration"
```

### Task 2: Build the SQLite Event Store

**Files:**
- Create: `src/admin/key_supplier/store.rs`
- Modify: `src/admin/key_supplier/mod.rs`

- [ ] **Step 1: Write failing store tests**

Cover schema migration, unique `event_id`, duplicate insertion, event listing, unread count, mark-read, atomic claim, stale-processing recovery, and retry transitions. Use in-memory SQLite.

```rust
#[test]
fn duplicate_event_id_is_not_enqueued_twice() {
    let store = SupplierEventStore::open_in_memory().unwrap();
    assert_eq!(store.insert_event(&fixture_event()).unwrap(), InsertOutcome::Inserted);
    assert_eq!(store.insert_event(&fixture_event()).unwrap(), InsertOutcome::Duplicate);
    assert_eq!(store.claim_next().unwrap().unwrap().event_id, fixture_event().event_id);
    assert!(store.claim_next().unwrap().is_none());
}
```

- [ ] **Step 2: Run store tests and verify RED**

Run: `cargo test key_supplier::store --lib`

Expected: FAIL because `SupplierEventStore` is missing.

- [ ] **Step 3: Implement schema and state transitions**

Create `supplier_events` with a unique event ID and no Key payload column. Use `BEGIN IMMEDIATE` for claiming, WAL for file-backed stores, bounded error/message fields, and typed statuses `received`, `processing`, `succeeded`, `skipped`, `failed`.

```rust
pub enum InsertOutcome { Inserted, Duplicate }

pub trait SupplierEventRepository: Send + Sync {
    fn insert_event(&self, event: &IncomingSupplierEvent) -> anyhow::Result<InsertOutcome>;
    fn claim_next(&self) -> anyhow::Result<Option<StoredSupplierEvent>>;
    fn complete(&self, id: i64, result: &ProcessSummary) -> anyhow::Result<()>;
    fn fail(&self, id: i64, message: &str) -> anyhow::Result<()>;
    fn list(&self, limit: u32, before: Option<i64>) -> anyhow::Result<SupplierEventPage>;
}
```

- [ ] **Step 4: Run store tests**

Run: `cargo test key_supplier::store --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/admin/key_supplier/store.rs src/admin/key_supplier/mod.rs
git commit -m "feat: persist supplier webhook events"
```

### Task 3: Implement the Supplier API Client

**Files:**
- Create: `src/admin/key_supplier/client.rs`
- Modify: `src/admin/key_supplier/mod.rs`

- [ ] **Step 1: Write failing client tests against a local Axum server**

Verify `X-API-Key`, JSON content type, profile/stock/status parsing, purchase order reuse across retry, webhook registration, test dispatch, non-2xx error extraction, 300-character truncation, and rejection of mismatched purchase order IDs or non-`ksk_` keys.

```rust
#[tokio::test]
async fn purchase_sends_stable_idempotency_key_and_validates_keys() {
    let server = SupplierStub::start().await;
    let result = SupplierClient::new(server.url(), "usr-test").unwrap()
        .purchase(2, "0123456789abcdef0123456789abcdef").await.unwrap();
    assert_eq!(result.purchased, 2);
    assert_eq!(server.purchase_order_ids(), vec!["0123456789abcdef0123456789abcdef"]);
}
```

- [ ] **Step 2: Run client tests and verify RED**

Run: `cargo test key_supplier::client --lib`

Expected: FAIL because the client is missing.

- [ ] **Step 3: Implement the bounded HTTP client**

Use a 15-second timeout, no proxy inheritance, JSON decoding, stable order IDs, and at most three attempts for transport/5xx failures. Never include request headers or returned keys in errors or `Debug` output.

```rust
pub async fn purchase(&self, count: u32, order_id: &str) -> Result<PurchaseSummary, SupplierError>;
pub async fn profile(&self) -> Result<SupplierProfile, SupplierError>;
pub async fn stock(&self) -> Result<SupplierStock, SupplierError>;
pub async fn status(&self) -> Result<Value, SupplierError>;
pub async fn register_webhook(&self, url: &str) -> Result<(), SupplierError>;
pub async fn test_webhook(&self) -> Result<(), SupplierError>;
```

- [ ] **Step 4: Run client tests**

Run: `cargo test key_supplier::client --lib`

Expected: PASS with no Key values in output.

- [ ] **Step 5: Commit**

```bash
git add src/admin/key_supplier/client.rs src/admin/key_supplier/mod.rs
git commit -m "feat: add key supplier api client"
```

### Task 4: Orchestrate Webhooks, Purchases, and Credential Imports

**Files:**
- Create: `src/admin/key_supplier/service.rs`
- Modify: `src/admin/key_supplier/mod.rs`
- Modify: `src/kiro/token_manager.rs` only if a narrow import result helper is required

- [ ] **Step 1: Write failing orchestration tests**

Test count selection `min(new_keys, stock, max)`, below-min skip, disabled automation, `purchase_order_id` passthrough, manual 32-hex ID generation, duplicate credential accounting, partial import failure, `all_keys_dead` notification-only behavior, and output redaction.

```rust
#[test]
fn purchase_count_respects_event_stock_and_configured_maximum() {
    assert_eq!(select_purchase_count(20, 8, 5, 2).unwrap(), 5);
    assert!(matches!(select_purchase_count(3, 1, 10, 2), Err(CountDecision::BelowMinimum)));
}
```

- [ ] **Step 2: Run service tests and verify RED**

Run: `cargo test key_supplier::service --lib`

Expected: FAIL because orchestration functions are missing.

- [ ] **Step 3: Implement event parsing and background processing**

Parse only the documented event forms, validate all IDs, immediately insert events, process claimed work asynchronously, and scan every 30 seconds. Build `KiroCredentials` with `auth_method = api_key`, fixed auth region `us-east-1`, configured API Region/RPM/priority/groups/source, and call existing `add_credential` for each returned key.

```rust
pub async fn ingest(&self, token: &str, body: &[u8]) -> Result<IngestResult, SupplierServiceError>;
pub async fn process_pending(&self);
pub async fn manual_purchase(&self, count: u32) -> Result<ProcessSummary, SupplierServiceError>;
pub async fn overview(&self) -> Result<SupplierOverview, SupplierServiceError>;
```

- [ ] **Step 4: Run orchestration and token-manager tests**

Run: `cargo test key_supplier::service --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/admin/key_supplier/service.rs src/admin/key_supplier/mod.rs src/kiro/token_manager.rs
git commit -m "feat: automate supplier key imports"
```

### Task 5: Expose Public and Admin HTTP Routes

**Files:**
- Create: `src/admin/key_supplier/handlers.rs`
- Modify: `src/admin/middleware.rs`
- Modify: `src/admin/router.rs`
- Modify: `src/admin/mod.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Write failing router tests**

Assert valid public webhook works without Admin Key, invalid token is 404/401, oversized or malformed payload is rejected, duplicate event returns success, and every config/overview/purchase/event endpoint requires Admin Key.

```rust
#[tokio::test]
async fn public_webhook_is_token_authenticated_but_not_admin_authenticated() {
    let response = app().oneshot(valid_webhook_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}
```

- [ ] **Step 2: Run router tests and verify RED**

Run: `cargo test key_supplier_router --lib`

Expected: FAIL because routes do not exist.

- [ ] **Step 3: Wire state, public route, and authenticated routes**

Construct the SQLite store from `cache_dir`, inject `Arc<KeySupplierService>` into `AdminState`, mount the public route outside `admin_auth_middleware`, and mount management routes inside it. Add a 64 KiB body limit and return structured JSON errors.

- [ ] **Step 4: Run router and all Rust tests**

Run: `cargo test key_supplier_router --lib`

Expected: PASS.

Run: `cargo test --all-targets`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/admin/key_supplier/handlers.rs src/admin/middleware.rs src/admin/router.rs src/admin/mod.rs src/main.rs
git commit -m "feat: expose key supplier webhook api"
```

### Task 6: Add Frontend API Contracts and Tests

**Files:**
- Create: `admin-ui/src/api/key-supplier.ts`
- Create: `admin-ui/src/lib/key-supplier.test.ts`
- Create: `admin-ui/src/lib/key-supplier.ts`
- Modify: `admin-ui/src/types/api.ts`

- [ ] **Step 1: Write failing frontend tests**

Test config payload omission for blank secret fields, event status labels, unread transition detection, and that no type contains a supplier API key or purchased Key response field.

```ts
test('blank supplier key preserves the stored secret', () => {
  expect(buildSupplierConfigPayload({ apiKey: '   ' })).not.toHaveProperty('apiKey')
})
```

- [ ] **Step 2: Run frontend tests and verify RED**

Run: `bun test src/lib/key-supplier.test.ts`

Expected: FAIL because the helper is missing.

- [ ] **Step 3: Add typed API functions and pure UI helpers**

Implement get/update config, overview, manual purchase, webhook register/test, event list, mark-read, and retry. Keep secrets write-only and use existing Axios Admin authentication.

- [ ] **Step 4: Run frontend tests**

Run: `bun test src/lib/key-supplier.test.ts`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add admin-ui/src/api/key-supplier.ts admin-ui/src/lib/key-supplier.ts admin-ui/src/lib/key-supplier.test.ts admin-ui/src/types/api.ts
git commit -m "feat: add supplier admin api client"
```

### Task 7: Build the Key Supplier Admin Page and Notifications

**Files:**
- Create: `admin-ui/src/components/key-supplier-page.tsx`
- Create: `admin-ui/src/components/key-supplier-ui.contract.test.ts`
- Modify: `admin-ui/src/App.tsx`
- Modify: `README.md`

- [ ] **Step 1: Write failing UI contract tests**

Verify the navigation tab, lazy page import, write-only secret copy, automatic/manual controls, event status rendering, unread badge, and the absence of Key plaintext rendering.

```ts
test('supplier page exposes automation and event controls without key plaintext', () => {
  const source = readFileSync(new URL('./key-supplier-page.tsx', import.meta.url), 'utf8')
  expect(source).toContain('自动购买')
  expect(source).toContain('手动购买')
  expect(source).not.toContain('result.keys')
})
```

- [ ] **Step 2: Run UI contract tests and verify RED**

Run: `bun test src/components/key-supplier-ui.contract.test.ts`

Expected: FAIL because the page is missing.

- [ ] **Step 3: Implement the responsive page and polling notifications**

Use existing components, Lucide icons, React Query, and Sonner. Poll overview every 30 seconds and events every 5 seconds; compare event IDs to show one toast per new unread event. Keep forms single-column on mobile and use compact unframed sections/cards only for distinct config, overview, operation, and repeated event items.

- [ ] **Step 4: Run frontend tests and production build**

Run: `bun test`

Expected: PASS.

Run: `bun run build`

Expected: PASS with the supplier page included in the Vite output.

- [ ] **Step 5: Commit**

```bash
git add admin-ui/src/components/key-supplier-page.tsx admin-ui/src/components/key-supplier-ui.contract.test.ts admin-ui/src/App.tsx README.md
git commit -m "feat: add key supplier admin page"
```

### Task 8: End-to-End Verification and Deployment Handoff

**Files:**
- Modify: `deploy/` only after the final public hostname is known
- Modify: `docs/superpowers/specs/2026-07-24-key-supplier-webhook-design.md` only if verified behavior differs

- [ ] **Step 1: Format and run all local verification**

Run: `cargo fmt --check`

Run: `cargo test --all-targets`

Run from `admin-ui`: `bun test`

Run from `admin-ui`: `bun run build`

Expected: every command exits 0 without warnings introduced by this feature.

- [ ] **Step 2: Run a local webhook smoke test**

Start RS with a disposable config and local supplier stub. POST a documented `new_keys_available` event twice, verify one supplier purchase request, one event row, and one imported credential. Confirm logs and API responses contain no `ksk_` value.

- [ ] **Step 3: Prepare domain instructions without changing DNS**

Document the exact callback URL, required DNS record, Cloudflare proxy choice, Nginx route, and certificate check. Do not register the webhook or enable automatic purchase until the user supplies the domain/DNS record and supplier API key.

- [ ] **Step 4: Review the final diff and commit verification/docs changes**

```bash
git diff --check
git status --short
git add README.md deploy docs/superpowers/specs/2026-07-24-key-supplier-webhook-design.md
git commit -m "docs: add supplier webhook deployment guide"
```

