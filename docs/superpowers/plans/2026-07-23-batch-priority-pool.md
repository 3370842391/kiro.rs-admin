# 批量最高优先池与自定义优先级实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在凭据批量编辑中支持输入任意 `u32` 优先级或一键建立最高优先池，并让“最好模式”真正先按优先级池、再按实时负载调度。

**Architecture:** 扩展现有 `/credentials/batch` 原子补丁，固定数值只修改选中账号，最高优先池则在同一持久化事务中把选中账号设为 `0` 并稳定压缩其他层级。调度器在 `EndpointMode::Best` 下把优先级移到比较链首位，同一池内继续使用 in-flight、首字 EWMA 和历史调度次数。前端复用现有批量编辑对话框和纯函数请求构建器。

**Tech Stack:** Rust 2024、Axum、Serde、parking_lot、React 19、TypeScript、TanStack Query、Bun Test。

---

### Task 1: 扩展批量更新 API 合同与校验

**Files:**
- Modify: `src/admin/types.rs`
- Modify: `src/admin/service.rs`
- Test: `src/admin/types.rs`
- Test: `src/admin/service.rs`

- [ ] **Step 1: 写请求反序列化和互斥校验失败测试**

在 `src/admin/types.rs` 的测试模块增加：

```rust
#[test]
fn batch_update_request_accepts_fixed_or_promoted_priority() {
    let fixed: BatchUpdateCredentialsRequest = serde_json::from_value(serde_json::json!({
        "ids": [1, 2], "priority": 10
    })).unwrap();
    assert_eq!(fixed.priority, Some(10));
    assert!(!fixed.promote_priority);

    let promoted: BatchUpdateCredentialsRequest = serde_json::from_value(serde_json::json!({
        "ids": [1, 2], "promotePriority": true
    })).unwrap();
    assert_eq!(promoted.priority, None);
    assert!(promoted.promote_priority);
}
```

在 `src/admin/service.rs` 测试模块构造同时带 `priority: Some(10)` 和 `promote_priority: true` 的请求，断言 `normalize_batch_update_request` 返回 `InvalidCredential`，并断言仅开启 `promote_priority` 也构成有效补丁。

- [ ] **Step 2: 运行测试并确认因字段不存在而失败**

Run:

```powershell
cargo test admin::types::tests::batch_update_request_accepts_fixed_or_promoted_priority -- --nocapture
cargo test admin::service::tests::normalize_batch_update_request_rejects_conflicting_priority_modes -- --nocapture
```

Expected: FAIL，提示 `priority` / `promote_priority` 字段不存在。

- [ ] **Step 3: 实现请求、响应与归一化字段**

在 `BatchUpdateCredentialsRequest` 增加：

```rust
#[serde(default)]
pub priority: Option<u32>,
#[serde(default)]
pub promote_priority: bool,
```

在 `BatchUpdateCredentialsResponse` 增加：

```rust
pub priority_adjusted: usize,
```

在 `normalize_batch_update_request` 中先拒绝冲突：

```rust
if request.priority.is_some() && request.promote_priority {
    return Err(AdminServiceError::InvalidCredential(
        "priority 与 promotePriority 不能同时设置".to_string(),
    ));
}
```

把两个字段写入 `CredentialBatchPatch`；“至少一个修改字段”的判断同时包含 `priority.is_some()` 和 `promote_priority`。更新现有测试中的请求构造，显式补上 `priority: None` 与 `promote_priority: false`。

- [ ] **Step 4: 运行 API 与 service 测试**

Run:

```powershell
cargo test admin::types::tests -- --nocapture
cargo test admin::service::tests::normalize_batch_update_request -- --nocapture
```

Expected: PASS。

- [ ] **Step 5: 提交 API 合同**

```powershell
git add -- src/admin/types.rs src/admin/service.rs
git commit -m "feat(priority): 扩展批量优先级接口"
```

### Task 2: 原子实现固定优先级与最高优先池

**Files:**
- Modify: `src/kiro/token_manager.rs`
- Modify: `src/admin/service.rs`
- Test: `src/kiro/token_manager.rs`

- [ ] **Step 1: 写固定数值和重编号失败测试**

在 `src/kiro/token_manager.rs` 测试模块新增测试，使用四个优先级分别为 `0, 0, 10, 20` 的凭据：

```rust
fn batch_priority_manager(values: &[(u64, u32)]) -> MultiTokenManager {
    let credentials = values
        .iter()
        .map(|(id, priority)| {
            let mut credential = batch_test_credential(*id, 0, &[], None);
            credential.priority = *priority;
            credential
        })
        .collect();
    MultiTokenManager::new(Config::default(), credentials, None, None, true).unwrap()
}

fn priorities(manager: &MultiTokenManager) -> Vec<(u64, u32)> {
    let mut values = manager
        .clone_all_credentials()
        .into_iter()
        .map(|credential| (credential.id.unwrap(), credential.priority))
        .collect::<Vec<_>>();
    values.sort_by_key(|value| value.0);
    values
}

#[test]
fn batch_priority_supports_fixed_value_and_stable_promotion() {
    let manager = batch_priority_manager(&[(1, 0), (2, 0), (3, 10), (4, 20)]);
    let fixed = manager.batch_update_credentials(
        &[3],
        CredentialBatchPatch {
            priority: Some(7),
            promote_priority: false,
            ..CredentialBatchPatch::default()
        },
    ).unwrap();
    assert_eq!(fixed.priority_adjusted, 1);
    assert_eq!(priorities(&manager), vec![(1, 0), (2, 0), (3, 7), (4, 20)]);

    let promoted = manager.batch_update_credentials(
        &[3, 4],
        CredentialBatchPatch {
            priority: None,
            promote_priority: true,
            ..CredentialBatchPatch::default()
        },
    ).unwrap();
    assert_eq!(priorities(&manager), vec![(1, 1), (2, 1), (3, 0), (4, 0)]);
    assert_eq!(promoted.priority_adjusted, 4);
}
```

再加三项断言：重复 promote 后 `priority_adjusted == 0`；全选后全部为 `0`；持久化路径故意指向目录时，内存中所有账号优先级完整回滚。

- [ ] **Step 2: 运行测试并确认失败原因是补丁字段/算法缺失**

Run:

```powershell
cargo test kiro::token_manager::tests::batch_priority -- --nocapture
```

Expected: FAIL，提示 `CredentialBatchPatch` 或 `CredentialBatchUpdateResult` 缺少优先级字段。

- [ ] **Step 3: 实现补丁字段和稳定层级压缩**

给补丁及结果增加字段并派生默认值：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CredentialBatchPatch {
    pub rpm_limit: Option<u32>,
    pub groups: Option<CredentialGroupPatch>,
    pub source_channel: Option<Option<String>>,
    pub priority: Option<u32>,
    pub promote_priority: bool,
}

pub(crate) struct CredentialBatchUpdateResult {
    pub selected: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub priority_adjusted: usize,
}
```

在 `batch_update_credentials` 中：

1. `promote_priority` 为真时备份全部账号，否则只备份选中账号。
2. 固定数值模式只更新选中账号并计入 `priority_adjusted`。
3. 最高优先池模式收集未选账号的旧优先级到有序去重集合，把层级映射为 `1..N`；选中账号设为 `0`。
4. 选中账号因优先级改变时同时计入 `updated`，未选账号改变只计入 `priority_adjusted`。
5. `updated > 0 || priority_adjusted > 0` 时持久化；失败恢复备份。
6. 成功释放 `entries` 和 `persist_lock` 后调用 `select_highest_priority()`，让普通 priority 模式立即生效。

将 `priority_adjusted` 透传到 `BatchUpdateCredentialsResponse`。

- [ ] **Step 4: 运行批量更新测试**

Run:

```powershell
cargo test kiro::token_manager::tests::batch_update_credentials -- --nocapture
cargo test kiro::token_manager::tests::batch_priority -- --nocapture
cargo test admin::service::tests::batch_update_credentials_returns_updated_summary -- --nocapture
```

Expected: PASS，且持久化失败测试证明所有账号均回滚。

- [ ] **Step 5: 提交原子更新实现**

```powershell
git add -- src/kiro/token_manager.rs src/admin/service.rs
git commit -m "feat(priority): 原子建立最高优先池"
```

### Task 3: 让最好模式按优先池实时调度

**Files:**
- Modify: `src/kiro/token_manager.rs`
- Test: `src/kiro/token_manager.rs`

- [ ] **Step 1: 写调度顺序失败测试**

新增三个测试：

```rust
fn best_priority_manager(values: &[(u64, u32)]) -> MultiTokenManager {
    let mut config = Config::default();
    config.endpoint_mode = EndpointMode::Best;
    let credentials = values
        .iter()
        .map(|(id, priority)| {
            let mut credential = batch_test_credential(*id, 0, &[], None);
            credential.priority = *priority;
            credential
        })
        .collect();
    MultiTokenManager::new(config, credentials, None, None, true).unwrap()
}

#[test]
fn best_mode_prefers_priority_tier_before_realtime_load() {
    let manager = best_priority_manager(&[(1, 0), (2, 10)]);
    manager.entries.lock().iter_mut().find(|entry| entry.id == 1).unwrap().in_flight = 3;
    assert_eq!(manager.select_next_credential(None, None).map(|v| v.0), Some(1));
}

#[test]
fn best_mode_uses_realtime_load_inside_same_priority_tier() {
    let manager = best_priority_manager(&[(1, 0), (2, 0)]);
    manager.entries.lock().iter_mut().find(|entry| entry.id == 1).unwrap().in_flight = 3;
    assert_eq!(manager.select_next_credential(None, None).map(|v| v.0), Some(2));
}

#[test]
fn best_mode_falls_back_when_top_tier_hits_rpm_limit() {
    let manager = best_priority_manager(&[(1, 0), (2, 10)]);
    let mut entries = manager.entries.lock();
    let top = entries.iter_mut().find(|entry| entry.id == 1).unwrap();
    top.credentials.rpm_limit = 1;
    top.recent_requests.push_back(Instant::now());
    drop(entries);
    assert_eq!(manager.select_next_credential(None, None).map(|v| v.0), Some(2));
}
```

- [ ] **Step 2: 运行测试并确认旧比较顺序选错账号**

Run:

```powershell
cargo test kiro::token_manager::tests::best_mode_prefers_priority_tier -- --nocapture
```

Expected: FAIL；旧实现因账号 2 的 `in_flight=0` 而选中账号 2。

- [ ] **Step 3: 调整最好模式比较器**

把 `EndpointMode::Best` 的 least-connection 比较链改为：

```rust
a.credentials.priority
    .cmp(&b.credentials.priority)
    .then_with(|| a.in_flight.cmp(&b.in_flight))
    .then_with(|| match (a.first_byte_ewma_ms, b.first_byte_ewma_ms) {
        (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
        _ => std::cmp::Ordering::Equal,
    })
    .then_with(|| a.balance_count.cmp(&b.balance_count))
    .then_with(|| a.id.cmp(&b.id))
```

保留进入 `available` 列表前已有的 disabled、冷却、RPM、模型和分组过滤；不清理 session affinity。

- [ ] **Step 4: 运行调度器相关测试**

Run:

```powershell
cargo test kiro::token_manager::tests::best_mode -- --nocapture
cargo test kiro::token_manager::tests::session_affinity -- --nocapture
cargo test kiro::token_manager::tests::test_select_next_credential -- --nocapture
```

Expected: PASS。

- [ ] **Step 5: 提交调度语义**

```powershell
git add -- src/kiro/token_manager.rs
git commit -m "feat(priority): 最好模式优先使用高优先级池"
```

### Task 4: 扩展前端请求构建器与类型

**Files:**
- Modify: `admin-ui/src/types/api.ts`
- Modify: `admin-ui/src/lib/rpm-operations.ts`
- Modify: `admin-ui/src/lib/rpm-operations.test.ts`

- [ ] **Step 1: 写优先级解析和请求构建失败测试**

在 `rpm-operations.test.ts` 增加：

```ts
test('优先级接受 0 和 u32 最大值', () => {
  expect(parsePriority('0')).toEqual({ ok: true, value: 0 })
  expect(parsePriority('4294967295')).toEqual({ ok: true, value: 4294967295 })
})

test.each(['', '-1', '1.5', '1e3', '4294967296'])('拒绝无效优先级 %s', (draft) => {
  expect(parsePriority(draft).ok).toBe(false)
})

test('固定数值与最高优先池生成互斥请求', () => {
  const base = {
    ids: [1, 2], editRpm: false, rpmDraft: '', editGroups: false,
    groupMode: 'replace' as const, groups: [], editSource: false, sourceChannel: '',
    editPriority: true,
  }
  expect(buildBatchUpdateRequest({ ...base, priorityMode: 'fixed', priorityDraft: '10' })).toEqual({
    ok: true, value: { ids: [1, 2], priority: 10 },
  })
  expect(buildBatchUpdateRequest({ ...base, priorityMode: 'promote', priorityDraft: '' })).toEqual({
    ok: true, value: { ids: [1, 2], promotePriority: true },
  })
})
```

- [ ] **Step 2: 运行测试并确认函数/字段缺失**

Run:

```powershell
bun test src/lib/rpm-operations.test.ts
```

Expected: FAIL，提示 `parsePriority` 不存在或返回请求缺少字段。

- [ ] **Step 3: 实现 TS 类型和纯函数**

给 `BatchUpdateCredentialsRequest` 增加 `priority?: number`、`promotePriority?: boolean`；给响应增加 `priorityAdjusted: number`。

在 `BatchUpdateInput` 增加可选字段以保持现有调用兼容：

```ts
editPriority?: boolean
priorityMode?: 'fixed' | 'promote'
priorityDraft?: string
```

实现 `parsePriority`，只接受十进制整数 `0..4294967295`。构建器在 `fixed` 模式写 `priority`，在 `promote` 模式只写 `promotePriority: true`，并把优先级开关纳入“至少选择一项”的判断。

- [ ] **Step 4: 运行纯函数测试**

Run:

```powershell
bun test src/lib/rpm-operations.test.ts
```

Expected: PASS。

- [ ] **Step 5: 提交前端合同**

```powershell
git add -- admin-ui/src/types/api.ts admin-ui/src/lib/rpm-operations.ts admin-ui/src/lib/rpm-operations.test.ts
git commit -m "feat(priority): 支持构建批量优先级请求"
```

### Task 5: 在批量编辑对话框加入两种优先级模式

**Files:**
- Modify: `admin-ui/src/components/batch-edit-credential-dialog.tsx`
- Modify: `admin-ui/src/components/admin-rpm-operations-ui.contract.test.ts`

- [ ] **Step 1: 写 UI 接线失败测试**

在合同测试增加断言：

```ts
test('batch dialog exposes fixed and promoted priority modes', async () => {
  const dialog = await readSource('src/components/batch-edit-credential-dialog.tsx')
  expect(dialog).toContain('editPriority')
  expect(dialog).toContain('priorityMode')
  expect(dialog).toContain('batch-priority-value')
  expect(dialog).toContain('指定数值')
  expect(dialog).toContain('最高优先池')
  expect(dialog).toContain('数字越小优先级越高')
  expect(dialog).toContain('可能承担全部新流量')
  expect(dialog).toContain('priorityAdjusted')
})
```

- [ ] **Step 2: 运行合同测试并确认失败**

Run:

```powershell
bun test src/components/admin-rpm-operations-ui.contract.test.ts
```

Expected: FAIL，因为对话框尚无优先级控件。

- [ ] **Step 3: 实现对话框状态、校验与提示**

增加状态：

```ts
const [editPriority, setEditPriority] = useState(false)
const [priorityMode, setPriorityMode] = useState<'fixed' | 'promote'>('fixed')
const [priorityDraft, setPriorityDraft] = useState('0')
const [priorityError, setPriorityError] = useState('')
const priorityInputRef = useRef<HTMLInputElement>(null)
```

对话框打开时重置状态。固定模式提交前调用 `parsePriority`，失败时显示行内错误并聚焦输入框。UI 使用两个带 `aria-pressed` 的按钮切换模式；最高优先池模式显示红/黄风险提示。把字段传入 `buildBatchUpdateRequest`。

成功提示改为：当 `result.priorityAdjusted > 0` 时追加 `，调整 ${result.priorityAdjusted} 个账号优先级`。

- [ ] **Step 4: 运行前端测试和构建**

Run:

```powershell
bun test
bun run build
```

Expected: 现有测试与新增测试全部 PASS，TypeScript/Vite 构建成功。

- [ ] **Step 5: 提交 UI**

```powershell
git add -- admin-ui/src/components/batch-edit-credential-dialog.tsx admin-ui/src/components/admin-rpm-operations-ui.contract.test.ts
git commit -m "feat(admin): 批量设置账号优先级"
```

### Task 6: 完整验证与本地合并

**Files:**
- Verify only; no production file expected.

- [ ] **Step 1: 格式化并运行全部 Rust 测试**

Run:

```powershell
cargo fmt --all
cargo test --all-targets --all-features
```

Expected: Rust 测试全部 PASS。

- [ ] **Step 2: 运行全部前端测试和生产构建**

Run from `admin-ui`:

```powershell
bun test
bun run build
```

Expected: 测试全部 PASS，构建成功。

- [ ] **Step 3: 审查任务差异和工作树**

Run:

```powershell
git diff master --stat
git diff master --check
git status --short
```

Expected: 仅包含规格、计划、后端批量优先级、调度比较器和批量编辑 UI；无密钥、构建产物或无关文件。

- [ ] **Step 4: 合并到本地 master 并复验重点测试**

Run from the main worktree:

```powershell
git merge --no-ff feature/batch-priority-pool -m "feat: 新增批量账号优先级"
cargo test kiro::token_manager::tests::batch_priority -- --nocapture
Set-Location admin-ui
bun test src/lib/rpm-operations.test.ts src/components/admin-rpm-operations-ui.contract.test.ts
```

Expected: 合并成功，重点测试全部 PASS，不执行远程 push。
