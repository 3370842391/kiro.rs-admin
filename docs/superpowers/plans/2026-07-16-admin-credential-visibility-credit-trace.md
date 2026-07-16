# 管理端账号完整展示、可用积分与 429 链路 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让管理端完整显示账号徽章、汇总启用账号的可用积分，并按请求展示可核查的 429 端点/账号重试链。

**Architecture:** 保持 Rust API 和生产重试行为不变，在前端增加两个无副作用派生模块：一个负责余额汇总与金额展示，另一个负责 trace 排序和恢复分类。`dashboard`、`RpmStatusBar`、`CredentialCard` 与失败弹窗只消费这些派生结果，所有边界语义由 Bun 单元测试固定，再用源码合约测试保证 UI 接线没有回退。

**Tech Stack:** React 19、TypeScript 6、Vite 8、Bun Test、Tailwind CSS、现有 `TraceRecord`/`BalanceResponse` API 类型

---

## 文件结构

- Create: `admin-ui/src/lib/credential-summary.ts` — 汇总启用账号余额并生成稳定的展示值。
- Create: `admin-ui/src/lib/credential-summary.test.ts` — 固定禁用、未知、零/负余额和覆盖值语义。
- Create: `admin-ui/src/lib/failure-trace.ts` — 排序尝试、分类恢复方式并生成紧凑链路标签。
- Create: `admin-ui/src/lib/failure-trace.test.ts` — 固定同账号换端点、同端点重试、换账号、失败和中断语义。
- Create: `admin-ui/src/components/admin-credential-observability-ui.contract.test.ts` — 验证三个 UI 改动与纯函数正确接线。
- Modify: `admin-ui/src/components/dashboard.tsx` — 从完整凭据集合与 `balanceMap` 派生积分汇总。
- Modify: `admin-ui/src/components/rpm-status-bar.tsx` — 展示积分主值、覆盖率及六项响应式状态。
- Modify: `admin-ui/src/components/credential-card.tsx` — 徽章从裁剪改成自动换行。
- Modify: `admin-ui/src/components/credential-failures-dialog.tsx` — 每个请求渲染一张卡和完整 attempts。
- Modify: `admin-ui/src/components/admin-rpm-operations-ui.contract.test.ts` — 更新状态栏网格断言，避免旧的五列约束与新六项冲突。

## 执行约束

- 工作目录固定为 `D:/kiro2api/kiro-rs2/kiro.rs-admin/.worktrees/admin-credential-visibility-credit-trace`。
- 不修改 `src/` 下 Rust 文件、数据库 schema、配置默认值或 API 响应结构。
- 每个任务严格遵守红—绿—重构：先看到目标测试失败，再写最小实现。
- 每次提交只暂存任务列出的文件；提交信息使用中文。
- `docs/` 被 `.gitignore` 忽略，实施期间无需重复提交计划文档。

---

### Task 1: 可用积分派生与格式化纯函数

**Files:**
- Create: `admin-ui/src/lib/credential-summary.test.ts`
- Create: `admin-ui/src/lib/credential-summary.ts`

- [ ] **Step 1: 写余额汇总失败测试**

创建 `admin-ui/src/lib/credential-summary.test.ts`：

```ts
import { describe, expect, test } from 'bun:test'
import {
  formatAvailableCreditSummary,
  summarizeAvailableCredits,
} from './credential-summary'

const balance = (remaining: number) => ({ remaining })

describe('summarizeAvailableCredits', () => {
  test('只汇总启用账号的有限正余额并保留有效覆盖数', () => {
    const throttled = {
      id: 7,
      disabled: false,
      throttledRemainingSecs: 60,
      balance: balance(2.5),
    }

    expect(
      summarizeAvailableCredits(
        [
          { id: 1, disabled: false, balance: balance(10) },
          { id: 2, disabled: true, balance: balance(999) },
          { id: 3, disabled: false, balance: balance(0) },
          { id: 4, disabled: false, balance: balance(-5) },
          { id: 5, disabled: false },
          { id: 6, disabled: false, balance: balance(Number.POSITIVE_INFINITY) },
          throttled,
        ],
        new Map(),
      ),
    ).toEqual({
      availableCredits: 12.5,
      enabledCount: 6,
      observedCount: 4,
    })
  })

  test('当前页面手动刷新值优先于后端余额缓存', () => {
    expect(
      summarizeAvailableCredits(
        [{ id: 1, disabled: false, balance: balance(10) }],
        new Map([[1, balance(30)]]),
      ),
    ).toEqual({
      availableCredits: 30,
      enabledCount: 1,
      observedCount: 1,
    })
  })
})

describe('formatAvailableCreditSummary', () => {
  test('格式化已知金额和覆盖率', () => {
    expect(
      formatAvailableCreditSummary({
        availableCredits: 1234.5,
        enabledCount: 15,
        observedCount: 12,
      }),
    ).toEqual({
      value: '$1,234.50',
      detail: '已统计 12/15 个启用账号',
    })
  })

  test('区分余额未知、已知为零和没有启用账号', () => {
    expect(
      formatAvailableCreditSummary({
        availableCredits: 0,
        enabledCount: 3,
        observedCount: 0,
      }),
    ).toEqual({ value: '待查询', detail: '已统计 0/3 个启用账号' })

    expect(
      formatAvailableCreditSummary({
        availableCredits: 0,
        enabledCount: 2,
        observedCount: 2,
      }),
    ).toEqual({ value: '$0.00', detail: '已统计 2/2 个启用账号' })

    expect(
      formatAvailableCreditSummary({
        availableCredits: 0,
        enabledCount: 0,
        observedCount: 0,
      }),
    ).toEqual({ value: '$0.00', detail: '无启用账号' })
  })
})
```

- [ ] **Step 2: 运行测试并确认因模块缺失而失败**

Run:

```powershell
cd admin-ui
bun test src/lib/credential-summary.test.ts
```

Expected: FAIL，错误包含 `Cannot find module './credential-summary'`。

- [ ] **Step 3: 写最小余额汇总实现**

创建 `admin-ui/src/lib/credential-summary.ts`：

```ts
export interface CreditBalance {
  remaining: number
}

export interface CreditCredential {
  id: number
  disabled: boolean
  balance?: CreditBalance
}

export interface AvailableCreditSummary {
  availableCredits: number
  enabledCount: number
  observedCount: number
}

export interface AvailableCreditDisplay {
  value: string
  detail: string
}

const usdFormatter = new Intl.NumberFormat('en-US', {
  style: 'currency',
  currency: 'USD',
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
})

export function summarizeAvailableCredits(
  credentials: ReadonlyArray<CreditCredential>,
  balanceOverrides: ReadonlyMap<number, CreditBalance>,
): AvailableCreditSummary {
  let availableCredits = 0
  let enabledCount = 0
  let observedCount = 0

  for (const credential of credentials) {
    if (credential.disabled) continue
    enabledCount += 1

    const remaining = (balanceOverrides.get(credential.id) ?? credential.balance)?.remaining
    if (remaining == null || !Number.isFinite(remaining)) continue

    observedCount += 1
    if (remaining > 0) availableCredits += remaining
  }

  return { availableCredits, enabledCount, observedCount }
}

export function formatAvailableCreditSummary(
  summary: AvailableCreditSummary,
): AvailableCreditDisplay {
  if (summary.enabledCount === 0) {
    return { value: '$0.00', detail: '无启用账号' }
  }

  const detail = `已统计 ${summary.observedCount}/${summary.enabledCount} 个启用账号`
  if (summary.observedCount === 0) {
    return { value: '待查询', detail }
  }

  return { value: usdFormatter.format(summary.availableCredits), detail }
}
```

- [ ] **Step 4: 运行聚焦测试并确认通过**

Run: `bun test src/lib/credential-summary.test.ts`

Expected: 4 tests PASS，0 FAIL。

- [ ] **Step 5: 提交纯函数**

```powershell
git add -- admin-ui/src/lib/credential-summary.ts admin-ui/src/lib/credential-summary.test.ts
git diff --cached --check
git commit -m "feat(admin): 增加启用账号积分汇总"
```

---

### Task 2: 将可用积分接入顶部状态栏

**Files:**
- Create: `admin-ui/src/components/admin-credential-observability-ui.contract.test.ts`
- Modify: `admin-ui/src/components/dashboard.tsx:102,271,1834-1837`
- Modify: `admin-ui/src/components/rpm-status-bar.tsx:1-70`
- Modify: `admin-ui/src/components/admin-rpm-operations-ui.contract.test.ts:95-107`

- [ ] **Step 1: 写积分 UI 接线失败测试**

创建 `admin-ui/src/components/admin-credential-observability-ui.contract.test.ts`：

```ts
import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(path, 'utf8').catch(() => '')
}

describe('admin credential observability UI wiring', () => {
  test('dashboard derives credit summary from all credentials and fresh balance overrides', async () => {
    const dashboard = await readSource('src/components/dashboard.tsx')

    expect(dashboard).toContain('summarizeAvailableCredits')
    expect(dashboard).toMatch(
      /summarizeAvailableCredits\(\s*data\?\.credentials\s*\?\?\s*\[\],\s*balanceMap\s*\)/s,
    )
    expect(dashboard).toContain('availableCreditSummary={availableCreditSummary}')
  })

  test('RPM status bar renders available credits and coverage', async () => {
    const status = await readSource('src/components/rpm-status-bar.tsx')

    expect(status).toContain('AvailableCreditSummary')
    expect(status).toContain('formatAvailableCreditSummary')
    expect(status).toContain('label="可用积分"')
    expect(status).toContain('value={creditDisplay.value}')
    expect(status).toContain('detail={creditDisplay.detail}')
    expect(status).toContain('sm:grid-cols-3')
    expect(status).toContain('xl:grid-cols-6')
  })
})
```

- [ ] **Step 2: 运行合约测试并确认接线尚不存在**

Run: `bun test src/components/admin-credential-observability-ui.contract.test.ts`

Expected: 2 tests FAIL；输出缺少 `summarizeAvailableCredits` 和 `AvailableCreditSummary`。

- [ ] **Step 3: 修改状态栏接口和第六项展示**

在 `admin-ui/src/components/rpm-status-bar.tsx` 顶部加入并使用积分类型：

```ts
import type { RpmSummary } from '@/types/api'
import {
  formatAvailableCreditSummary,
  type AvailableCreditSummary,
} from '@/lib/credential-summary'

interface RpmStatusBarProps {
  summary?: RpmSummary
  totalInFlight: number
  availableCreditSummary: AvailableCreditSummary
}
```

将函数签名和派生值替换为：

```ts
export function RpmStatusBar({
  summary,
  totalInFlight,
  availableCreditSummary,
}: RpmStatusBarProps) {
  const current = summary?.current ?? 0
  const limitedCapacity = summary?.limitedCapacity ?? 0
  const remainingLimitedCapacity = summary?.remainingLimitedCapacity ?? 0
  const unlimitedAccounts = summary?.unlimitedAccounts ?? 0
  const saturatedAccounts = summary?.saturatedAccounts ?? 0
  const hasUnlimitedCapacity = unlimitedAccounts > 0
  const creditDisplay = formatAvailableCreditSummary(availableCreditSummary)
```

将状态栏网格类名替换为：

```tsx
<div className="grid grid-cols-2 gap-x-4 gap-y-1 sm:grid-cols-3 xl:grid-cols-6">
```

在“进行中请求”状态项之后、网格结束之前加入：

```tsx
<StatusItem
  label="可用积分"
  value={creditDisplay.value}
  detail={creditDisplay.detail}
/>
```

- [ ] **Step 4: 从 dashboard 传入真实汇总**

在 `admin-ui/src/components/dashboard.tsx` 的 `totalInFlight` import 后加入：

```ts
import { summarizeAvailableCredits } from '@/lib/credential-summary'
```

在现有 `inFlightRequestCount` 后加入：

```ts
const availableCreditSummary = summarizeAvailableCredits(
  data?.credentials ?? [],
  balanceMap,
)
```

将 `RpmStatusBar` 调用替换为：

```tsx
<RpmStatusBar
  summary={data ? data.rpmSummary : undefined}
  totalInFlight={inFlightRequestCount}
  availableCreditSummary={availableCreditSummary}
/>
```

- [ ] **Step 5: 更新旧状态栏网格断言**

在 `admin-ui/src/components/admin-rpm-operations-ui.contract.test.ts` 的 `status bar exposes finite and unlimited rolling-window capacity` 测试中，将：

```ts
expect(status).toContain('sm:grid-cols-5')
```

替换为：

```ts
expect(status).toContain('sm:grid-cols-3')
expect(status).toContain('xl:grid-cols-6')
```

- [ ] **Step 6: 运行聚焦测试、全部测试和类型构建**

Run:

```powershell
bun test src/lib/credential-summary.test.ts src/components/admin-credential-observability-ui.contract.test.ts src/components/admin-rpm-operations-ui.contract.test.ts
bun test
bun run build
```

Expected: 聚焦测试、全部测试和 `tsc -b && vite build` 均成功。

- [ ] **Step 7: 提交积分 UI 接线**

```powershell
git add -- admin-ui/src/components/dashboard.tsx admin-ui/src/components/rpm-status-bar.tsx admin-ui/src/components/admin-rpm-operations-ui.contract.test.ts admin-ui/src/components/admin-credential-observability-ui.contract.test.ts
git diff --cached --check
git commit -m "feat(admin): 在状态栏展示可用积分"
```

---

### Task 3: 账号徽章从裁剪改为自动换行

**Files:**
- Modify: `admin-ui/src/components/admin-credential-observability-ui.contract.test.ts`
- Modify: `admin-ui/src/components/credential-card.tsx:684`

- [ ] **Step 1: 写徽章完整显示失败测试**

在 `admin credential observability UI wiring` describe 内加入：

```ts
test('credential identity badges wrap without clipping', async () => {
  const card = await readSource('src/components/credential-card.tsx')
  const badgeRow = card.match(/<div className="([^"]*\[&>\*\]:shrink-0[^"]*)">\s*\{badges\}/)?.[1]

  expect(badgeRow).toBeDefined()
  expect(badgeRow).toContain('flex-wrap')
  expect(badgeRow).toContain('gap-x-1')
  expect(badgeRow).toContain('gap-y-1')
  expect(badgeRow).not.toContain('overflow-hidden')
  expect(card).toContain('truncate text-sm font-medium leading-5')
})
```

- [ ] **Step 2: 运行聚焦测试并确认因仍在裁剪而失败**

Run: `bun test src/components/admin-credential-observability-ui.contract.test.ts`

Expected: `credential identity badges wrap without clipping` FAIL，提示缺少 `flex-wrap` 或仍含 `overflow-hidden`。

- [ ] **Step 3: 替换徽章容器类名**

在 `admin-ui/src/components/credential-card.tsx` 将：

```tsx
<div className="mt-1 flex min-w-0 items-center gap-1 overflow-hidden [&>*]:shrink-0">
```

替换为：

```tsx
<div className="mt-1 flex min-w-0 flex-wrap items-center gap-x-1 gap-y-1 [&>*]:shrink-0">
```

- [ ] **Step 4: 运行聚焦测试并确认通过**

Run: `bun test src/components/admin-credential-observability-ui.contract.test.ts`

Expected: 3 tests PASS，0 FAIL。

- [ ] **Step 5: 提交账号展示修复**

```powershell
git add -- admin-ui/src/components/credential-card.tsx admin-ui/src/components/admin-credential-observability-ui.contract.test.ts
git diff --cached --check
git commit -m "fix(admin): 完整显示账号身份徽章"
```

---

### Task 4: 失败尝试排序与恢复分类纯函数

**Files:**
- Create: `admin-ui/src/lib/failure-trace.test.ts`
- Create: `admin-ui/src/lib/failure-trace.ts`

- [ ] **Step 1: 写失败链派生测试**

创建 `admin-ui/src/lib/failure-trace.test.ts`：

```ts
import { describe, expect, test } from 'bun:test'
import type { TraceAttempt, TraceRecord } from '@/types/api'
import {
  compactAttemptLabel,
  failureDisposition,
  failureDispositionLabel,
  sortTraceAttempts,
} from './failure-trace'

function attempt(
  index: number,
  credentialId: number,
  endpoint: string,
  outcome: string,
  httpStatus: number | null,
): TraceAttempt {
  return {
    attempt: index,
    credentialId,
    endpoint,
    outcome,
    httpStatus,
    email: null,
    errorSnippet: outcome === 'success' ? null : 'upstream error',
    durationMs: 20,
  }
}

function record(overrides: Partial<TraceRecord> = {}): TraceRecord {
  return {
    traceId: 'trace-1',
    ts: '2026-07-16T00:00:00Z',
    keyId: 1,
    keySource: 'clientKey',
    keyName: 'newapi',
    responseMode: 'detection',
    model: 'claude-opus-4-8',
    isStream: true,
    finalStatus: 'error',
    finalCredentialId: 202,
    errorType: 'account_throttled',
    errorMessage: null,
    totalAttempts: 0,
    durationMs: 40,
    interruptedAfterBytes: null,
    attempts: [],
    ...overrides,
  }
}

describe('sortTraceAttempts', () => {
  test('按 attempt 升序返回副本且不修改查询缓存数组', () => {
    const original = [
      attempt(2, 203, 'ide', 'success', 200),
      attempt(0, 202, 'ide', 'account_throttled', 429),
      attempt(1, 202, 'runtime', 'account_throttled', 429),
    ]

    expect(sortTraceAttempts(original).map((item) => item.attempt)).toEqual([0, 1, 2])
    expect(original.map((item) => item.attempt)).toEqual([2, 0, 1])
  })
})

describe('failureDisposition', () => {
  test('区分同账号换端点、同端点重试和换账号成功', () => {
    expect(
      failureDisposition(
        record({
          finalStatus: 'success',
          finalCredentialId: 202,
          attempts: [
            attempt(0, 202, 'ide', 'account_throttled', 429),
            attempt(1, 202, 'runtime', 'success', 200),
          ],
        }),
        202,
      ),
    ).toBe('switched_endpoint')

    expect(
      failureDisposition(
        record({
          finalStatus: 'success',
          finalCredentialId: 202,
          attempts: [
            attempt(0, 202, 'ide', 'transient', 503),
            attempt(1, 202, 'ide', 'success', 200),
          ],
        }),
        202,
      ),
    ).toBe('retried_same_endpoint')

    expect(
      failureDisposition(
        record({
          finalStatus: 'success',
          finalCredentialId: 203,
          attempts: [
            attempt(0, 202, 'ide', 'account_throttled', 429),
            attempt(1, 203, 'ide', 'success', 200),
          ],
        }),
        202,
      ),
    ).toBe('switched_credential')
  })

  test('区分最终失败、流式中断和未到达上游', () => {
    expect(
      failureDisposition(
        record({ attempts: [attempt(0, 202, 'ide', 'bad_request', 400)] }),
        202,
      ),
    ).toBe('failed')

    expect(
      failureDisposition(
        record({
          finalStatus: 'interrupted',
          attempts: [attempt(0, 202, 'ide', 'stream_interrupted', 200)],
        }),
        202,
      ),
    ).toBe('interrupted')

    expect(failureDisposition(record(), 202)).toBe('not_sent')
  })

  test('每种分类都有稳定中文结论', () => {
    expect(failureDispositionLabel('switched_endpoint')).toBe('同账号切换端点后成功')
    expect(failureDispositionLabel('retried_same_endpoint')).toBe('同账号重试后成功')
    expect(failureDispositionLabel('switched_credential')).toBe('切换其他账号后成功')
    expect(failureDispositionLabel('interrupted')).toBe('流式响应中断')
    expect(failureDispositionLabel('failed')).toBe('最终失败')
    expect(failureDispositionLabel('not_sent')).toBe('请求未到达上游')
  })
})

describe('compactAttemptLabel', () => {
  test('紧凑标签明确账号、端点和 HTTP/网络结果', () => {
    expect(compactAttemptLabel(attempt(0, 202, 'ide', 'account_throttled', 429)))
      .toBe('#202 / ide 429')
    expect(compactAttemptLabel(attempt(1, 202, '', 'network_error', null)))
      .toBe('#202 / 未知端点 网络错误')
  })
})
```

- [ ] **Step 2: 运行测试并确认因模块缺失而失败**

Run: `bun test src/lib/failure-trace.test.ts`

Expected: FAIL，错误包含 `Cannot find module './failure-trace'`。

- [ ] **Step 3: 写最小失败链派生实现**

创建 `admin-ui/src/lib/failure-trace.ts`：

```ts
import type { TraceAttempt, TraceRecord } from '@/types/api'

export type FailureDisposition =
  | 'switched_endpoint'
  | 'retried_same_endpoint'
  | 'switched_credential'
  | 'interrupted'
  | 'failed'
  | 'not_sent'

export function sortTraceAttempts(
  attempts: ReadonlyArray<TraceAttempt>,
): TraceAttempt[] {
  return [...attempts].sort((left, right) => left.attempt - right.attempt)
}

export function failureDisposition(
  record: TraceRecord,
  inspectedCredentialId: number,
): FailureDisposition {
  const attempts = sortTraceAttempts(record.attempts)
  if (attempts.length === 0) return 'not_sent'
  if (record.finalStatus === 'interrupted') return 'interrupted'
  if (record.finalStatus !== 'success') return 'failed'
  if (record.finalCredentialId !== inspectedCredentialId) return 'switched_credential'

  const successAttempt = [...attempts]
    .reverse()
    .find(
      (item) =>
        item.credentialId === inspectedCredentialId && item.outcome === 'success',
    )
  const failedAttempts = attempts.filter(
    (item) =>
      item.credentialId === inspectedCredentialId && item.outcome !== 'success',
  )
  const switchedEndpoint =
    successAttempt != null &&
    failedAttempts.some(
      (item) => item.endpoint.trim() !== successAttempt.endpoint.trim(),
    )

  return switchedEndpoint ? 'switched_endpoint' : 'retried_same_endpoint'
}

export function failureDispositionLabel(disposition: FailureDisposition): string {
  switch (disposition) {
    case 'switched_endpoint':
      return '同账号切换端点后成功'
    case 'retried_same_endpoint':
      return '同账号重试后成功'
    case 'switched_credential':
      return '切换其他账号后成功'
    case 'interrupted':
      return '流式响应中断'
    case 'failed':
      return '最终失败'
    case 'not_sent':
      return '请求未到达上游'
  }
}

export function compactAttemptLabel(attempt: TraceAttempt): string {
  const endpoint = attempt.endpoint.trim() || '未知端点'
  const status = attempt.httpStatus == null ? '网络错误' : String(attempt.httpStatus)
  return `#${attempt.credentialId} / ${endpoint} ${status}`
}
```

- [ ] **Step 4: 运行聚焦测试并确认通过**

Run: `bun test src/lib/failure-trace.test.ts`

Expected: 5 tests PASS，0 FAIL。

- [ ] **Step 5: 提交失败链纯函数**

```powershell
git add -- admin-ui/src/lib/failure-trace.ts admin-ui/src/lib/failure-trace.test.ts
git diff --cached --check
git commit -m "feat(admin): 增加失败链恢复分类"
```

---

### Task 5: 凭据失败弹窗按请求展示完整尝试链

**Files:**
- Modify: `admin-ui/src/components/admin-credential-observability-ui.contract.test.ts`
- Modify: `admin-ui/src/components/credential-failures-dialog.tsx:1-145`

- [ ] **Step 1: 写请求级失败弹窗接线测试**

在 `admin credential observability UI wiring` describe 内加入：

```ts
test('failure dialog groups records and exposes the complete endpoint chain', async () => {
  const dialog = await readSource('src/components/credential-failures-dialog.tsx')

  expect(dialog).toContain('records.map((rec)')
  expect(dialog).toContain('sortTraceAttempts')
  expect(dialog).toContain('failureDisposition')
  expect(dialog).toContain('failureDispositionLabel')
  expect(dialog).toContain('compactAttemptLabel')
  expect(dialog).toContain('客户端 Key：')
  expect(dialog).toContain('端点：')
  expect(dialog).toContain('耗时')
  expect(dialog).not.toContain('failedHops')
  expect(dialog).not.toContain('本次请求最终由其他凭据成功')
})
```

- [ ] **Step 2: 运行聚焦测试并确认旧弹窗仍在展平失败跳**

Run:

```powershell
bun test src/lib/failure-trace.test.ts src/components/admin-credential-observability-ui.contract.test.ts
```

Expected: 新增 UI 测试 FAIL；纯函数测试 PASS。

- [ ] **Step 3: 用请求卡片替换失败跳展平逻辑**

将 `admin-ui/src/components/credential-failures-dialog.tsx` 完整替换为：

```tsx
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from '@/components/ui/dialog'
import { Badge } from '@/components/ui/badge'
import { useTraces } from '@/hooks/use-traces'
import {
  compactAttemptLabel,
  failureDisposition,
  failureDispositionLabel,
  sortTraceAttempts,
  type FailureDisposition,
} from '@/lib/failure-trace'
import type { TraceAttempt, TraceRecord } from '@/types/api'

interface CredentialFailuresDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  credentialId: number
  email?: string
}

function outcomeStyle(outcome: string | null): {
  label: string
  variant: 'destructive' | 'warning' | 'outline' | 'secondary'
} {
  switch (outcome) {
    case 'success':
      return { label: '成功', variant: 'secondary' }
    case 'quota_exhausted':
      return { label: '额度耗尽', variant: 'warning' }
    case 'account_throttled':
      return { label: '账号风控', variant: 'warning' }
    case 'auth_failed':
      return { label: '鉴权失败', variant: 'destructive' }
    case 'transient':
      return { label: '瞬态错误', variant: 'outline' }
    case 'network_error':
      return { label: '网络错误', variant: 'destructive' }
    case 'bad_request':
      return { label: '请求错误', variant: 'destructive' }
    case 'stream_interrupted':
      return { label: '流中断', variant: 'warning' }
    default:
      return { label: outcome || '未知', variant: 'secondary' }
  }
}

function recoveryVariant(
  disposition: FailureDisposition,
): 'destructive' | 'warning' | 'outline' | 'secondary' {
  if (disposition === 'failed') return 'destructive'
  if (disposition === 'interrupted') return 'warning'
  if (disposition === 'not_sent') return 'secondary'
  return 'outline'
}

function formatTime(ts: string): string {
  const date = new Date(ts)
  if (Number.isNaN(date.getTime())) return ts
  return date.toLocaleString('zh-CN', { hour12: false })
}

function keySourceLabel(rec: TraceRecord): string {
  return rec.keyName ?? `#${rec.keyId}`
}

function credentialLabel(attempt: TraceAttempt): string {
  return attempt.email
    ? `${attempt.email} (#${attempt.credentialId})`
    : `#${attempt.credentialId}`
}

function AttemptRow({ attempt, position }: { attempt: TraceAttempt; position: number }) {
  const style = outcomeStyle(attempt.outcome)
  const endpoint = attempt.endpoint.trim() || '未知端点'
  const http = attempt.httpStatus == null ? '网络错误' : `HTTP ${attempt.httpStatus}`

  return (
    <div className="rounded-md border border-border/50 bg-background/60 p-2.5">
      <div className="flex flex-wrap items-center gap-2 text-[12px]">
        <Badge variant="secondary">第 {position + 1} 跳</Badge>
        <span className="font-medium">{credentialLabel(attempt)}</span>
        <Badge variant="outline">端点：{endpoint}</Badge>
        <span className="font-mono text-muted-foreground">{http}</span>
        <span className="text-muted-foreground">耗时 {attempt.durationMs} ms</span>
        <Badge variant={style.variant}>{style.label}</Badge>
      </div>
      {attempt.errorSnippet ? (
        <pre className="mt-2 max-h-32 overflow-auto whitespace-pre-wrap break-all rounded-md bg-secondary/50 p-2 font-mono text-[11px] text-muted-foreground">
          {attempt.errorSnippet}
        </pre>
      ) : null}
    </div>
  )
}

function FailureRequestCard({
  rec,
  inspectedCredentialId,
}: {
  rec: TraceRecord
  inspectedCredentialId: number
}) {
  const attempts = sortTraceAttempts(rec.attempts)
  const disposition = failureDisposition(rec, inspectedCredentialId)
  const retryCount = Math.max(0, attempts.length - 1)

  return (
    <article className="rounded-lg border border-border/60 bg-secondary/30 p-3">
      <div className="flex flex-wrap items-center gap-2 text-[13px]">
        <span className="tabular-nums text-muted-foreground">{formatTime(rec.ts)}</span>
        <Badge variant="secondary">客户端 Key：{keySourceLabel(rec)}</Badge>
        <Badge variant={recoveryVariant(disposition)}>
          {failureDispositionLabel(disposition)}
        </Badge>
        <span className="text-[12px] text-muted-foreground">
          尝试 {attempts.length} 次（含 {retryCount} 次重试）
        </span>
        <span className="ml-auto text-[12px] text-muted-foreground">{rec.model}</span>
      </div>

      {attempts.length === 0 ? (
        <div className="mt-3 text-[13px] text-muted-foreground">请求未到达上游</div>
      ) : (
        <>
          <div className="mt-3 flex flex-wrap items-center gap-1 text-[12px] font-mono text-muted-foreground">
            {attempts.map((attempt, index) => (
              <span key={`${rec.traceId}-summary-${attempt.attempt}`} className="inline-flex items-center gap-1">
                {index > 0 ? <span aria-hidden="true">→</span> : null}
                <span>{compactAttemptLabel(attempt)}</span>
              </span>
            ))}
          </div>
          <div className="mt-3 space-y-2">
            {attempts.map((attempt, index) => (
              <AttemptRow
                key={`${rec.traceId}-${attempt.attempt}`}
                attempt={attempt}
                position={index}
              />
            ))}
          </div>
        </>
      )}

      {rec.finalStatus === 'interrupted' && rec.interruptedAfterBytes != null ? (
        <div className="mt-2 text-[12px] text-muted-foreground">
          中断前已发送 {rec.interruptedAfterBytes} 字节
        </div>
      ) : null}
    </article>
  )
}

export function CredentialFailuresDialog({
  open,
  onOpenChange,
  credentialId,
  email,
}: CredentialFailuresDialogProps) {
  const { data, isLoading } = useTraces(
    { failedAttemptCredentialId: credentialId, limit: 50 },
    open,
  )
  const records = data?.records ?? []

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-4xl">
        <DialogHeader>
          <DialogTitle>失败日志详情</DialogTitle>
          <DialogDescription>
            {email || `凭据 #${credentialId}`} 最近涉及失败的请求（最多 50 个请求）
          </DialogDescription>
        </DialogHeader>
        <div className="max-h-[70vh] space-y-3 overflow-y-auto pr-1">
          {isLoading ? (
            <div className="py-6 text-center text-sm text-muted-foreground">加载中…</div>
          ) : records.length === 0 ? (
            <div className="py-6 text-center text-sm text-muted-foreground">
              该凭据暂无失败记录（trace 关闭或近期无失败）。
            </div>
          ) : (
            records.map((rec) => (
              <FailureRequestCard
                key={rec.traceId}
                rec={rec}
                inspectedCredentialId={credentialId}
              />
            ))
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}
```

- [ ] **Step 4: 运行失败链和 UI 聚焦测试**

Run:

```powershell
bun test src/lib/failure-trace.test.ts src/components/admin-credential-observability-ui.contract.test.ts
```

Expected: 所有聚焦测试 PASS，0 FAIL。

- [ ] **Step 5: 运行完整前端测试与构建**

Run:

```powershell
bun test
bun run build
```

Expected: 全部 Bun tests PASS；`tsc -b && vite build` 退出码为 0。

- [ ] **Step 6: 提交请求级失败弹窗**

```powershell
git add -- admin-ui/src/components/credential-failures-dialog.tsx admin-ui/src/components/admin-credential-observability-ui.contract.test.ts
git diff --cached --check
git commit -m "fix(admin): 展示完整429重试链路"
```

---

### Task 6: 最终回归与范围审计

**Files:**
- Verify only: `admin-ui/src/**`

- [ ] **Step 1: 运行全部前端测试**

Run:

```powershell
cd D:/kiro2api/kiro-rs2/kiro.rs-admin/.worktrees/admin-credential-visibility-credit-trace/admin-ui
bun test
```

Expected: 现有 78 个基线测试与本次新增 13 个测试全部 PASS（合计 91），0 FAIL。

- [ ] **Step 2: 运行生产构建**

Run: `bun run build`

Expected: `tsc -b && vite build` 成功，Vite 输出 `✓ built`。

- [ ] **Step 3: 检查差异质量和后端零改动**

Run:

```powershell
cd ..
git diff master...HEAD --check
git diff master...HEAD --name-only
git status --short
```

Expected:

- `git diff --check` 无输出。
- 变更文件只位于 `admin-ui/` 和已提交的两份 `docs/superpowers/` 文档。
- 不出现 `src/*.rs`、`Cargo.toml`、配置文件、数据库或凭据文件。
- `git status --short` 无输出。

- [ ] **Step 4: 复核客户影响边界**

逐项对照 `docs/superpowers/specs/2026-07-16-admin-credential-visibility-credit-trace-design.md` 验证：

```text
账号徽章：只改变管理端换行布局
可用积分：只汇总已有 balance/balanceMap，不触发查询
429 弹窗：只读取 TraceRecord.attempts，不改变重试
聊天/SSE/首字节/Token：无文件改动、无行为改动
```

Expected: 四项全部满足；如果出现范围外文件，停止合并并先移除范围外改动。

- [ ] **Step 5: 记录最终提交序列**

Run:

```powershell
git log --oneline master..HEAD
```

Expected: 包含设计文档提交及五个实现提交，提交主题分别覆盖积分汇总、积分展示、徽章显示、恢复分类和 429 链路展示。
