# 在线更新强制检查设计

## 背景

管理端的在线更新后端已经支持 `GET /api/admin/system/update/check?force=true`，前端 API `checkSystemUpdate(force)` 也会在 `force=true` 时发送该参数。当前弹窗中的“检查”按钮调用 React Query 的普通 `refetch()`，其固定查询函数仍为 `checkSystemUpdate(false)`，因此手动操作继续命中后端 30 分钟缓存。

当前运行实例为 `v0.8.6`，GitHub 最新 Release 为 `v0.8.7`。Release 已发布且包含 `Linux-musl-x64` 资产，现象可归因于手动按钮未使用现有强制刷新能力，而不是 Release 缺失或版本号未递增。

## 目标

- 打开弹窗和后台轮询继续使用普通缓存查询。
- 用户点击手动按钮时绕过后端 30 分钟缓存。
- 强制查询结果立即更新弹窗和全局更新提示使用的 React Query 缓存。
- GitHub 查询失败并回退旧缓存时，明确展示后端返回的 warning，不把旧数据伪装成刚获取的结果。
- 更新安装流程、GitHub Release 解析和自动轮询频率保持不变。

## 前端设计

在 `admin-ui/src/lib/update-check.ts` 提供一个可注入查询函数的纯异步边界：

```ts
export type SystemUpdateFetcher = (force?: boolean) => Promise<UpdateCheckInfo>

export function forceCheckSystemUpdate(
  fetcher: SystemUpdateFetcher = checkSystemUpdate,
): Promise<UpdateCheckInfo> {
  return fetcher(true)
}
```

`ImageUpdateDialog` 使用独立 `useMutation` 执行 `forceCheckSystemUpdate()`。成功时通过 `queryClient.setQueryData(['system-update-check'], result)` 立即覆盖普通查询缓存；失败时通过现有 `extractErrorMessage` 显示错误。手动检查进行中时禁用强制检查和更新按钮，避免检查与二进制替换并发。

弹窗左下角按钮文案改为“强制检查”，旋转图标同时响应普通查询和强制 mutation 的加载状态。弹窗打开时的 `useQuery` 及 `useUpdateCheck` 的自动轮询继续调用 `checkSystemUpdate(false)`，前端 `staleTime` 和后端 TTL 均不调整。

## 数据与错误语义

- 远程查询成功：响应 `cached=false`，界面立即显示最新版本。
- 远程查询失败且后端存在旧缓存：后端返回 `cached=true` 和 `warning`，界面保留版本信息并显示 warning。
- 远程查询失败且无缓存：界面显示 warning，不能启用“更新并重启”。
- 强制查询请求本身失败：toast 显示请求错误，现有查询数据不被清空。
- GitHub 只有普通 commit/Actions artifact、没有 Release 时，在线更新不会把它视为可更新版本；该既有语义保持不变。

## 测试与验收

- 先为 `forceCheckSystemUpdate` 编写 Bun 单元测试，使用注入 fetcher 断言收到的参数严格为 `true`，形成 RED。
- 最小实现纯函数后运行测试形成 GREEN。
- 运行前端 TypeScript 与 Vite 生产构建，验证组件 mutation、查询缓存写回和 JSX 类型。
- 人工验收：运行旧版实例时打开在线更新弹窗，点击“强制检查”，应从 `v0.8.6` 立即发现 GitHub Release `v0.8.7`，无需等待 30 分钟。

## 非目标

- 不缩短或删除后端 30 分钟缓存。
- 不让弹窗每次打开都强制调用 GitHub。
- 不改变 Release 发布、下载、SHA256 校验或进程重启逻辑。
