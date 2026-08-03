# Supplier Settings, Health, and Region Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让多供应商共享导入预设、保留单家采购差异，并修复 Nickname、库存水位和采购区域的线上错误。

**Architecture:** 在现有 `SupplierKind` 枚举分发基础上新增集中式能力声明和标准化区域类型，不引入运行时插件。凭据持久化禁用原因，采购服务统一生成健康快照；公共导入预设与单家可选覆盖解析成一个 `ResolvedSupplierImportPreset`，现有扁平字段继续物化以保证旧版本读取。

**Tech Stack:** Rust 2024、Serde、Axum、Rusqlite、React 19、TypeScript、TanStack Query、Bun/Vite。

---

### Task 1: Persist disable reasons and fix stock classification

**Files:**
- Modify: `src/kiro/model/credentials.rs`
- Modify: `src/kiro/token_manager.rs`
- Test: `src/kiro/model/credentials.rs`
- Test: `src/kiro/token_manager.rs`

- [ ] **Step 1: Add failing serialization and health tests**

Add tests proving `disableReason` round-trips, legacy disabled credentials fall back to `Manual`, and `TooManyFailures` is excluded from `target_credited` while `Manual` remains credited.

```rust
assert_eq!(health.ready, 0);
assert_eq!(health.target_credited, 0);
assert_eq!(health.system_disabled, 1);
```

- [ ] **Step 2: Run focused tests and confirm RED**

Run:

```powershell
cargo test kiro::token_manager::tests::supplier_health_excludes_system_disabled_from_target
```

Expected: compile failure for the new persisted type/fields.

- [ ] **Step 3: Move the reason enum into the credential model**

Implement a serializable `CredentialDisableReason` enum with the existing variants and add:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub disable_reason: Option<CredentialDisableReason>,
```

Keep `disabled`, `died_at`, and `quota_exhausted_at` for backward compatibility.

- [ ] **Step 4: Use one authoritative reason in Token Manager**

Every automatic/manual disable and re-enable path must update `credentials.disable_reason`; runtime snapshots read that value. Legacy load precedence is explicit reason, `died_at`, `quota_exhausted_at`, then `Manual`.

- [ ] **Step 5: Expand health output and classification**

Replace the old five-field health result with fields including `ready`, `target_credited`, `manual_reserved`, `cooling`, and `system_disabled`, while preserving `usable` as a serialized compatibility alias equal to `target_credited` for old consumers.

- [ ] **Step 6: Run focused tests and confirm GREEN**

Run:

```powershell
cargo test kiro::token_manager::tests::supplier_credential_health
cargo test kiro::token_manager::tests::pool_health
cargo test kiro::token_manager::tests::disable_reason
```

Expected: all matching tests pass.

### Task 2: Add normalized supplier region and capability models

**Files:**
- Create: `src/admin/key_supplier/capabilities.rs`
- Modify: `src/admin/key_supplier/mod.rs`
- Modify: `src/model/config.rs`
- Modify: `src/admin/key_supplier/config.rs`
- Test: `src/model/config.rs`
- Test: `src/admin/key_supplier/config.rs`

- [ ] **Step 1: Add failing capability/config tests**

Test the documented matrix: CEO supports fixed/webhook/best-available, IO supports fixed/batch, Drop/CC/legacy omit purchase region. Test that invalid fixed regions fail before any purchase.

- [ ] **Step 2: Run focused tests and confirm RED**

```powershell
cargo test admin::key_supplier::capabilities
cargo test admin::key_supplier::config::tests::purchase_region
```

- [ ] **Step 3: Implement normalized types**

Create serializable enums:

```rust
pub enum SupplierRegion { Us, Eu }
pub enum PurchaseRegionMode { Omit, Fixed, Webhook, BestAvailable, Batch }
pub enum RegionSource { PurchaseResponse, Webhook, Request, ConfigFallback }
```

Provide strict conversions to supplier wire values (`us/eu`) and Kiro API values (`us-east-1/eu-central-1`).

- [ ] **Step 4: Implement capability declaration**

Add `SupplierCapabilities::for_kind(kind)` as the single source for region modes, idempotency, webhook registration, and price support. Keep protocol request/response parsing in the client.

- [ ] **Step 5: Add per-supplier region settings**

Extend `KeySupplierConfig` with `purchase_region_mode`, optional `purchase_region`, and `credential_api_region_fallback`. Deserialize old `apiRegion` into the fallback and continue serializing `apiRegion` with the resolved fallback for rollback.

- [ ] **Step 6: Validate by capability and run tests**

Reject modes unsupported by the selected kind; require a fallback whenever the protocol cannot produce authoritative region evidence.

For legacy CEO entries that do not contain a supported region mode, migrate the runtime default to fixed US so an upgrade does not continue selecting European inventory:

```rust
assert_eq!(runtime.settings.purchase_region_mode, PurchaseRegionMode::Fixed);
assert_eq!(runtime.settings.purchase_region, Some(SupplierRegion::Us));
```
Explicitly persisted supported modes remain unchanged.

### Task 3: Carry actual region through quote, purchase, and webhook

**Files:**
- Modify: `src/admin/key_supplier/client.rs`
- Modify: `src/admin/key_supplier/store.rs`
- Modify: `src/admin/key_supplier/service.rs`
- Test: `src/admin/key_supplier/client.rs`
- Test: `src/admin/key_supplier/service.rs`

- [ ] **Step 1: Add failing protocol tests**

Cover:

```rust
// CEO fixed US always sends zone=us and returns actual_region=Us.
// CEO webhook EU sends zone=eu and imports eu-central-1.
// IO fixed EU sends region=eu only without order_id.
// IO batch sends order_id and omits region.
// Drop and CC never send zone/region.
```

- [ ] **Step 2: Run the new tests and confirm RED**

```powershell
cargo test admin::key_supplier::client::tests::kiro_ceo_fixed_us
cargo test admin::key_supplier::service::tests::kiro_ceo_eu_imports_eu_region
```

- [ ] **Step 3: Persist webhook region evidence**

Add nullable `event_region` to `IncomingSupplierEvent` and `supplier_events`, with an additive SQLite migration. Parse CEO `zone` and any documented future `region` field without inferring from human messages.

- [ ] **Step 4: Standardize purchase input/output**

Replace loose `zone: Option<&str>` with a typed purchase context and extend `Purchase`:

```rust
pub actual_region: Option<SupplierRegion>,
pub region_source: Option<RegionSource>,
```

- [ ] **Step 5: Implement per-protocol request rules**

CEO sends the resolved zone; IO sends region only for non-batch purchases; Drop/CC/legacy omit it. Response region wins over webhook/request/fallback.

- [ ] **Step 6: Run client/service protocol tests**

Expected: all existing supplier tests and new region tests pass without contacting real suppliers.

### Task 4: Add common import preset and mandatory supplier Nickname

**Files:**
- Modify: `src/model/config.rs`
- Modify: `src/admin/key_supplier/config.rs`
- Modify: `src/admin/key_supplier/handlers.rs`
- Modify: `src/admin/key_supplier/service.rs`
- Modify: `src/admin/router.rs`
- Test: corresponding Rust test modules

- [ ] **Step 1: Add failing config and Nickname tests**

Test common defaults, per-supplier overrides, old flat config loading, rollback materialization, and names:

```text
ceo-1df694d5-1
ceo-生产-1df694d5-1
```

- [ ] **Step 2: Run focused tests and confirm RED**

```powershell
cargo test model::config::tests::key_supplier_common
cargo test admin::key_supplier::service::tests::purchased_credentials_always_include_supplier
```

- [ ] **Step 3: Implement common import config**

Add `KeySupplierCommonConfig` and `SupplierImportOverrides` for source channel, Nickname label, RPM, priority, groups, and forbidden cleanup. Resolve them in one function returning `ResolvedSupplierImportPreset`.

- [ ] **Step 4: Preserve old flat fields**

On load, lift identical values into common settings and preserve differences as overrides. On save, write resolved legacy fields so v0.9.45 can still read each supplier.

- [ ] **Step 5: Make supplier identity mandatory in Nickname**

The source segment always starts with trimmed supplier name or id; optional label is inserted after it and can never replace it. Keep the order prefix and index suffix under 128 Unicode characters.

- [ ] **Step 6: Import using resolved preset and resolved region**

`credential_from_supplier_key` receives the resolved preset and final credential API region; it must not read the old raw runtime `api_region`.

- [ ] **Step 7: Run config/service tests**

Expected: old and new config fixtures, mandatory supplier names, and region-specific imports pass.

### Task 5: Persist one structured decision snapshot

**Files:**
- Modify: `src/admin/key_supplier/store.rs`
- Modify: `src/admin/key_supplier/service.rs`
- Modify: `src/admin/key_supplier/handlers.rs`
- Test: `src/admin/key_supplier/store.rs`
- Test: `src/admin/key_supplier/service.rs`

- [ ] **Step 1: Add failing round-trip tests**

Test successful, skipped, and failed events all carry one `SupplierDecisionSnapshot` with scope, target, health categories, deficit, requested amount, region evidence, decision, and reason.

- [ ] **Step 2: Add `decision_snapshot_json` migration**

Use Serde JSON and a versioned typed struct. Keep current scalar pool columns unchanged for old clients.

- [ ] **Step 3: Build the snapshot once per decision**

The service creates one snapshot, uses it for database persistence and tracing fields, and carries it through every `ProcessAction` branch.

- [ ] **Step 4: Run event store/service tests**

Expected: old rows deserialize with `None`; all new decision paths round-trip the safe snapshot.

### Task 6: Update the management UI

**Files:**
- Modify: `admin-ui/src/types/api.ts`
- Modify: `admin-ui/src/lib/key-supplier.ts`
- Modify: `admin-ui/src/lib/key-supplier.test.ts`
- Modify: `admin-ui/src/components/key-supplier-page.tsx`
- Modify: `admin-ui/src/components/key-supplier-ui.contract.test.ts`

- [ ] **Step 1: Add failing TypeScript and UI contract tests**

Require a common import settings section, capability-driven region controls, renamed fallback label, mandatory supplier Nickname preview, and expanded decision details.

- [ ] **Step 2: Run tests and confirm RED**

```powershell
cd admin-ui
bun test src/lib/key-supplier.test.ts src/components/key-supplier-ui.contract.test.ts
```

- [ ] **Step 3: Update API types and pure helpers**

Model common settings, import overrides, supplier capabilities, regions, expanded health, and decision snapshots. Add pure helpers for merging drafts and displaying region modes.

- [ ] **Step 4: Implement common and supplier sections**

Place common import settings together. Keep connection/stock/price/region/webhook settings per supplier. Use selects for region; omit the control for no-region protocols.

- [ ] **Step 5: Render auditable health and decisions**

Show `目标 / 计入目标 / 可调度 / 系统禁用` and event decision fields without exposing secret or purchased key material.

- [ ] **Step 6: Run UI tests and build**

```powershell
bun test
bun run build
```

Expected: all tests pass and production bundle builds.

### Task 7: Full verification, review, commit, and push

**Files:** all task files only.

- [ ] **Step 1: Format and run focused checks**

```powershell
cargo fmt --check
cargo test admin::key_supplier
cargo test kiro::token_manager::tests::supplier_credential_health
```

- [ ] **Step 2: Run full verification**

```powershell
cargo test
cargo clippy --all-targets --all-features
cd admin-ui
bun test
bun run build
```

Expected: no task-related failures. Compare the known baseline failure `http_client::tests::upstream_uses_one_connection_per_request` if it remains.

- [ ] **Step 3: Review diff and secret safety**

Inspect `git diff`, search for API keys/server addresses, run `git diff --check`, and review migration/backward-compat behavior.

- [ ] **Step 4: Commit only task files**

```powershell
git add -- <explicit task paths>
git diff --cached --check
git commit -m "feat(key-supplier): 统一供应商设置并修复库存区域判定"
```

- [ ] **Step 5: Push the feature branch**

```powershell
git push -u deploy feature/supplier-settings-health-region
```

Expected: remote branch is created successfully; no deployment or real purchase is triggered.
