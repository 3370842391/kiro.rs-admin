# 在线更新强制检查 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让在线更新弹窗的手动按钮真正绕过后端 30 分钟缓存，同时保留自动查询的缓存与限流保护。

**Architecture:** 保留现有 `checkSystemUpdate(force)` API 和普通 React Query 查询，在纯函数边界中固定传入 `true`，由弹窗的独立 mutation 调用。强制查询成功后立即覆盖 `['system-update-check']` 缓存，失败或后端回退旧缓存时沿用现有 warning/error 展示。

**Tech Stack:** React 19、TypeScript 6、TanStack Query 5、Axios、Bun test、Vite

---

### Task 1: 为强制检查调用建立 RED→GREEN 单元测试

**Files:**
- Create: `admin-ui/src/lib/update-check.test.ts`
- Create: `admin-ui/src/lib/update-check.ts`

- [ ] **Step 1: Write the failing test**

创建 `admin-ui/src/lib/update-check.test.ts`：

```ts
import { expect, test } from 'bun:test'
import type { UpdateCheckInfo } from '@/types/api'
import { forceCheckSystemUpdate } from './update-check'

test('force update check always bypasses the backend cache', async () => {
  let receivedForce: boolean | undefined
  const expected: UpdateCheckInfo = {
    currentVersion: '0.8.6',
    latestVersion: '0.8.7',
    hasUpdate: true,
    buildType: 'binary',
    checkedAt: '2026-07-13T00:16:04+08:00',
    cached: false,
  }

  const result = await forceCheckSystemUpdate(async (force) => {
    receivedForce = force
    return expected
  })

  expect(receivedForce).toBe(true)
  expect(result).toBe(expected)
})
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```powershell
cd admin-ui
bun test src/lib/update-check.test.ts
```

Expected: FAIL because `src/lib/update-check.ts` / `forceCheckSystemUpdate` does not exist.

- [ ] **Step 3: Write minimal implementation**

创建 `admin-ui/src/lib/update-check.ts`：

```ts
import { checkSystemUpdate } from '@/api/credentials'
import type { UpdateCheckInfo } from '@/types/api'

export type SystemUpdateFetcher = (
  force?: boolean,
) => Promise<UpdateCheckInfo>

export function forceCheckSystemUpdate(
  fetcher: SystemUpdateFetcher = checkSystemUpdate,
): Promise<UpdateCheckInfo> {
  return fetcher(true)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run:

```powershell
bun test src/lib/update-check.test.ts
```

Expected: 1 pass, 0 fail.

- [ ] **Step 5: Commit**

```powershell
git add -- admin-ui/src/lib/update-check.ts admin-ui/src/lib/update-check.test.ts
git commit -m "test(update): 固化强制版本检查契约"
```

### Task 2: 将弹窗手动按钮接入强制查询 mutation

**Files:**
- Modify: `admin-ui/src/components/image-update-dialog.tsx`

- [ ] **Step 1: Add the force-check mutation**

在 import 中加入：

```ts
import { forceCheckSystemUpdate } from '@/lib/update-check'
```

在 `applyMutation` 之前加入：

```ts
const forceCheckMutation = useMutation({
  mutationFn: forceCheckSystemUpdate,
  onSuccess: (result) => {
    queryClient.setQueryData(['system-update-check'], result)
    if (result.warning) {
      toast.warning(result.warning)
    } else if (result.hasUpdate) {
      toast.success(`发现新版本 v${result.latestVersion}`)
    } else {
      toast.success('当前已是最新版本')
    }
  },
  onError: (err) => {
    toast.error(`检查更新失败: ${extractErrorMessage(err)}`)
  },
})
```

- [ ] **Step 2: Use a single checking state**

在派生状态中加入：

```ts
const isChecking = isFetching || forceCheckMutation.isPending
const canUpdate =
  !!updateCheck?.hasUpdate &&
  !applyMutation.isPending &&
  !forceCheckMutation.isPending
```

删除旧的单行 `canUpdate` 定义。

- [ ] **Step 3: Rewire the button**

把左下角按钮调整为：

```tsx
<Button
  type="button"
  variant="ghost"
  size="sm"
  disabled={isChecking || applyMutation.isPending}
  onClick={() => forceCheckMutation.mutate()}
  title="绕过缓存并重新查询 GitHub Release"
>
  <RefreshCw
    className={`h-3.5 w-3.5 ${isChecking ? 'animate-spin' : ''}`}
  />
  <span className="ml-1.5">强制检查</span>
</Button>
```

从 `useQuery` 解构中删除不再使用的 `refetch`。

- [ ] **Step 4: Run focused test and production build**

Run:

```powershell
cd admin-ui
bun test src/lib/update-check.test.ts src/lib/cache-policy.test.ts
bun run build
```

Expected: 3 tests pass and Vite build exits 0.

- [ ] **Step 5: Commit**

```powershell
git add -- admin-ui/src/components/image-update-dialog.tsx
git commit -m "fix(update): 手动检查绕过版本缓存"
```

### Task 3: 文档与本地验收

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Document manual and automatic semantics**

在“在线更新和发布”章节补充：

```md
- 弹窗打开和后台轮询使用 30 分钟服务端缓存，降低 GitHub API 压力。
- “强制检查”会请求 `/api/admin/system/update/check?force=true`，用于 Release 刚发布时立即刷新；查询失败时可能返回带 warning 的旧缓存结果。
```

- [ ] **Step 2: Run complete frontend verification**

```powershell
cd admin-ui
bun test src/lib/update-check.test.ts src/lib/cache-policy.test.ts
bun run build
cd ..
git diff --check
```

Expected: tests/build pass and diff check exits 0.

- [ ] **Step 3: Commit**

```powershell
git add -- README.md
git commit -m "docs(update): 说明强制检查与缓存边界"
```

- [ ] **Step 4: Manual acceptance against the running old instance**

部署包含本改动的管理面板后，打开在线更新弹窗并点击“强制检查”。验收标准：

```text
currentVersion = 0.8.6
latestVersion = 0.8.7
hasUpdate = true
cached = false
```

界面必须立即显示“可更新 → v0.8.7”，不等待 30 分钟。
