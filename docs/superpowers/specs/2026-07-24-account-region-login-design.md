# 账号 Region 登录修复设计

## 背景

旧式 IAM Identity Center Start URL（例如 `https://d-99674db463.awsapps.com/start`）本身不包含 Region。当前账号导入窗口没有 Region 输入，确认导入时会把账号 Region 默认保存为 `us-east-1`；批量登录创建运行时又只使用全局配置 Region，没有使用账号库中的 Region。因此 `eu-central-1` 门户会被错误地请求到美区端点。

## 目标

- 允许用户在账号导入窗口填写 Region，例如 `eu-central-1`。
- 将填写的 Region 保存到导入的每个账号。
- 批量登录和登录后提取 API Key 时使用账号自身保存的 Region。
- 支持一次选择不同 Region 的账号并发执行。
- 保持现有 `us-east-1` 账号行为不变。

## 设计

### 导入界面

在“粘贴并识别账号”窗口增加 `Region` 输入框。初始值优先读取 GUI 已保存配置中的 Region；没有配置时使用 `us-east-1`。确认导入时对 Region 去除首尾空格并传给账号服务，账号服务继续通过现有仓库接口保存 Region。

Region 不能为空，并限制为小写字母、数字和连字符组成的 AWS Region 形式。无效输入在写入账号库前显示错误，不产生部分导入。

### 登录调度

协调器读取每个账号的 `region`，按 `(login_mode, region)` 分组。每个分组创建独立运行时，并把该组 Region 写入 `GuiFormState`。账号登录、凭据保存以及后续 API Key 请求继续使用凭据中的 Region；这样美区和德国区账号可在同一批任务中运行而不会共享错误端点。

旧数据如果仍保存为 `us-east-1`，用户用相同账号和 Start URL 重新导入并填写 `eu-central-1` 后，现有 upsert 行为会更新 Region，同时保留账号标识及相关管理状态。

### AWS 端点

现有企业 HTTP 登录器已经根据 Region 构造 OIDC、SSO Portal 和 Sign-in 端点，`eu-central-1` 无需新增特殊端点。修复只保证正确 Region 能从界面贯穿到现有端点构造逻辑。

## 错误处理

- Region 为空或格式无效时阻止导入并提示用户。
- 不自动猜测旧式 `d-*.awsapps.com` 门户的 Region，避免网络探测失败或误判。
- 登录失败继续沿用现有脱敏事件与账号失败状态，不记录密码或 token。

## 测试

- 导入界面确认操作会把用户填写的 `eu-central-1` 传给服务。
- Region 默认值来自已保存 GUI 配置，缺省时为 `us-east-1`。
- 协调器对不同 Region 的账号创建不同运行时，并把正确 Region 传入登录器。
- `eu-central-1` 企业登录会构造德国区 OIDC 和 Portal 端点。
- 运行现有批量登录相关回归测试，确保并发和美区登录行为不变。
