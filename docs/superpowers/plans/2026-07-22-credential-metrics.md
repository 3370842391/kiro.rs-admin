# 账号实时指标展示 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在凭据列表中显示每账号最近 1 分钟 RPM 及至少六项相关运行指标，并将状态刷新间隔缩短到 10 秒。

**Architecture:** 复用现有 `/credentials` 响应，不增加后端字段；在 `credential-metrics.ts` 集中处理 RPM、成功率、Token 到期、余额新鲜度和代理状态的安全格式化，`CredentialCard` 只负责布局。新增指标带放在账号身份徽章下方，使用响应式网格避免桌面和窄屏溢出。

**Tech Stack:** React 19、TypeScript、TanStack Query、Tailwind CSS、Lucide、Bun Test。

---

### Task 1: 指标派生与格式化纯函数

**Files:**
- Create: `admin-ui/src/lib/credential-metrics.ts`
- Create: `admin-ui/src/lib/credential-metrics.test.ts`

- [ ] **Step 1: Write failing tests**

覆盖 `formatRpmMetric`、`formatRpmUtilization`、`formatSuccessRate`、`formatTokenState`、`formatBalanceFreshness` 和 `connectionLabel`：有限速、0/0、不限速、无请求、过期 Token、未来 Token、余额未查询、代理/直连均应有稳定文本。

- [ ] **Step 2: Run focused tests and verify RED**

Run: `bun test src/lib/credential-metrics.test.ts`

Expected: 因模块和函数尚不存在而失败。

- [ ] **Step 3: Implement minimal pure functions**

定义以下返回值，所有函数只接收原始字段和可选 `nowMs`，避免读取全局时间导致测试不稳定：

```ts
export function formatRpmMetric(current: number, limit: number): string
export function formatRpmUtilization(current: number, limit: number): string
export function formatSuccessRate(success: number, failures: number): string
export function formatTokenState(expiresAt: string | null, nowMs?: number): string
export function formatBalanceFreshness(updatedAt: number | undefined, nowMs?: number): string
export function connectionLabel(hasProxy: boolean): string
```

输入为负数、非有限数或非法日期时返回“未知/暂无数据”，不能产生 `NaN`、`Infinity` 或误导性的 0%。

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `bun test src/lib/credential-metrics.test.ts`

Expected: 所有指标格式化测试通过。

- [ ] **Step 5: Commit**

```powershell
git add admin-ui/src/lib/credential-metrics.ts admin-ui/src/lib/credential-metrics.test.ts
git commit -m "feat(admin): 增加账号指标格式化"
```

### Task 2: 接入账号卡片的 7 项指标

**Files:**
- Modify: `admin-ui/src/components/credential-card.tsx`
- Create: `admin-ui/src/components/credential-metrics-ui.contract.test.ts`

- [ ] **Step 1: Write failing UI contract test**

读取 `credential-card.tsx` 源码，断言存在“近1分钟 RPM”“RPM 使用率”“成功率”“进行中”“Token”“余额更新”“代理/直连”标签、`formatRpmMetric` 和 `formatSuccessRate` 接线，以及 list/card 两种视图都调用统一指标组件。

- [ ] **Step 2: Run test and verify RED**

Run: `bun test src/components/credential-metrics-ui.contract.test.ts`

Expected: 当前卡片没有这些新标签或统一指标组件，测试失败。

- [ ] **Step 3: Implement compact metric strip**

在 `credential-card.tsx` 引入纯函数，并增加 `CredentialMetric`/`CredentialMetricsStrip` 内部组件。指标带使用 `inline-flex`、`tabular-nums`、`title` 和 `aria-label`，在身份徽章下使用：

```tsx
<div className="mt-1 grid min-w-0 grid-cols-2 gap-1 text-[11px] sm:grid-cols-4 xl:flex xl:flex-wrap">
  <CredentialMetric label="近1分钟 RPM" value={formatRpmMetric(...)} />
  <CredentialMetric label="RPM 使用率" value={formatRpmUtilization(...)} />
  <CredentialMetric label="成功率" value={formatSuccessRate(...)} />
  <CredentialMetric label="进行中" value={String(credential.inFlight)} />
  <CredentialMetric label="Token" value={formatTokenState(...)} />
  <CredentialMetric label="余额更新" value={formatBalanceFreshness(...)} />
  <CredentialMetric label="连接" value={connectionLabel(...)} />
</div>
```

`CredentialMetricsStrip` 同时放入 list 和 card 的身份区域，禁止重复计算；已有 RPM 列保留，作为大屏详细负载显示，新的指标带用于快速扫描。

- [ ] **Step 4: Run UI contract and typecheck**

Run: `bun test src/components/credential-metrics-ui.contract.test.ts` and `bun run build`

Expected: 合同测试通过，TypeScript 和 Vite 构建成功。

- [ ] **Step 5: Commit**

```powershell
git add admin-ui/src/components/credential-card.tsx admin-ui/src/components/credential-metrics-ui.contract.test.ts
git commit -m "feat(admin): 在账号卡片显示实时指标"
```

### Task 3: 缩短状态刷新与全量验证

**Files:**
- Modify: `admin-ui/src/hooks/use-credentials.ts`

- [ ] **Step 1: Change refresh interval**

把 `useCredentials` 的 `refetchInterval` 从 `30000` 改为 `10000`，保留现有 query key、缓存和错误处理。

- [ ] **Step 2: Add refresh contract test**

在 `credential-metrics-ui.contract.test.ts` 中断言 `use-credentials.ts` 使用 `refetchInterval: 10000`，防止后续改回 30 秒导致 RPM 显示滞后。

- [ ] **Step 3: Run complete frontend verification**

Run: `bun test` and `bun run build` from `admin-ui`。

Expected: 全部前端测试通过，生产构建成功。

- [ ] **Step 4: Run Rust regression suite**

Run: `cargo test -j 1` and `cargo fmt --all -- --check` from repository root。

Expected: Rust 测试通过；格式检查无错误。该功能不改 Rust，但验证管理端接口契约没有回归。

- [ ] **Step 5: Review task-only diff and commit**

```powershell
git diff --check
git status --short
git add admin-ui/src/hooks/use-credentials.ts admin-ui/src/components/credential-metrics-ui.contract.test.ts
git commit -m "feat(admin): 提升账号状态刷新频率"
```

确认没有构建产物、凭据或主目录无关改动进入提交。

### Task 4: 本地合并回 master

- [ ] **Step 1: 从主工作树确认未提交文件**

主工作树现有未提交改动不得暂存或覆盖。

- [ ] **Step 2: 快进合并功能分支**

Run from main worktree: `git merge --ff-only feature/credential-metrics`

- [ ] **Step 3: 在合并结果运行前端关键测试**

Run: `bun test src/lib/credential-metrics.test.ts src/components/credential-metrics-ui.contract.test.ts` from `admin-ui`。

Expected: 新增纯函数和 UI 合同测试通过。

- [ ] **Step 4: Report**

报告新增指标、刷新频率、测试结果、提交哈希，以及尚未推送 GitHub/服务器的状态。
