# Key Supplier Account Presets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让自动采购账号按可配置 RPM 和已有分组入库，并可选择在上游返回 403 时自动删除。

**Architecture:** 在供应商配置中增加 `autoDeleteForbidden`，导入时把它固化为凭证级 `deleteOnForbidden` 标记。Provider 的普通和流式认证失败路径复用一个 403 处理函数，由 TokenManager 只删除带标记凭证；前端复用现有分组查询与 `GroupMultiSelect`。

**Tech Stack:** Rust、Serde、Tokio、React、TypeScript、TanStack Query、Bun Test。

---

### Task 1: 配置与凭证标记

**Files:**
- Modify: `src/model/config.rs`
- Modify: `src/kiro/model/credentials.rs`
- Modify: `src/admin/key_supplier/config.rs`
- Modify: `src/admin/key_supplier/service.rs`

- [ ] **Step 1: Write the failing tests**

在配置默认值、camelCase 往返和供应商凭证构造测试中断言：

```rust
assert!(!config.key_supplier.auto_delete_forbidden);
assert!(encoded["keySupplier"]["autoDeleteForbidden"].as_bool().unwrap());
assert!(credential.delete_on_forbidden);
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test key_supplier_defaults_keep_legacy_config_compatible key_supplier_config_round_trips_in_camel_case auto_purchase_uses_webhook_order_and_stock_bounds`

Expected: FAIL，字段尚不存在。

- [ ] **Step 3: Write minimal implementation**

为 `KeySupplierConfig`、`SupplierRuntimeConfig`、`SupplierConfigView` 和 `SupplierConfigUpdate` 增加默认 `false` 的 `auto_delete_forbidden`；为 `KiroCredentials` 增加默认 `false` 的 `delete_on_forbidden`。构造供应商凭证时写入：

```rust
delete_on_forbidden: runtime.auto_delete_forbidden,
```

- [ ] **Step 4: Run focused tests**

Run: `cargo test key_supplier`

Expected: PASS。

### Task 2: 403 自动删除

**Files:**
- Modify: `src/kiro/token_manager.rs`
- Modify: `src/kiro/provider.rs`

- [ ] **Step 1: Write the failing TokenManager test**

构造一个 `delete_on_forbidden=true` 和一个普通凭证，断言：

```rust
assert_eq!(manager.delete_credential_on_forbidden(flagged_id).unwrap(), Some(true));
assert!(manager.get_credential(flagged_id).is_none());
assert_eq!(manager.delete_credential_on_forbidden(normal_id).unwrap(), None);
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test delete_credential_on_forbidden`

Expected: FAIL，方法尚不存在。

- [ ] **Step 3: Implement guarded deletion and shared provider handling**

新增：

```rust
pub fn delete_credential_on_forbidden(&self, id: u64) -> anyhow::Result<Option<bool>>
```

未标记返回 `Ok(None)`；带标记时调用现有删除与持久化逻辑并返回是否仍有可用凭证。Provider 新增共享 helper：403 优先调用该方法，删除失败或未标记则回退 `report_failure`；401 始终走原逻辑。普通和流式请求都调用该 helper。

- [ ] **Step 4: Run focused tests**

Run: `cargo test delete_credential_on_forbidden`

Expected: PASS。

### Task 3: 管理端预设控件

**Files:**
- Modify: `admin-ui/src/types/api.ts`
- Modify: `admin-ui/src/lib/key-supplier.ts`
- Modify: `admin-ui/src/lib/key-supplier.test.ts`
- Modify: `admin-ui/src/components/key-supplier-page.tsx`
- Modify: `admin-ui/src/components/key-supplier-ui.contract.test.ts`

- [ ] **Step 1: Write failing frontend tests**

配置 payload 应包含：

```ts
autoDeleteForbidden: true,
```

UI contract 应包含 `GroupMultiSelect`、`useGroupOptions`、“自动采购 RPM 预设”和“403 时自动删除”。

- [ ] **Step 2: Run tests to verify they fail**

Run: `bun test src/lib/key-supplier.test.ts src/components/key-supplier-ui.contract.test.ts`

Expected: FAIL，新字段和新控件不存在。

- [ ] **Step 3: Implement controls**

扩展 TypeScript 配置类型和 payload；供应页面删除逗号分组文本状态，改为：

```tsx
<GroupMultiSelect
  value={config.groups}
  options={groupOptions}
  onChange={(groups) => updateField('groups', groups)}
/>
```

RPM 标签改为“自动采购 RPM 预设”，增加 `autoDeleteForbidden` Switch。

- [ ] **Step 4: Run frontend tests and build**

Run: `bun test && bun run build`

Expected: 全部 PASS，构建成功。

### Task 4: 完整验证与提交

**Files:**
- Verify all modified files

- [ ] **Step 1: Run Rust verification**

Run: `cargo test`

Expected: 全部 PASS。

- [ ] **Step 2: Inspect changes**

Run: `git status --short && git diff --stat && git diff --check`

Expected: 只有本功能文件及原有 `.cargo-target/`，无空白错误。

- [ ] **Step 3: Commit verified implementation**

显式暂存本功能文件并提交：

```bash
git commit -m "feat(supplier): 增加自动采购账号预设与403清理"
```

- [ ] **Step 4: Deploy and smoke test**

构建正式镜像并仅替换 `kiro-rs-admin`；确认正式后台与 Webhook HTTP 200、测试容器镜像不变、近期日志无 panic/fatal。
