# 任务:登录后自动创建 & 提取 Kiro API Key(ksk_)

## 背景 / 已验证结论(2026-07-16 实测)
门户 `ksk_` API key 可 **headless 创建,零浏览器**。抓包 + 实测确认:
- 鉴权只需 `authorization: Bearer <token>`,cookie / x-csrf-token / x-kiro-* 全部可省(实测去掉仍 HTTP 200)。
- Bearer token 格式 `aoaAAAA...:<DER-ECDSA-sig>`,**正是 device-code / IdC 流拿到的 access_token 格式**。企业号 `enterprise_http.py` 登录后的 `access_token` 直接可用。
- 两步链路(均 `content-type: application/x-amz-json-1.0`,AWS-JSON-1.0):
  1. `POST https://q.{region}.amazonaws.com/` · `x-amz-target: AmazonCodeWhispererService.ListAvailableProfiles` · body `{"maxResults":10}` → `profiles[0].arn` = profileArn。企业号(external_idp)需带 `tokentype: EXTERNAL_IDP`;idc/social 不需要。
  2. `POST https://management.{region}.kiro.dev/` · `x-amz-target: KiroControlPlaneBearerService.CreateApiKey` · body `{"profileArn":"...","label":"账号名"}` → `{"keyId","keyPrefix","rawKey"}`。**rawKey 只在创建时返回一次**。
     - 列举:target `KiroControlPlaneBearerService.ListApiKeys`,body `{"profileArn":"..."}`(不含 rawKey,用于幂等判重)。

## 用户已确认的决策
- **触发**:集成进 batch_login 登录流(登录成功后自动追加建 key)。
- **去向**:两个都要 —— ① 导出成文本清单;② 写回账号库(凭据里存 ksk_)。
- **label**:用账号名(codeflow2-N)。

## 实现步骤

### 1) 新模块 `api_key_client.py`
纯 async,复用现有 `CurlCffiTransport`(Chrome 指纹,与登录同款,避免风控差异)。函数:
- `resolve_profile_arn(transport, *, token, region, token_type) -> str | None`
  打 ListAvailableProfiles;`token_type` 为 `"EXTERNAL_IDP"` 时加 `tokentype` 头;取 `first_arn`。
- `list_api_keys(transport, *, token, region, profile_arn) -> list[dict]`
- `create_api_key(transport, *, token, region, profile_arn, label) -> str`(返回 rawKey)
- `ensure_api_key(transport, *, token, region, profile_arn, label) -> (raw_key|None, profile_arn)`
  编排:profile_arn 缺则先 resolve;先 ListApiKeys 判重(同 label 已存在则跳过——但 raw 不可再取,见下「幂等策略」);否则 create。
- 错误类型 `ApiKeyError(code, stage, retryable, message)`,与现有 EnterpriseHttpError 风格一致,失败不炸整个登录流。

### 2) 凭据模型加字段 `credential_models.py`
- `CredentialRecord` 增 `kiro_api_key: str | None = None`。
- `as_add_request()`:输出 `"kiroApiKey"`(对齐 kiro.rs/admin 的 `authMethod:"api_key"` 契约,见 add-credential-dialog / batch-import-dialog 的 `kiroApiKey` 字段)。
- `from_add_request()`:解析 `kiroApiKey`。

### 3) 集成进登录流 `local_runner.py`
- `LocalBatchRunner._login` 成功拿到 credential 后,若开关开启:调 `ensure_api_key`,把 `raw_key` 写入 `credential.kiro_api_key`,把 resolve 出的 `profile_arn` 回填 `credential.profile_arn`(顺带补上企业号缺失的 profileArn)。
- 开关 + region 从 `LocalRunSettings` 取(新增 `create_api_key: bool`、`api_key_label_mode` 先固定「账号名」)。
- 事件:发 `WorkerEvent("api_key_created", {...})` / 失败 `api_key_failed`,GUI 可展示。建 key 失败**不**判登录失败(凭据仍有效),只在结果里标注。

### 4) transport 装配 `gui_runtime.py`
- 企业号:已有 `CurlCffiTransport`,给 runner 传一个 `transport_factory` 供建 key 用(每账号新 transport,和 IsolatedEnterpriseAuth 一致)。
- 社交号:同样给一个 curl_cffi transport 工厂(建 key 不走浏览器)。
- 把 `create_api_key` 开关从 form 透传。

### 5) 导出文本清单
- 复用 `oidc_exporter` 旁边新增 `api_key_exporter`(或在 coordinator 收尾处):
  输出 `login = <账号> / apikey = ksk_xxx` 每行一条,落到 `oidc_export_directory`(与 OIDC 导出同目录),文件名 `kiro-apikeys-<ts>.txt`。
  未拿到 key 的账号列在文件尾部注释,便于人工补。

### 6) 写回账号库 `account_repository.py`
- credential 走现有 `save_credential`(整条 JSON 加密进 credentials 表,`kiro_api_key` 随 blob 落库,**无需 schema 迁移**)。
- 确认 `load_credential` → `CredentialRecord.from_add_request`/反序列化能带出新字段。

### 7) GUI 开关 `gui_app.py` / `gui_settings.py`
- 加勾选「登录后自动创建 API Key」(默认关,灰度)。
- 加 region 复用现有 region 设置。
- 结果区展示每账号 key 状态(成功/已存在/失败)。

## 幂等策略(重要)
rawKey 只在创建时返回一次。若账号已存在同名 key,ListApiKeys 只能看到 keyPrefix 看不到完整 key。策略:
- 默认**每次都建新 key**(label 用账号名,允许同名多把——门户允许),把最新 rawKey 写库+导出。
- 提供开关「已建过则跳过」:ListApiKeys 命中同 label 时跳过(此时库里若已有 ksk_ 就保留,没有则标注「需手动」)。

## 测试
- 单元:`api_key_client` 用假 transport 断言 target/body/tokentype 头正确、rawKey 解析、错误分类。
- 契约:CredentialRecord round-trip 带 kiroApiKey。
- 端到端(手动/1 个真号 dryRun 关):跑 1 个 codeflow2-N,确认拿到 ksk_、导出文件、库里可读回。
- 现有测试全绿(scripts 下 pytest / 相关 contract)。

## 不做 / 边界
- 不动登录本身逻辑(贴近原版最小改动原则,见 memory feedback_kirogo_align_main_minimal 精神)。
- 不删除已有 key(只读 List + 建 Create,无 Delete)。
- 社交号(Google/M365)同一套代码天然支持,但本次目标是 codeflow2-N 企业号。
