# 管理端批量 RPM 与运行容量可视化实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为管理端增加一次性批量修改 RPM/分组/来源的接口和界面，并显示最近 60 秒集群 RPM、有限容量、剩余容量、满载账号与进行中请求。

**Architecture:** TokenManager 在单次写锁内完成全部 ID 校验和内存补丁，解锁后只持久化一次；AdminService 负责 HTTP 输入规范化和从同一份快照计算只读 RPM 汇总。前端通过单一 batch API 提交，纯函数负责输入校验和展示状态，Dashboard 复用现有 30 秒凭据查询，不进入模型请求或流式响应链路。

**Tech Stack:** Rust 2024、Axum 0.8、Serde、parking_lot、React 19、TypeScript 6、TanStack Query、Bun、Tailwind CSS、Lucide。

---

## 文件结构

- 修改 `src/kiro/token_manager.rs`：运行态快照、领域批量补丁、单锁全量校验、单次持久化及领域测试。
- 修改 `src/admin/types.rs`：batch API DTO、`RpmSummary`、`inFlight` 字段及 Serde 测试。
- 修改 `src/admin/service.rs`：输入边界校验、补丁映射、汇总纯函数和批量服务方法。
- 修改 `src/admin/handlers.rs`：`PUT /credentials/batch` handler。
- 修改 `src/admin/router.rs`：注册静态 batch 路由。
- 修改 `admin-ui/src/types/api.ts`：RPM 汇总和 batch API 类型。
- 修改 `admin-ui/src/api/credentials.ts`：单请求 batch API 客户端。
- 修改 `admin-ui/src/hooks/use-credentials.ts`：batch mutation 与查询刷新。
- 创建 `admin-ui/src/lib/rpm-operations.ts`：RPM 输入、请求组装和展示状态纯函数。
- 创建 `admin-ui/src/lib/rpm-operations.test.ts`：前端业务规则单测。
- 修改 `admin-ui/src/components/batch-edit-credential-dialog.tsx`：RPM 开关、单请求提交、失败保留选择。
- 创建 `admin-ui/src/components/rpm-status-bar.tsx`：紧凑且响应式的全局状态条。
- 修改 `admin-ui/src/components/credential-card.tsx`：80%/100% RPM 状态与 `inFlight` 徽章。
- 修改 `admin-ui/src/components/dashboard.tsx`：状态条和批量编辑入口接线。
- 创建 `admin-ui/src/components/admin-rpm-operations-ui.contract.test.ts`：关键 UI/API 接线契约测试。

### Task 1: 定义批量协议与运行快照

**Files:**
- Modify: `src/admin/types.rs:12`
- Modify: `src/kiro/token_manager.rs:943`

- [ ] **Step 1: 先写 DTO 和快照失败测试**

在 `src/admin/types.rs` 的 tests 中增加 `batch_update_request_deserializes_camel_case`，断言：

```rust
let request: BatchUpdateCredentialsRequest = serde_json::from_value(serde_json::json!({
    "ids": [1, 2],
    "rpmLimit": 0,
    "groups": { "mode": "add", "values": [" ztest ", "ztest"] },
    "sourceChannel": "test-pool"
})).unwrap();
assert_eq!(request.ids, vec![1, 2]);
assert_eq!(request.rpm_limit, Some(0));
assert_eq!(request.groups.unwrap().mode, BatchGroupMode::Add);
```

在 `src/kiro/token_manager.rs` 增加 `snapshot_exposes_in_flight`，获取凭据、建立 guard 后断言快照为 1，drop 后断言为 0。

- [ ] **Step 2: 运行测试并确认 RED**

Run:

```powershell
$env:CARGO_BUILD_JOBS='2'
cargo test --bin kiro-rs --no-default-features batch_update_request_deserializes_camel_case
cargo test --bin kiro-rs --no-default-features snapshot_exposes_in_flight
```

Expected: 分别因 DTO 和 `CredentialEntrySnapshot.in_flight` 尚不存在而编译失败。

- [ ] **Step 3: 添加最小协议和快照字段**

在 `src/admin/types.rs` 添加并统一复用：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchGroupMode { Replace, Add, Remove }

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchGroupsPatchRequest {
    pub mode: BatchGroupMode,
    pub values: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchUpdateCredentialsRequest {
    pub ids: Vec<u64>,
    pub rpm_limit: Option<u32>,
    pub groups: Option<BatchGroupsPatchRequest>,
    pub source_channel: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpmSummary {
    pub window_seconds: u64,
    pub current: u64,
    pub limited_capacity: u64,
    pub remaining_limited_capacity: u64,
    pub unlimited_accounts: u64,
    pub saturated_accounts: u64,
    pub enabled_accounts: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchUpdateCredentialsResponse {
    pub selected: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub rpm_summary: RpmSummary,
}
```

为 `CredentialsStatusResponse` 添加 `rpm_summary`，为 `CredentialStatusItem` 和 `CredentialEntrySnapshot` 添加 `in_flight: u32`，并在 `MultiTokenManager::snapshot()` 填充现有 `CredentialEntry.in_flight`。将固定窗口导出为 `pub(crate) const RPM_WINDOW_SECONDS: u64 = 60`，现有窗口由该常量构造。

- [ ] **Step 4: 运行测试并确认 GREEN**

Run: `cargo test --bin kiro-rs --no-default-features batch_update_request_deserializes_camel_case snapshot_exposes_in_flight`

Expected: 过滤器分别执行时通过；若 Cargo 拒绝两个过滤串，则拆成两条命令。

- [ ] **Step 5: 提交协议和快照**

```powershell
git add -- src/admin/types.rs src/kiro/token_manager.rs
git commit -m "feat(admin): 定义批量RPM协议与运行快照"
```

### Task 2: TokenManager 原子批量补丁

**Files:**
- Modify: `src/kiro/token_manager.rs:3392`
- Test: `src/kiro/token_manager.rs:4241`

- [ ] **Step 1: 写全量校验和补丁语义失败测试**

新增以下测试，每个测试只验证一个行为：

```rust
#[test]
fn batch_update_credentials_updates_all_targets_and_persists_once() { /* RPM/来源/分组全部生效并从文件读回 */ }

#[test]
fn batch_update_credentials_missing_id_is_fail_closed() { /* [1, 999] 返回错误且 1 未变化 */ }

#[test]
fn batch_update_credentials_rejects_duplicate_ids() { /* [1, 1] 返回领域错误且零修改 */ }

#[test]
fn batch_update_credentials_supports_replace_add_remove() { /* 保序去重；remove 只删目标值 */ }

#[test]
fn batch_update_credentials_keeps_recent_request_window_when_lowering_limit() { /* rpmCurrent 不清零 */ }
```

- [ ] **Step 2: 运行测试并确认 RED**

Run: `cargo test -j 2 --bin kiro-rs --no-default-features batch_update_credentials`

Expected: `MultiTokenManager::batch_update_credentials` 和领域补丁类型尚不存在。

- [ ] **Step 3: 实现领域补丁和单次持久化**

在 Admin 无关的 TokenManager 层定义：

```rust
pub(crate) enum CredentialGroupPatch {
    Replace(Vec<String>),
    Add(Vec<String>),
    Remove(Vec<String>),
}

pub(crate) struct CredentialBatchPatch {
    pub rpm_limit: Option<u32>,
    pub groups: Option<CredentialGroupPatch>,
    pub source_channel: Option<Option<String>>,
}

pub(crate) struct CredentialBatchUpdateResult {
    pub selected: usize,
    pub updated: usize,
    pub unchanged: usize,
}
```

实现 `batch_update_credentials(&self, ids: &[u64], patch: CredentialBatchPatch)`：在一次 `entries.lock()` 中用 `HashSet` 拒绝重复 ID、确认全部 ID 存在后再修改；add/remove 必须基于锁内实时 groups 计算；仅当配置真实变化时增加 `updated`。释放 entries 锁后只调用一次 `persist_credentials()`。校验错误不得修改任何账号；写盘失败沿用现有 mutation 语义返回错误，不承诺回滚已修改内存。

- [ ] **Step 4: 运行批量与现有 RPM 回归**

Run:

```powershell
cargo test -j 2 --bin kiro-rs --no-default-features batch_update_credentials
cargo test -j 2 --bin kiro-rs --no-default-features test_rpm_window_count_and_expiry
cargo test -j 2 --bin kiro-rs --no-default-features test_select_skips_rpm_exceeded_credential
```

Expected: 全部 PASS。

- [ ] **Step 5: 提交 TokenManager 批量能力**

```powershell
git add -- src/kiro/token_manager.rs
git commit -m "feat(credentials): 原子批量更新账号配置"
```

### Task 3: Admin 汇总、校验与路由

**Files:**
- Modify: `src/admin/service.rs:1005`
- Modify: `src/admin/handlers.rs:439`
- Modify: `src/admin/router.rs:60`
- Test: `src/admin/service.rs:4284`

- [ ] **Step 1: 写汇总口径和请求边界失败测试**

为纯函数 `calculate_rpm_summary` 写测试，构造有限、无限、禁用、满载和窗口未过期账号，断言：

```rust
assert_eq!(summary.window_seconds, RPM_WINDOW_SECONDS);
assert_eq!(summary.current, 17); // 包含禁用账号尚未过期的已发生请求
assert_eq!(summary.limited_capacity, 30); // 只含启用且有限速账号
assert_eq!(summary.remaining_limited_capacity, 8);
assert_eq!(summary.unlimited_accounts, 1);
assert_eq!(summary.saturated_accounts, 1);
assert_eq!(summary.enabled_accounts, 3);
```

为 `normalize_batch_update_request` 写空 ID、10001 个 ID、重复 ID、RPM 100001、空补丁、add/remove 空分组失败测试，并验证 replace 空数组和 `sourceChannel: ""` 是有效清空操作。

- [ ] **Step 2: 运行测试并确认 RED**

Run:

```powershell
cargo test -j 2 --bin kiro-rs --no-default-features rpm_summary
cargo test -j 2 --bin kiro-rs --no-default-features normalize_batch_update_request
```

Expected: 汇总和规范化函数尚不存在。

- [ ] **Step 3: 实现服务、handler 和路由**

`normalize_batch_update_request` 执行：ID 数量 `1..=10000`、无重复、RPM `<=100000`、groups trim/去空/稳定去重、add/remove 归一化后不得为空、replace 空数组允许清空、来源 trim 后空值映射为 `Some(None)`、至少有一个真实补丁。分组沿用现有单账号“字符串标签”语义，不新增 GroupManager 注册限制。

`calculate_rpm_summary` 从同一份 `CredentialEntrySnapshot` 列表计算，`current` 包含所有账号窗口请求；容量、剩余、不限速、满载和启用数仅统计 `!disabled`。`get_all_credentials()` 同时返回 `inFlight` 和 `rpmSummary`。

新增：

```rust
pub fn batch_update_credentials(
    &self,
    request: BatchUpdateCredentialsRequest,
) -> Result<BatchUpdateCredentialsResponse, AdminServiceError>
```

handler 复用现有 `AdminServiceError.status_code()` 与脱敏 JSON 响应；router 注册 `.route("/credentials/batch", put(batch_update_credentials))`，最终路径为 `PUT /api/admin/credentials/batch`。

- [ ] **Step 4: 运行 Admin 和主程序检查**

Run:

```powershell
cargo test -j 2 --bin kiro-rs --no-default-features rpm_summary
cargo test -j 2 --bin kiro-rs --no-default-features normalize_batch_update_request
cargo check -j 2 --bin kiro-rs --locked --no-default-features
```

Expected: PASS；除仓库基线已有 warning 外不新增 warning。

- [ ] **Step 5: 提交 Admin API**

```powershell
git add -- src/admin/service.rs src/admin/handlers.rs src/admin/router.rs
git commit -m "feat(admin): 增加批量账号接口与RPM汇总"
```

### Task 4: 前端请求组装、输入校验与 API

**Files:**
- Create: `admin-ui/src/lib/rpm-operations.ts`
- Create: `admin-ui/src/lib/rpm-operations.test.ts`
- Modify: `admin-ui/src/types/api.ts:2`
- Modify: `admin-ui/src/api/credentials.ts:365`
- Modify: `admin-ui/src/hooks/use-credentials.ts:177`

- [ ] **Step 1: 写纯函数失败测试**

测试期望 API：

```ts
expect(parseRpmLimit('0')).toEqual({ ok: true, value: 0 })
expect(parseRpmLimit('100000')).toEqual({ ok: true, value: 100000 })
expect(parseRpmLimit('')).toEqual({ ok: false, message: '请输入 RPM 上限' })
expect(parseRpmLimit('-1').ok).toBe(false)
expect(parseRpmLimit('1.5').ok).toBe(false)
expect(parseRpmLimit('100001').ok).toBe(false)
expect(rpmLoadState(7, 10)).toBe('normal')
expect(rpmLoadState(8, 10)).toBe('warning')
expect(rpmLoadState(10, 10)).toBe('saturated')
expect(totalInFlight([{ inFlight: 2 }, { inFlight: 3 }])).toBe(5)
```

再测试 `buildBatchUpdateRequest`：RPM 开关关闭时不输出 `rpmLimit`，`0` 保持为 0，groups 输出 `{mode, values}`，空来源保持为显式清除。

- [ ] **Step 2: 运行测试并确认 RED**

Run: `bun test src/lib/rpm-operations.test.ts`

Expected: 模块尚不存在。

- [ ] **Step 3: 实现纯函数和类型/API/hook**

`parseRpmLimit` 只接受 `0..=100000` 的整数字符串；`rpmLoadState` 对不限速返回 `unlimited`，有限账号按 `<80%`、`80%..<100%`、`>=100%` 分类。`buildBatchUpdateRequest` 返回 `BatchUpdateCredentialsRequest` 或具体校验错误，不通过 `Number('')` 隐式清零。

`batchUpdateCredentials(request)` 调用：

```ts
api.put<BatchUpdateCredentialsResponse>('/credentials/batch', request, { timeout: 30_000 })
```

`useBatchUpdateCredentials` 成功后 invalidate `['credentials']`，错误路径不清理选择。

- [ ] **Step 4: 运行单测和类型构建**

Run:

```powershell
bun test src/lib/rpm-operations.test.ts
bun run build
```

Expected: PASS。

- [ ] **Step 5: 提交前端业务层**

```powershell
git add -- admin-ui/src/lib/rpm-operations.ts admin-ui/src/lib/rpm-operations.test.ts admin-ui/src/types/api.ts admin-ui/src/api/credentials.ts admin-ui/src/hooks/use-credentials.ts
git commit -m "feat(admin-ui): 接入批量RPM更新接口"
```

### Task 5: 批量弹窗与容量状态 UI

**Files:**
- Modify: `admin-ui/src/components/batch-edit-credential-dialog.tsx:38`
- Create: `admin-ui/src/components/rpm-status-bar.tsx`
- Modify: `admin-ui/src/components/credential-card.tsx:424`
- Modify: `admin-ui/src/components/dashboard.tsx:1563`
- Create: `admin-ui/src/components/admin-rpm-operations-ui.contract.test.ts`

- [ ] **Step 1: 写 UI 接线契约失败测试**

沿用现有源码契约模式，读取目标文件并断言：

```ts
expect(batchDialog).toContain('useBatchUpdateCredentials')
expect(batchDialog).toContain('editRpm')
expect(batchDialog).not.toContain('for (let i = 0; i < credentials.length; i++)')
expect(batchDialog).not.toContain('onDone()\n    } catch')
expect(dashboard).toContain('<RpmStatusBar')
expect(statusBar).toContain('remainingLimitedCapacity')
expect(card).toContain('inFlight')
```

- [ ] **Step 2: 运行契约测试并确认 RED**

Run: `bun test src/components/admin-rpm-operations-ui.contract.test.ts`

Expected: 新组件和接线尚不存在。

- [ ] **Step 3: 改造批量编辑弹窗**

增加“修改 RPM” Switch 和数值输入，输入属性固定为 `type="number" inputMode="numeric" min={0} max={100000} step={1}`；0 显示“不限速”，正数显示“每账号最近 60 秒最多 N 次”。提交时只调用一次 mutation；成功后关闭弹窗并 `onDone()`，失败只 toast 且保持弹窗、选择与输入。移除逐账号 progress 和 `computeGroups`。DialogContent 使用 `max-h-[calc(100dvh-2rem)] overflow-y-auto p-4 sm:p-6`。

- [ ] **Step 4: 添加状态条和账号负载提示**

`RpmStatusBar` 使用无嵌套卡片的紧凑布局，移动端 `grid-cols-2`、宽屏 5 列；显示最近 60 秒 RPM、有限容量、剩余、满载账号和进行中请求。存在不限速账号时明确显示“容量不限 / 不限速账号 N”，同时保留有限容量明细。

Dashboard 在工具栏与列表间插入状态条，使用全部账号计算 `totalInFlight`，不使用当前页或筛选结果；批量按钮改名为“批量编辑”，title 写明“RPM / 分组 / 来源”。

CredentialCard 的 RPM tooltip 改为“最近 60 秒滚动窗口”；80% 使用警告色，100% 使用错误色和“已满载”；`inFlight > 0` 时增加“进行中 N”徽章，不给列表增加固定宽列。

- [ ] **Step 5: 运行 UI 测试和生产构建**

Run:

```powershell
bun test src/components/admin-rpm-operations-ui.contract.test.ts
bun test
bun run build
```

Expected: 全部 PASS，320px 约束下无显式固定最小宽度引入的横向溢出。

- [ ] **Step 6: 提交 UI**

```powershell
git add -- admin-ui/src/components/batch-edit-credential-dialog.tsx admin-ui/src/components/rpm-status-bar.tsx admin-ui/src/components/credential-card.tsx admin-ui/src/components/dashboard.tsx admin-ui/src/components/admin-rpm-operations-ui.contract.test.ts
git commit -m "feat(admin-ui): 增加RPM容量与批量编辑界面"
```

### Task 6: 完整验证、影响说明与本地集成

**Files:**
- Modify: `docs/2026-07-14-completed-work-summary.md`
- Modify: `docs/2026-07-14-cctest-ztest-customer-impact.md`

- [ ] **Step 1: 更新客户影响文档**

明确记录：本功能不修改 Anthropic/OpenAI 请求、system、工具参数、缓存计费、Token 拆分、SSE 或首 Token；只读指标不影响客户对话。管理员降低 RPM 后，账号在最近 60 秒窗口自然回落前会暂停接收新请求，可能让请求切换到其他账号；所有启用账号均满载时沿用现有无可用凭据行为。批量写盘失败返回错误，但与现有单账号更新一致，不承诺内存回滚。

- [ ] **Step 2: 运行完整验证**

Run:

```powershell
$env:CARGO_BUILD_JOBS='2'
cargo test --bin kiro-rs --locked --no-default-features
cargo check --all-targets --locked --no-default-features
Set-Location admin-ui
bun test
bun run build
Set-Location ..
git diff --check
git status --short
```

Expected: Rust、Bun 和生产构建全部通过；只允许两条已记录的既有 Rust warning，不允许新增 warning 或未解释文件。

- [ ] **Step 3: 请求独立代码审查并修复发现**

重点审查：全量校验是否先于任何修改、只持久化一次、summary 口径、API 失败是否保留选择、`0` 是否保持不限速、移动端是否引入溢出、是否误触模型主链路。

- [ ] **Step 4: 创建最终本地提交**

```powershell
git add -- docs/2026-07-14-completed-work-summary.md docs/2026-07-14-cctest-ztest-customer-impact.md
git commit -m "docs(admin): 记录批量RPM功能与客户影响"
```

- [ ] **Step 5: 合并回本地 master**

在主工作区确认干净并运行：

```powershell
git merge --no-ff feature/admin-rpm-operations -m "merge: 合并批量RPM与容量可视化"
```

不执行 `git push`；推送 GitHub、部署公网 8991 或生产 8990 必须由用户另行明确授权。

## 自检结果

- 规格覆盖：batch API、全量校验、单次持久化、RPM 汇总、`inFlight`、跨页选择、失败保留选择、移动端状态条和客户影响均有对应任务。
- 类型一致：前后端统一使用 `rpmSummary`、`inFlight`、`rpmLimit`、`groups.mode/values`、`sourceChannel`；响应统一包含 `enabledAccounts`。
- 明确边界：沿用当前分组字符串语义；不改协议主链路；不承诺写盘失败回滚；不新增额外轮询；不部署生产。
- 占位符扫描：计划中没有 TBD、TODO 或未定义的“稍后实现”步骤。
