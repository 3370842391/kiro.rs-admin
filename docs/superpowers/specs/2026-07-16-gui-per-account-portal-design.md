# GUI 每账号企业门户格式设计

## 目标

批量登录 GUI 支持 `{account}|{password}|{start_url}`。第三列为该账号自己的 AWS 企业门户地址，密码即使包含普通特殊字符或额外的 `|` 也按最右侧 HTTPS URL 之前的完整内容解析。

## 行为

- 预览表新增“企业门户”列。
- 所有有效行使用同一个门户时，自动回填顶部 Start URL。
- 有多个门户时不覆盖顶部 Start URL，运行时逐行使用各自门户。
- 企业模式允许顶部 Start URL 为空，但此时每个账号都必须带 `start_url`。
- 独立企业 CLI 同样优先使用每行门户；只有缺少逐行门户时才使用 `--start-url`。
- `ssoins-*.portal.<region>.app.aws` 自动发现真实 directory 和 signin endpoint；企业登录保持纯 HTTP，不启动浏览器。

## 安全与错误处理

- 逐行 URL 只接受 HTTPS、有效主机名且禁止 URL 内嵌用户名或密码。
- 日志、预览状态和异常不输出密码、Cookie、JWE、CSRF、workflow handle 或 token。
- 某行没有逐行门户且全局门户为空时，只失败该行并继续后续账号。

## 验证

为解析器、运行器、GUI 控制器、GUI 预览辅助逻辑、独立 CLI 和新式 AWS 门户发现流程增加单元测试，再执行完整 batch-login 回归、GUI `--check`、Python 编译和 `git diff --check`。
