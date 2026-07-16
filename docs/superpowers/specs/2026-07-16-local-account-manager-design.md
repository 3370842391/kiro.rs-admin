# Kiro 本地账号管理与一键登录导出设计

## 目标

把现有批量登录 GUI 升级为以账号表格为中心的本地账号管理器，同时保持企业账号纯 HTTP 自动登录、Microsoft 登录、RS 导入和现有凭据保险机制可用。

用户可以一次粘贴约 100 个账号，确认解析结果后写入本地账号库；在主表中搜索、筛选、勾选、连续选择或拖动选择账号；查看密码、更新密码、标记已售、添加销售备注；并对选中账号执行“一键登录导出 JSON”或导出 `账号----当前密码----Start URL` 文本。

## 分阶段交付

### 阶段一：OIDC 导出与 RS 兼容

- 保留当前完整凭据 JSON 的 `version / generatedAt / credentials` 内部结构。
- 新增独立的 OIDC 精简导出器，不用精简格式替换内部保险文件。
- 支持合并 JSON、逐账号 JSON、两种同时生成。
- 支持从已有完整凭据 JSON 手动转换。
- 修复 RS 前端解析器，使 `{"credentials":[...]}` 也能直接导入。

### 阶段二：本地账号库与管理界面

- 引入本地 SQLite 账号库和迁移逻辑。
- 新增单页大表格、搜索、筛选、多选、密码查看器、密码更新、备注与已售状态。
- 新增粘贴解析预览和确认入库。
- 新增账号密码文本导出与“导出后标记已售出”。

### 阶段三：一键登录与凭据同步

- 让现有登录运行器从勾选的数据库账号创建任务。
- 自动跳过可复用凭据，只登录无凭据、失败或密码更新过的账号。
- 登录、首次改密和凭据保存成功后回写账号库。
- 导出只包含成功或已有有效凭据的账号，并可选继续导入 RS。

每个阶段结束时软件都必须可运行、可测试；后续阶段不能破坏前一阶段的导出兼容性。

## 架构

采用“本地 SQLite 账号库 + 现有登录服务 + 独立导出器”结构：

- `AccountRepository` 负责账号、状态、备注、密码密文和凭据密文的事务保存。
- `PasswordProtector` 复用 Windows DPAPI，只在用户点击查看、复制、导出或登录时短暂解密。
- `AccountSelectionService` 把 GUI 中勾选的账号转换成批量任务，不把 Treeview 状态当作持久化数据。
- `LocalBatchRunner` 继续负责编排企业或 Microsoft 登录，但通过结果回调更新账号库。
- `OidcCredentialExporter` 只负责把凭据投影成 RS/Kiro Account Manager 兼容格式并原子写文件。
- RS 前端导入器兼容内部完整凭据文件、扁平数组、单对象和 Kiro Account Manager `accounts` 格式。

登录协议代码不直接依赖 Tkinter 或 SQLite；GUI 不直接调用企业 HTTP 请求。这样账号存储、登录、导出和界面可以分别测试。

## 数据模型

SQLite 数据库按 schema version 管理，首版包含以下逻辑实体。

### accounts

- `id`：内部整数主键。
- `login_mode`：`enterprise` 或 `microsoft`。
- `account`：登录账号。
- `start_url`：企业门户 URL，Microsoft 可为空。
- `region`：默认 `us-east-1`。
- `initial_password_ciphertext`：粘贴导入的一次性密码，经 DPAPI 加密。
- `current_password_ciphertext`：当前可登录密码，经 DPAPI 加密。
- `login_status`：`pending / running / success / failed`。
- `credential_status`：`missing / valid / stale`。
- `lifecycle_status`：`managed / sold`。
- `note`：销售或内部备注。
- `last_error_code`、`last_error_stage`：脱敏错误信息。
- `last_login_at`、`last_exported_at`、`created_at`、`updated_at`。

唯一键为 `login_mode + account(casefold) + 规范化 start_url`。重复粘贴时执行合并：更新新的输入密码和非空 URL/Region，但不覆盖已有备注、已售状态和已验证的当前密码。

### credentials

- 每个账号最多一条当前凭据。
- 保存 OIDC 所需字段和内部完整字段。
- refresh token、access token、client secret 等敏感字段以整体 JSON 经 DPAPI 加密后保存。
- 手动更新当前密码时将 `credential_status` 改为 `stale`，但保留旧凭据用于审计和失败恢复；它不会进入新的正常导出。

### operation_history

- 记录账号入库、登录成功/失败、自动改密、手动更新密码、导出和销售状态变化。
- 只记录脱敏元数据，不记录明文密码、Token 或 client secret。

## 密码规则

- 同时保留“初始一次性密码”和“当前登录密码”。
- 企业首次改密时必须先生成新密码，DPAPI 加密写入数据库并回读校验成功，然后才发送改密请求。
- 自动登录改密成功后，`current_password` 自动指向新密码。
- 用户在别处改密后，可选择一个或多个账号手动填写最新密码；保存后对应凭据立即标记为 `stale`。
- 密码查看器默认遮罩；点击眼睛仅在当前窗口显示，关闭窗口或切换账号立即清空明文控件。
- 导出账号密码文本默认使用当前密码；当前密码为空时不得静默回退到一次性密码，界面明确列出无法导出的账号。

## 主界面

采用用户选定的“B：单页大表格”。

### 工具栏

- 粘贴并识别
- 全选、反选、取消选择
- 一键登录导出 JSON
- 导出账号密码
- 查看密码
- 更新密码
- 标记已售、恢复管理

### 表格

列包括：勾选、账号、当前密码遮罩、Start URL、登录状态、凭据状态、销售状态、备注、最近更新时间。

选择行为支持：

- 单击勾选列；
- `Ctrl` 添加或取消单个选择；
- `Shift` 连续选择；
- 按住鼠标拖动选择连续行；
- 搜索或切换筛选时，已勾选集合按账号 ID 保留。

搜索匹配账号、Start URL 和备注。状态筛选为：全部、待登录、可导出、登录失败、已售出。除“已售出”筛选外，已售账号默认不显示且不参与批量操作。

### 粘贴入库

沿用可自定义输入模板，支持现有格式和 `账号|密码|URL`。粘贴内容先显示预览，列出有效、重复和格式错误数量；用户确认后才写入数据库。一次确认使用单个事务，解析错误行不会入库。

### 密码查看器

双击账号或点击“查看密码”打开详情窗口，分别显示一次性密码和当前密码，提供单字段复制。窗口同时显示登录状态、凭据状态、备注和最近操作，但不显示 Token 明文。

## 导出设计

### OIDC 精简 JSON

字段与 Kiro Account Manager 的 OIDC 精简导出对齐：

- 固定字段：`email`、小写 `authMethod`、`provider`、`refreshToken`。
- 按非空值输出：`region`、`startUrl`、`clientId`、`clientSecret`、`profileArn`、`tokenEndpoint`、`scopes`、`issuerUrl`。
- 不输出 `accessToken`、`expiresAt`、`priority`、`rpmLimit`、`sourceChannel` 和密码。
- `refreshToken` 缺失或为空时，该账号不能进入 OIDC 导出；错误必须指出账号的脱敏标识。

合并文件为顶层对象数组。逐账号文件也使用单元素对象数组，以完全匹配参考项目输出。文件命名：

- 合并：`kiro-accounts-YYYYMMDD-HHMMSS.oidc.json`。
- 单账号：`NNN-安全化账号名.oidc.json`，重名时追加稳定短哈希。

文件使用同目录临时文件、flush、fsync 和 `os.replace` 原子替换，并尽力限制为当前用户读写。

GUI 支持“合并、逐账号、两者”三种模式和导出目录；配置保存这些非敏感选项。从已有完整凭据 JSON 转换时使用相同导出器。

### 账号密码文本

默认模板为 `{account}----{password}----{start_url}`，导出对话框允许修改模板并实时预览。支持复制到剪贴板或保存 TXT。

导出对话框提供批次备注和“导出成功后标记为已售出”复选框。只有剪贴板写入或文件原子保存成功后，才在单个数据库事务中更新所有选中账号的备注、`lifecycle_status=sold` 和导出时间。失败时不得改变账号状态。

## 一键登录导出 JSON

操作对象为当前勾选且处于 `managed` 状态的账号。默认策略：

- `credential_status=valid` 且密码未更新：复用现有凭据。
- `missing / stale`、上次登录失败或没有凭据：执行自动登录。
- 用户可显式勾选“强制重新登录”覆盖默认策略。

每个账号使用隔离的 HTTP 会话或浏览器上下文。账号成功后立即提交账号密码与凭据数据库事务；失败只更新脱敏失败状态并继续下一个。批次结束后，导出全部成功账号和可复用账号，失败账号不生成伪造或空 refresh token 文件。

若选择导入 RS，顺序必须是：本地数据库可靠保存、OIDC 文件可靠写入、RS 导入。RS 导入失败不回滚本地凭据和文件，但在批次摘要中明确显示。

## RS 导入兼容

`parseImportEntries()` 增加对顶层 `credentials` 数组的显式判断，必须位于把单个嵌套 credentials 对象识别为单账号的分支之前。支持格式：

- 顶层扁平数组；
- 顶层扁平单对象；
- `{ "credentials": [...] }` 内部完整凭据文件；
- `{ "accounts": [...] }` Kiro Account Manager 完整导出；
- 单账号内嵌 `credentials` 对象。

## 错误处理和安全

- GUI 日志、异常、历史记录和文件名不得包含密码、Token、client secret 或完整 Admin Key。
- DPAPI 解密失败时阻止登录、查看和导出该账号，并提示备份/用户环境不匹配。
- 数据库写入使用事务；批量入库、销售状态批量更新不可部分成功。
- 文件导出失败时清理临时文件。
- 已售出账号默认排除登录、改密和导出；只有在“已售出”筛选中明确选择后，用户才能恢复管理或执行单账号操作。
- 删除账号不属于本轮范围，避免误删；只提供标记已售和恢复管理。
- 不上传 GitHub，不把数据库、密码库、导出 JSON、账号 TXT 或可视化草图提交到 Git。

## 迁移与兼容

- 首次打开新版账号管理器时，可显式选择现有完整凭据 JSON 和 `.passwords.sqlite3` 导入。
- 导入使用唯一键去重，不删除或覆盖源文件。
- 没有账号数据库时自动创建；已有未知 schema version 时拒绝写入并提示升级，不自动降级。
- 现有 CLI 和旧 GUI 的批量登录入口继续工作，阶段二只增加新的账号库模式，不移除原文件模式。

## 测试与验收

- OIDC 导出器覆盖合并、逐账号、两者、缺 refresh token、原子替换、文件名安全和敏感信息不泄漏。
- RS 前端解析器覆盖 `credentials` 数组回归用例和全部既有格式。
- SQLite 仓库覆盖建库、事务、唯一键合并、状态保留、DPAPI 失败和 schema version。
- GUI 控制器覆盖粘贴确认、多选集合、筛选后选择保留、密码查看清空、批量状态更新。
- 文本导出覆盖模板、缺当前密码、写入成功后标记已售、失败不改变状态。
- 一键登录覆盖复用有效凭据、刷新 stale、强制登录、失败继续、逐账号会话隔离、保存后导出和 RS 失败不回滚。
- 最终运行 Python 全量单元测试、前端测试/类型检查和 `git diff --check`。

## 非目标

- 不提供云端多用户同步。
- 不自动把已售账号从数据库删除。
- 不实现未经用户提供凭据的找回密码。
- 不把明文密码长期写入日志、配置文件或普通 JSON。
- 不改变企业账号纯 HTTP 登录为浏览器自动化。
