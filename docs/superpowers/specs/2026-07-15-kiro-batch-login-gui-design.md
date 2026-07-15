# Kiro 批量登录桌面助手设计

## 背景

仓库已有 `scripts/kiro_batch_login.py` 和 `scripts/batch_login/`，能够通过 Playwright 自动完成企业账号与 Microsoft 账号登录，但当前实现有三个限制：

1. 输入解析只适合 `{account}----{password}` 一类单分隔符格式，不能直接处理 `login = ... / onetime password = ...`。
2. 登录会话、token 兑换和凭据落库全部依赖 RS；`--result` 生成的只是脱敏 JSONL checkpoint，不是可导入的完整凭据 JSON。
3. 命令行参数、SSH 转发、账号文件转换和结果检查需要用户分别操作，批量使用不方便。

本设计新增一个独立运行的 Tkinter 桌面助手，并把认证流程拆成可复用的本地认证后端。用户可以先在本机验证登录并取得完整凭据 JSON，也可以在 JSON 安全落盘后选择直接连接或通过 SSH 隧道导入 RS。

## 目标

- 粘贴或打开账号文本，按预设或自定义模板解析账号和密码。
- 预览有效项、重复项和错误项，并导出统一账号文本。
- 在本机自动完成企业账号和 Microsoft 账号登录。
- 每个成功账号立即写入包含 token 的完整凭据 JSON。
- 支持“仅保存 JSON”和“保存 JSON 后导入 RS”两种结果模式。
- 支持直接连接 RS，或由界面启动并管理 SSH 本地转发。
- 显示逐账号进度、阶段、错误和人工接管提示，支持安全取消与 checkpoint 恢复。
- 不把账号密码、token、OAuth code、回调 URL或 Admin Key写入普通日志和脱敏 checkpoint。

## 非目标

- 不把账号密码上传给 RS。
- 不在第一版并发运行多个浏览器登录；登录固定串行，避免验证码、回调和会话冲突。
- 不自动绕过 MFA、CAPTCHA、账号锁定或身份提供商的安全检查。
- 不保存 SSH 密码，也不实现 SSH 密码登录。
- 不加密完整凭据 JSON；界面必须明确提示该文件高度敏感，并尽力收紧本地文件权限。
- 不修改 Admin Web UI；桌面助手是独立 Python 程序。
- 不读取、覆盖或迁移仓库中现有的 `accounts.txt`。

## 总体架构

采用 Tkinter 薄界面与本地异步工作器分离的结构：

```text
Tkinter 主线程
  ├─ 输入解析与预览
  ├─ 参数表单、进度表格、日志和按钮状态
  └─ 线程安全事件队列
           │
           ▼
后台线程中的 asyncio 事件循环
  ├─ LocalEnterpriseAuth：AWS IdC 设备授权
  ├─ LocalMicrosoftAuth：Kiro Portal / Microsoft / Entra PKCE
  ├─ BrowserFlows：Playwright 页面自动化与人工接管
  ├─ CredentialStore：完整凭据原子落盘
  ├─ CheckpointStore：脱敏状态与恢复
  ├─ SshTunnel：可选 SSH 本地转发
  └─ RsImportClient：可选批量导入 RS
```

Tkinter 控件只能由主线程操作。后台工作器通过线程安全队列发送结构化事件，主线程用 `root.after(...)` 定时取出事件并更新界面。取消操作通过 `loop.call_soon_threadsafe(task.cancel)` 进入工作器事件循环，不能从 Tkinter 线程直接操作 Playwright 对象。

## 账号解析与统一格式

### 预设规则

内置以下输入模板：

```text
login = {account} / onetime password = {password}
```

统一输出默认使用：

```text
{account}----{password}
```

统一输出模板也可自定义，但同样必须且只能各包含一次 `{account}` 和 `{password}`。输出只做占位符替换，不对密码内容转义或二次解析。

### 自定义模板

- 自定义模板必须且只能各包含一次 `{account}` 和 `{password}`。
- 模板中的其他文字全部作为字面量匹配，用户不需要编写正则表达式。
- 解析器对整行做锚定匹配，避免部分匹配掩盖错误。
- 账号去除首尾空白；密码保留捕获到的原始内容，不以 `/`、`\`、`$`、`<`、`>`、`#` 或空格再次拆分。
- 空行忽略；注释行沿用现有 `#` 行首规则。
- 重复账号按 `casefold()` 判断，预览中高亮，默认只导出和执行第一条。
- 企业账号允许普通用户名；Microsoft 模式要求邮箱格式。

### 预览

预览表格包含行号、账号、遮罩密码、解析状态和错误原因。密码默认遮罩，只有显式开启“显示密码”后才显示。界面提供复制统一格式和另存为 UTF-8 文本文件，不自动读取已有账号文件。

## 本地企业账号认证

企业模式复用当前 RS 的 AWS IAM Identity Center 设备授权语义，但将协议调用移到本地 Python：

1. 根据 `region` 调用 `https://oidc.<region>.amazonaws.com/client/register` 注册公共 OIDC 客户端。
2. 使用 `clientId`、`clientSecret` 和 `startUrl` 调用 `/device_authorization`。
3. 打开 `verificationUriComplete`，或打开 `verificationUri` 后填写 `userCode`。
4. Playwright 在独立 BrowserContext 中填写当前账号和一次性密码。
5. 本地按服务端返回的 interval 轮询 `/token`，处理 `authorization_pending`、`slow_down`、`expired_token` 和 `access_denied`。
6. 成功后组装 RS `AddCredentialRequest` 兼容结构。

企业凭据至少包含：

```json
{
  "email": "原始账号",
  "authMethod": "idc",
  "provider": "Enterprise",
  "refreshToken": "...",
  "accessToken": "...",
  "clientId": "...",
  "clientSecret": "...",
  "startUrl": "https://example.awsapps.com/start",
  "region": "us-east-1",
  "expiresAt": "RFC3339"
}
```

`startUrl` 和 `region` 为企业模式必填项。账号密码不参与 AWS token 请求，只进入本地浏览器页面。

## 本地 Microsoft 认证

Microsoft 模式在本地实现 Kiro Portal PKCE 状态机：

1. 生成第一段 PKCE verifier/challenge 和 state，打开 `https://app.kiro.dev/signin`。
2. Playwright 自动填写 Microsoft 邮箱和密码，并监听浏览器网络请求中的回调 URL。
3. 普通 Microsoft/social 路径捕获根回调的 code/state，向 Kiro social token endpoint 兑换 token。
4. 企业 Entra external IdP 路径先解析 `/signin/callback` 中的 issuer、clientId、scopes 和 loginHint。
5. 只允许 HTTPS 且主机后缀为 `.microsoftonline.com`、`.microsoftonline.us` 或 `.microsoftonline.cn` 的 issuer、authorization endpoint 和 token endpoint。
6. 通过 OIDC discovery 获取第二段端点，生成独立 PKCE/state，打开授权地址并捕获 `/oauth/callback`。
7. 本地向 Entra token endpoint 兑换 access/refresh token，并从 JWT 尽力提取邮箱。

普通 Microsoft 凭据使用 `authMethod: "social"`、`provider: "Microsoft"`；企业 Entra 凭据使用 `authMethod: "external_idp"`、`provider: "Enterprise"`，并保存 `clientId`、`tokenEndpoint`、`issuerUrl` 和 `scopes`。

每个账号使用独立 BrowserContext。默认显示浏览器；无头模式下如检测到 MFA 或 CAPTCHA，任务记录为 `manual_required`，不承诺自动完成。

## 完整凭据 JSON

完整凭据文件采用可直接转换为 RS `BatchImportRequest` 的结构：

```json
{
  "version": 1,
  "generatedAt": "2026-07-15T00:00:00Z",
  "credentials": []
}
```

- `credentials` 中每项使用 RS 的 camelCase `AddCredentialRequest` 字段。
- 不保存登录密码、Admin Key、SSH 私钥内容、OAuth code/state 或完整回调 URL。
- 每成功一个账号，都在目标目录创建同目录临时文件，完整写入并 `fsync` 后用原子替换更新正式文件。
- 如果目标文件已经存在，开始运行前要求用户选择“继续追加并去重”或另存新文件；不得静默覆盖。
- 去重键为认证方式、大小写折叠后的账号及企业 `startUrl`/external IdP `issuerUrl` 组合。
- 文件创建后使用 `os.chmod(..., 0o600)` 尽力限制权限；Windows 无法完全保证 POSIX 权限语义时显示安全警告，但不把敏感内容打印到日志。
- 界面只显示字段存在性和 token 掩码，不展示完整 token；完整查看应由用户自行打开文件。

## 脱敏 checkpoint 与恢复

完整凭据文件与运行 checkpoint 必须分离。checkpoint 继续使用逐行 JSON，但恢复键改为稳定的账号标识，不依赖输入行号：

```text
mode + accountHash + startUrl/issuerUrl
```

企业模式在开始前已知 `startUrl`，因此使用规范化后的 Start URL。Microsoft 模式的 issuer 只有完成第一段 Portal 回调后才能获知，登录前恢复检查使用固定逻辑作用域 `microsoft`；完整凭据文件成功落盘后的去重仍使用实际 `issuerUrl`。这样无需为了判断是否需要登录而提前启动一次登录流程。

记录字段包括批次 ID、原始行号、账号哈希、账号掩码、模式、状态、阶段、时间、错误码、是否可重试和导入结果。不得包含密码或任何 token。

恢复时：

- 已写入完整凭据 JSON 的成功项跳过。
- 可重试失败重新执行。
- `manual_required` 默认重新执行，并要求可见浏览器。
- 取消项重新执行。
- 输入顺序变化不影响恢复匹配。

## RS 导入

结果模式分为：

1. **仅保存完整 JSON**：不显示也不校验 RS、SSH 和 Admin Key 参数。
2. **保存完整 JSON 并导入 RS**：完整 JSON 成功落盘后才允许发起导入。

导入使用现有 `POST /api/admin/credentials/batch-import`，GUI 从完整文件读取 `credentials` 并构造正式请求，默认 `verify: true`。界面显示每项 `verified`、`duplicate` 或 `failed` 事件及最终汇总。导入失败不删除本地凭据；用户可以在同一界面重新导入现有 JSON，而无需重新登录。

Admin Key 默认从 `KIRO_RS_ADMIN_KEY` 环境变量读取，也允许在界面输入。输入框始终遮罩，Key 仅保存在当前进程内，不写入设置、日志或 checkpoint。

## SSH 隧道

RS 连接提供“直接 URL”和“SSH 隧道”两种模式。SSH 模式调用系统 OpenSSH 客户端，不引入 Python SSH 库。

直接模式接受 RS 根地址或带反向代理前缀的地址，并沿用现有客户端规则自动补全 `/api/admin`。远程连接必须使用 HTTPS；HTTP 只允许 `127.0.0.1`、`::1` 或 `localhost`。SSH 模式建立成功后统一使用 `http://127.0.0.1:<local-port>` 访问隧道。

参数包括 SSH 主机、SSH 端口、用户名、可选私钥路径、远端 RS 主机与端口，以及可选本地端口。远端 RS 主机默认 `127.0.0.1`；本地端口留空时自动选择空闲端口。

等价命令结构为：

```text
ssh -N -L 127.0.0.1:<local>:<remote-host>:<remote-port>
    -p <ssh-port>
    -o ExitOnForwardFailure=yes
    -o ServerAliveInterval=30
    -o StrictHostKeyChecking=accept-new
    [-i <identity-file>]
    <user>@<host>
```

- 不传 SSH 密码，不把私钥内容读入程序。
- `accept-new` 只自动接受首次出现的主机；已记录主机密钥变化时必须失败。
- 启动后轮询本地端口并调用 RS `GET /api/admin/credentials` 预检。
- 本地端口发生竞争时，自动重新选择端口并最多重试两次。
- 只终止由当前 GUI 启动的 SSH 子进程；不扫描或关闭用户已有隧道。
- GUI 退出、用户断开或任务取消时关闭所持有的 SSH 进程。

## 界面布局

使用单窗口分区布局：

1. 顶部规则栏：预设/自定义模板、打开文件、转换并预览。
2. 中部双栏：左侧原始文本，右侧账号预览、错误状态和复制/保存按钮。
3. 登录配置区：企业/Microsoft 模式、Start URL、region、可见/无头浏览器、结果方式和凭据 JSON 路径。
4. RS 连接区：直接/SSH 模式、连接参数和 Admin Key；仅在选择导入 RS 时启用。
5. 日志与进度区：当前账号、当前阶段、总体进度和逐项结果。
6. 底部操作区：停止、开始批量登录和状态摘要。

运行时锁定会改变数据含义的输入控件，但保留日志复制、显示/隐藏敏感字段和停止按钮。关闭窗口时若任务仍在运行，先请求取消并等待浏览器、网络会话和 SSH 子进程清理完成。

## 运行事件模型

后台工作器向 GUI 发送以下类型的结构化事件：

- `batch_started(total)`
- `account_started(index, total, accountMasked, mode)`
- `stage_changed(stage, message)`
- `manual_action_required(kind, message)`
- `account_finished(status, code, credentialSaved)`
- `import_event(index, status, credentialId, error)`
- `batch_finished(summary)`
- `batch_cancelled(summary)`
- `fatal_error(code, message)`

事件中只允许出现账号掩码和经过脱敏的错误文本。对第三方响应正文、回调 URL和异常对象统一先脱敏再进入事件队列。

## 错误处理

- 输入错误在启动前全部列出；存在非重复类错误时阻止运行。
- 浏览器导航、页面识别、凭据错误、账号锁定、MFA、CAPTCHA 和超时均作为单账号结果，默认继续下一个账号。
- 协议响应格式错误、token endpoint 安全校验失败和 state 不匹配视为不可重试失败。
- 网络超时、临时 5xx 和浏览器导航失败标记为可重试。
- 完整凭据 JSON 写盘失败是批次级致命错误：立即停止，不得继续登录出更多尚未安全保存的 token。
- RS 导入失败不影响已经保存的凭据，不回滚本地文件。
- 取消当前账号时停止轮询、关闭 BrowserContext，并记录 `cancelled`；此前成功文件保持可用。
- SSH 启动失败、端口占用、主机密钥变化或 RS 预检失败必须给出可执行的错误信息，不输出 Admin Key。

## 测试策略

### 解析与存储单元测试

- 预设模板正确解析用户给出的 `login = ... / onetime password = ...` 格式。
- 密码包含 `/`、`\`、`$`、`<`、`>`、`#`、空格和统一分隔符时保持原样。
- 自定义模板占位符缺失、重复、顺序变化和固定前后缀均有测试。
- 重复账号、空账号、空密码和 Microsoft 非邮箱输入正确分类。
- 完整凭据文件原子写入、追加去重、已有文件冲突和写盘失败均有测试。
- 完整文件与 checkpoint 均断言不包含登录密码；checkpoint 额外断言不包含 token、code、state 和 Admin Key。

### 协议单元测试

- 使用 `httpx.MockTransport` 覆盖 IdC 注册、设备授权、pending、slow_down、成功、过期和拒绝。
- 覆盖 Microsoft social 与 external IdP 两条路径、OIDC discovery、安全后缀校验、state 校验和 token 兑换。
- 验证生成的凭据字段与 RS `AddCredentialRequest` 兼容。
- 覆盖 RS 批量导入 SSE 的 verified、duplicate、failed 和 summary。
- SSH 命令构造、端口重试、预检、取消和进程清理使用假的 subprocess 适配器测试。

### GUI 与集成测试

- GUI 业务状态抽离为可测试控制器，测试控件启用状态、事件消费、取消和恢复摘要；不依赖真实显示器。
- 使用假的认证后端运行多账号批次，验证每成功一个账号就先保存凭据，再发送导入事件。
- 运行现有 `tests/batch_login`，保证原 CLI、浏览器流程、RS 客户端和 checkpoint 行为不被意外破坏。
- 本机烟雾测试包括解析示例文本、启动 GUI、选择仅保存模式、取消假任务和打开结果目录。
- 真实登录验证由用户使用测试账号执行；自动测试不接触真实账号、密码、token、Admin Key 或 `accounts.txt`。

## 验收标准

- 用户可以直接粘贴原始 `login = ... / onetime password = ...` 文本并看到正确预览。
- 企业与 Microsoft 模式都能在不连接 RS 的情况下完成本地认证并生成完整凭据 JSON。
- 生成的凭据可在稍后通过同一 GUI 导入 RS，无需重新登录。
- 选择“保存并导入”时严格遵守“先落盘、后导入”。
- SSH 隧道由 GUI 启动、验证并在退出时清理，且不会影响用户已有 SSH 进程。
- 日志和 checkpoint 中不存在明文密码、token、OAuth code/state 或 Admin Key。
- 中途取消或进程异常后，已经成功保存的凭据仍可使用，恢复不会因账号文件重新排序而重复登录。
- 现有批量登录 CLI 测试保持通过。
