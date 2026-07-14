# 管理端批量 RPM 与运行容量可视化设计

日期：2026-07-14
状态：待用户审阅

## 1. 目标

在不改变模型请求、缓存计费和客户对话内容的前提下，提升大量凭据的日常管理效率，并让管理员能直接看到最近 60 秒的总请求负载和可用 RPM 容量。

本设计只处理管理端账号配置与只读运行指标。严格 SSE、首字节策略、CCTest/Ztest 协议指标和运行时 DEBUG 控制拆分为后续独立子项目，避免账号批量写入与协议主链路在同一批次上线。

## 2. 已有能力与缺口

### 已有能力

- 凭据列表已经支持全选当前页。
- 凭据列表已经支持跨页全选全部筛选结果。
- 单个凭据已经返回并显示 `rpmCurrent / rpmLimit`。
- `rpmCurrent` 使用 TokenManager 最近 60 秒请求时间戳滑动窗口，是真实滚动窗口，不是自然分钟计数。
- 单凭据更新接口已经支持 `rpmLimit`、分组和来源渠道。
- 批量编辑窗口已经支持分组与来源渠道，但通过前端逐账号调用单更新接口。

### 当前缺口

- 批量编辑窗口不能修改 RPM。
- 前端逐账号更新会产生大量 HTTP 请求，过程中失败时形成部分成功状态。
- API 没有返回所有账号的总 RPM、有限容量、剩余容量、不限速账号数和满载账号数。
- 顶部没有集群级 RPM 运行摘要，管理员必须逐卡片相加。
- TokenManager 内部已有 `in_flight`，但 Admin API 没有暴露，无法观察当前并发占用。

## 3. 方案比较

### 方案 A：继续由前端循环调用单账号接口

优点是改动最少。缺点是账号多时请求慢、重复持久化、失败后状态不一致，也无法给出可靠的整体结果。仅保留为旧客户端兼容路径，不作为新批量操作实现。

### 方案 B：增加服务端批量补丁接口（推荐）

服务端一次接收全部 ID 和补丁，先完成全量校验，再在一次内存批次中更新，并只持久化一次。前端只发送一个请求并显示统一结果。它能显著减少管理端操作时间和凭据文件写入次数，且容易覆盖自动化测试。

### 方案 C：直接修改 `credentials.json` 后重启

实现简单，但会绕过运行时状态、管理端鉴权和校验，并中断所有请求。拒绝采用。

## 4. 后端设计

### 4.1 批量更新接口

新增：

```text
PUT /api/admin/credentials/batch
```

请求：

```json
{
  "ids": [1, 2, 3],
  "rpmLimit": 20,
  "groups": {
    "mode": "add",
    "values": ["ztest"]
  },
  "sourceChannel": "test-pool"
}
```

字段规则：

- `ids`：必填，数量为 1..=10000；重复 ID 或不存在 ID 时整体拒绝。
- `rpmLimit`：可选，`0` 表示不限速，最大值为 `100000`。
- `groups`：可选，支持 `replace`、`add`、`remove`；值先 trim、去空和去重。
- `sourceChannel`：可选；空字符串表示清除，字段缺省表示不修改。
- 请求至少包含一个实际补丁字段。
- 第一版不批量修改 refresh/access token、代理密码、优先级或凭据 ID。

响应：

```json
{
  "selected": 3,
  "updated": 3,
  "unchanged": 0,
  "rpmSummary": {
    "windowSeconds": 60,
    "current": 12,
    "limitedCapacity": 60,
    "remainingLimitedCapacity": 48,
    "unlimitedAccounts": 0,
    "saturatedAccounts": 0
  }
}
```

处理顺序：

1. 校验 ID、补丁和范围。
2. 确认所有 ID 都存在，再执行任何修改。
3. 在 TokenManager 单次写入批次中计算每个账号的新值。
4. 只持久化一次 `credentials.json`。
5. 返回变更数量和更新后的 RPM 汇总。

若持久化失败，接口返回错误并记录日志；不得把 refresh token、access token 或代理密码写入响应和错误消息。

### 4.2 RPM 汇总

扩展 `CredentialsStatusResponse`，新增：

```json
{
  "rpmSummary": {
    "windowSeconds": 60,
    "current": 42,
    "limitedCapacity": 180,
    "remainingLimitedCapacity": 138,
    "unlimitedAccounts": 2,
    "saturatedAccounts": 1,
    "enabledAccounts": 20
  }
}
```

口径：

- `current`：所有凭据最近 60 秒请求数之和，包括刚被禁用但窗口尚未过期的请求，代表真实已发生负载。
- `limitedCapacity`：当前未禁用且 `rpmLimit > 0` 的账号上限之和。
- `remainingLimitedCapacity`：有限账号的 `max(rpmLimit - rpmCurrent, 0)` 之和。
- `unlimitedAccounts`：当前未禁用且 `rpmLimit == 0` 的账号数量。
- `saturatedAccounts`：当前未禁用、有限速且 `rpmCurrent >= rpmLimit` 的账号数量。
- `enabledAccounts`：未手动或自动禁用的账号数量；冷却账号仍属于启用账号，但 UI 单独显示冷却状态。
- 存在不限速账号时不伪造“总容量”数字，UI 显示“容量不限”，同时保留有限账号容量明细。

### 4.3 单账号运行字段

`CredentialEntrySnapshot` 和 `CredentialStatusItem` 新增只读 `inFlight`。该字段来自现有 TokenManager RAII 计数，不新增请求路径锁。

前端可从 `rpmCurrent` 和 `rpmLimit` 计算剩余量与利用率，不在 API 中重复保存派生字段。

## 5. 前端设计

### 5.1 批量编辑窗口

沿用现有 `BatchEditCredentialDialog`，增加“修改 RPM”开关和数字输入：

- 输入 `0` 时明确显示“不限速”。
- 输入正整数时显示“每账号每 60 秒最多 N 次”。
- 未开启开关时不发送 `rpmLimit`。
- 提交前显示将影响的账号数量和补丁摘要。
- 改为调用一个批量 API，不再逐账号循环。
- 成功后刷新凭据查询并清空选择；失败时保留选择，方便修正后重试。

分组和来源渠道同时迁移到同一批量 API，保持现有 replace/add/remove 语义。

### 5.2 全局 RPM 状态条

在凭据列表筛选栏下方增加一个紧凑、无嵌套卡片的状态条：

- `当前 RPM`：最近 60 秒真实请求总数。
- `有限容量`：有限账号上限之和；存在不限速账号时附带“不限速账号 N”。
- `剩余`：有限账号剩余 RPM。
- `满载账号`：达到 RPM 上限的账号数量。
- `进行中`：所有账号 `inFlight` 之和。

状态条使用现有查询数据，不新增独立轮询。桌面横向排列，移动端两列换行；数值使用等宽数字，不使用装饰性大卡片。

### 5.3 单账号展示增强

- 保留现有 `当前/上限` RPM。
- 达到 80% 时使用警示色，达到上限时使用错误色和“已满载”提示。
- 显示 `inFlight`，仅在大于 0 时突出。
- Tooltip 明确 RPM 是滚动 60 秒窗口，避免误解为自然分钟清零。

## 6. 并发与一致性

- 批量更新只修改配置字段，不清空已有 RPM 窗口，也不中断正在进行的请求。
- 降低 RPM 后，已发生请求不会被删除；账号在窗口自然下降到新上限以下前暂停获取新请求。
- 提高 RPM 后立即对后续凭据选择生效。
- `rpmLimit=0` 表示不限速，不是禁用账号。
- 批量请求校验失败时不得修改任何目标账号。
- 多管理员同时修改时以服务端收到顺序为准；每个批量请求内部保持全量校验和单次持久化。

## 7. 客户影响

- 不修改 Anthropic/OpenAI 请求体、系统提示词、工具参数、缓存拆分或计费。
- 不增加模型 Token。
- RPM 降低可能让新请求更早切换到其他账号；当全部账号均达到限制时，客户可能等待或收到现有的无可用凭据错误。
- RPM 提高会增加对应账号的上游请求速率，管理员需自行确保不超过账号真实限制。
- 指标读取和 UI 展示是只读操作，不进入客户响应路径。

## 8. 测试与验收

后端测试：

- 批量 RPM 对全部目标生效并只持久化一次。
- 不存在 ID、重复 ID、空补丁和越界 RPM 全部 fail-closed。
- 分组 replace/add/remove 与现有前端语义一致。
- 降低 RPM 后现有窗口保留，选择器立即遵守新上限。
- 汇总正确处理有限、无限、禁用、满载和窗口边界账号。
- `inFlight` 在 guard 获取和释放后准确变化。

前端测试：

- 跨页全选结果完整进入批量请求。
- RPM 开关未开启时不提交字段，`0` 正确提交为不限速。
- 请求失败保留选择，请求成功清空选择并刷新数据。
- 总 RPM 状态条在有限、无限和空账号三种状态下显示正确。
- 320px 宽度无横向溢出，按钮文字和数值不重叠。

完整验证：

```text
cargo test --locked --no-default-features
cargo check --all-targets --locked --no-default-features
bun test
bun run build
git diff --check
```

## 9. 后续独立子项目

本功能完成后再依次设计和实施：

1. 运行时日志级别：管理端临时启用 `kiro_rs=debug`，30/60 分钟自动回到 INFO，不长期开启第三方库 DEBUG。
2. 协议指标：首 Token P50/P95、上游首字节、空响应重试恢复率、工具协议错误率和 5xx 分类。
3. 严格 SSE：保证 ping 不早于 `message_start`，并明确它对“任意字节首包”和真实首 Token 的不同影响。
4. 8991 更新专用凭据后重新运行 Ztest/CCTest，再根据真实报告决定下一轮协议改动。

## 10. 明确不做

- 不批量修改或显示任何 secret/token。
- 不提供任意 Rust `EnvFilter` 输入框。
- 不通过注入检测站提示词提高分数。
- 不把滚动 RPM 窗口改成自然分钟计数。
- 不在本批次部署生产 8990。
