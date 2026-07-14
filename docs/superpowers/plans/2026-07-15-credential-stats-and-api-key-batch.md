# 凭据成功统计与 API Key 批量入口 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让新凭据的管理端成功次数从真实的 0 开始，同时保持 balanced 调度公平，并让 API Key 批量导入入口在单条添加弹窗中可直接发现。

**Architecture:** 后端把对外展示的 `success_count` 与内部调度用的 `balance_count` 分离；旧统计文件缺少新字段时由 `success_count` 平滑迁移，新增凭据只继承调度基线而不继承展示统计。前端复用已有批量导入对话框，通过 `initialMode` 直接打开 API Key 文本模式，不新增第二套解析或导入逻辑。

**Tech Stack:** Rust、Serde、Tokio tests、React 19、TypeScript、Bun test、Vite

---

### Task 1: 分离真实成功统计与调度计数

**Files:**
- Modify: `src/kiro/token_manager.rs`
- Test: `src/kiro/token_manager.rs`

- [x] **Step 1: 写入失败回归测试**

在 `token_manager.rs` 的测试模块新增用例：先让旧凭据成功 3 次，再添加新 API Key；断言新凭据快照中的 `success_count == 0`，同时 balanced 下一次可选到新凭据但不会靠伪造展示统计实现。

```rust
#[tokio::test]
async fn add_credential_starts_with_zero_visible_success_count() {
    let mut config = Config::default();
    config.load_balancing_mode = "balanced".to_string();

    let mut existing = KiroCredentials::default();
    existing.id = Some(1);
    existing.kiro_api_key = Some("ksk_existing".to_string());
    existing.auth_method = Some("api_key".to_string());
    existing.api_region = Some("us-east-1".to_string());
    let manager = MultiTokenManager::new(config, vec![existing], None, None, true).unwrap();
    manager.report_success(1);
    manager.report_success(1);
    manager.report_success(1);

    let mut added_credential = KiroCredentials::default();
    added_credential.kiro_api_key = Some("ksk_added".to_string());
    added_credential.auth_method = Some("api_key".to_string());
    added_credential.api_region = Some("us-east-1".to_string());
    let new_id = manager.add_credential(added_credential).await.unwrap();
    let added = manager.snapshot().entries.into_iter().find(|entry| entry.id == new_id).unwrap();
    assert_eq!(added.success_count, 0);

    let entries = manager.entries.lock();
    let added_internal = entries.iter().find(|entry| entry.id == new_id).unwrap();
    assert_eq!(added_internal.balance_count, 3);
}
```

- [x] **Step 2: 运行测试并确认 RED**

Run: `cargo test -q add_credential_starts_with_zero_visible_success_count`

Expected: FAIL，实际值为现有同组账号的成功数基线，而不是 0。

- [x] **Step 3: 添加内部 `balance_count` 并兼容旧统计**

在 `CredentialEntry` 中加入仅供调度使用的字段，并让持久化结构用可选字段兼容旧 JSON：

```rust
struct CredentialEntry {
    success_count: u64,
    balance_count: u64,
}

#[derive(Serialize, Deserialize)]
struct StatsEntry {
    success_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    balance_count: Option<u64>,
    #[serde(default)]
    total_failure_count: u64,
    last_used_at: Option<String>,
}
```

加载旧统计时使用 `s.balance_count.unwrap_or(s.success_count)`；保存时写入 `Some(e.balance_count)`。`report_success` 同时递增两个值，`reset_success_count` 只清理展示统计，避免清零按钮改变调度公平性。

- [x] **Step 4: 将 balanced 调度改用内部计数**

```rust
"balanced" => available
    .iter()
    .min_by_key(|entry| (entry.balance_count, entry.credentials.priority)),
```

`least_conn` 的最终平局字段也改为 `balance_count`。新增凭据使用同组账号最小 `balance_count` 作为内部基线，但 `success_count` 固定为 0。

- [x] **Step 5: 运行后端回归测试**

Run: `cargo test -q add_credential_starts_with_zero_visible_success_count`

Expected: PASS。

Run: `cargo test -q token_manager`

Expected: 相关 Token Manager 测试全部 PASS。

### Task 2: 从单条添加弹窗直达 API Key 批量导入

**Files:**
- Modify: `admin-ui/src/components/add-credential-dialog.tsx`
- Modify: `admin-ui/src/components/batch-import-dialog.tsx`
- Modify: `admin-ui/src/components/dashboard.tsx`
- Test: `admin-ui/src/components/api-key-import-ui.contract.test.ts`

- [x] **Step 1: 扩展 UI 合约测试并确认 RED**

断言单条添加弹窗暴露 `onBatchApiKeyImport`，批量对话框支持 `initialMode`，Dashboard 从 API Key 表单进入时传入 `api-key` 模式，并将原菜单文案明确为“批量导入凭据 / API Key / KAM”。

```ts
expect(addDialog).toContain('onBatchApiKeyImport')
expect(addDialog).toContain('批量添加 API Key')
expect(batchDialog).toContain("initialMode?: ImportMode")
expect(dashboard).toContain('initialMode={batchImportInitialMode}')
expect(dashboard).toContain('批量导入凭据 / API Key / KAM')
```

Run: `bun test src/components/api-key-import-ui.contract.test.ts`

Expected: FAIL，因为这些入口尚不存在。

- [x] **Step 2: 为批量对话框增加初始模式**

```tsx
interface BatchImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  initialMode?: ImportMode
}

export function BatchImportDialog({ open, onOpenChange, initialMode = 'json' }: BatchImportDialogProps) {
  const [importMode, setImportMode] = useState<ImportMode>(initialMode)
  useEffect(() => {
    if (open) setImportMode(initialMode)
  }, [open, initialMode])
}
```

- [x] **Step 3: 添加复用现有导入器的直达按钮**

`AddCredentialDialog` 新增 `onBatchApiKeyImport` 回调，并在 API Key 单条输入区域显示 `type="button"` 的“批量添加 API Key”按钮。Dashboard 关闭单条弹窗后，以 `initialMode="api-key"` 打开原有 `BatchImportDialog`；普通菜单入口仍以 `json` 模式打开。

- [x] **Step 4: 运行管理端测试与构建**

Run: `bun test src/components/api-key-import-ui.contract.test.ts`

Expected: PASS。

Run: `bun run build`

Expected: TypeScript 与 Vite 构建成功。

### Task 3: 完整验证与提交

**Files:**
- Modify: `CHANGELOG.md`

- [x] **Step 1: 记录用户可见行为变化**

在 CHANGELOG 顶部当前版本中记录：新增凭据成功统计从 0 开始、内部 balanced 权重独立持久化、API Key 单条弹窗可直达批量导入。明确不会改变客户端 API 与对话内容。

- [x] **Step 2: 运行最终验证**

Run: `cargo fmt -- --check`

Expected: PASS。

Run: `cargo test -q add_credential_starts_with_zero_visible_success_count`

Expected: PASS。

Run: `bun test`

Expected: PASS。

Run: `bun run build`

Expected: PASS。

- [x] **Step 3: 检查变更边界**

Run: `git status --short && git diff --check && git diff --stat`

Expected: 仅包含本计划、Token Manager、管理端三个组件、UI 合约测试与 CHANGELOG；无空白错误。

- [x] **Step 4: 创建本地提交**

```powershell
git add src/kiro/token_manager.rs admin-ui/src/components/add-credential-dialog.tsx admin-ui/src/components/batch-import-dialog.tsx admin-ui/src/components/dashboard.tsx admin-ui/src/components/api-key-import-ui.contract.test.ts CHANGELOG.md docs/superpowers/plans/2026-07-15-credential-stats-and-api-key-batch.md
git commit -m "fix(admin): 分离成功统计并直达API Key批量导入"
```
