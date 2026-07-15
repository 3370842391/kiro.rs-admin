# 企业账号纯 HTTP 自动登录设计

## 目标

企业账号登录不启动浏览器，通过 AWS OIDC、signin 工作流和 SSO 接口取得完整凭据。Microsoft 登录保持现有浏览器流程。

## 协议架构

新增独立的企业 HTTP 协议层，使用 `curl_cffi` 的 Chrome TLS 模拟维护 Cookie，会话顺序为：OIDC 注册与设备码、企业 Portal 初始化、D2C visitor、signin 工作流初始化、用户名、JWE 一次性密码、按 `stepId` 可选改密、SSO token、设备码关联、OIDC token 轮询。

AWS signin 指纹从当前 signin `app.js` 动态提取 XXTEA key、identifier 和版本，生成参考实现同结构的浏览器指纹。密码 JWE 使用 RSA-OAEP-256 与 AES-256-GCM。每一步只接受明确的 HTTP 状态、`stepId` 和必需字段；MFA、验证码或未知步骤明确失败，不回退浏览器。

## 新密码保险库

新密码使用 `secrets` 生成，满足大写、小写、数字、特殊字符并随机打乱。保险库是独立 SQLite 文件，Windows 上密码字段先用当前用户 DPAPI 加密；数据库启用 WAL、`synchronous=FULL` 和 `busy_timeout`。

改密严格执行：

1. 生成候选密码并创建唯一 `operation_id`。
2. 事务写入 `prepared`，提交后重新读取、解密并校验。
3. 任何保存、加密、fsync 或读回失败都终止账号，绝不发送改密请求。
4. 发送 AWS 改密请求。
5. 明确成功更新为 `confirmed`；明确拒绝更新为 `rejected`；请求已发送但响应未知更新为 `uncertain` 并停止账号。
6. `uncertain` 不生成第二个密码。恢复时优先用已保存候选密码验证，再尝试原一次性密码。

保险库与 token 凭据 JSON 分离；普通日志、checkpoint 和事件中不出现密码、JWE、Cookie、CSRF、workflow handle 或 token。

## 集成

GUI 企业模式不再启动 Playwright，固定新密码输入改为只读的“自动生成并保存”说明及保险库路径。运行时仅 Microsoft 模式初始化 Playwright。企业认证返回现有 `CredentialRecord`，因此保存 JSON 和 RS 导入逻辑保持不变。

## 错误处理

- TLS/网络失败发生在改密请求之前：可重试当前账号。
- 改密请求发送后的网络结果未知：标记 `uncertain`，不可盲重试。
- AWS 明确返回密码复杂度错误：标记 `rejected`；允许生成新候选前必须先持久化新记录。
- 未支持的 MFA/CAPTCHA/未知 `stepId`：结构化失败并继续下一账号。
- 保险库不可用：`password_vault_failed`，不发送改密。

## 验证

使用本地 HTTP fixture 和假传输验证完整状态机，不使用真实账号。对 XXTEA、JWE 结构、DPAPI/保险库事务、状态转换、崩溃恢复、GUI 不启动浏览器分别建立单元测试。真实 AWS 首个账号测试由用户本地执行，日志仅输出脱敏阶段与响应分类。
