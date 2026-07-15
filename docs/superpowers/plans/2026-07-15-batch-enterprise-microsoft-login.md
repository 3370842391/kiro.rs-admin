# RS 批量企业与 Microsoft 自动登录实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `kiro.rs-admin` 中补齐适合自动化客户端的登录会话能力，并交付独立 Python/Playwright CLI，从账号密码文件串行完成企业 AWS SSO 与 Microsoft Entra 登录、凭证入库、去重和中断恢复。

**Architecture:** Rust 继续持有 PKCE/OIDC 会话、token 交换、凭证验证和账号池持久化，只增加幂等取消、结构化错误和登录身份去重。Python 以独立 CLI 运行，分层实现格式解析、脱敏 checkpoint、RS Admin API 客户端、Playwright 页面驱动和串行编排；密码只存在于 Python 进程内存与浏览器填写动作中。

**Tech Stack:** Rust 2024、Axum 0.8、Tokio、Serde、parking_lot、React 19 类型定义、Python 3.11+、httpx、Playwright async API、`unittest`、Cargo 内置测试。

---

## 文件结构

- `src/admin/types.rs`：取消响应、登录成功重复标记、兼容旧字段的结构化错误响应。
- `src/admin/error.rs`：带 HTTP 状态、稳定错误码、阶段和 retryable 的认证错误变体。
- `src/admin/service.rs`：会话取消、认证错误映射、登录凭证“新增或返回已有 ID”编排。
- `src/admin/handlers.rs`：IDC/Social 取消 handlers。
- `src/admin/router.rs`：取消路由与鉴权回归测试。
- `src/kiro/token_manager.rs`：基于 refresh token、真实 profile ARN、邮箱和租户范围查找已有登录身份。
- `admin-ui/src/types/api.ts`：把登录成功响应的 `duplicate` 暴露为可选字段，保持现有 UI 行为。
- `scripts/batch_login/models.py`：CLI 领域模型、状态、错误和运行配置。
- `scripts/batch_login/input_parser.py`：格式模板编译、逐行解析、校验与输入去重。
- `scripts/batch_login/redaction.py`：账号、URL query、token/code 和异常文本脱敏。
- `scripts/batch_login/checkpoint.py`：无密码 JSONL checkpoint、恢复决策和退出码。
- `scripts/batch_login/rs_client.py`：异步 RS Admin API 客户端、旧/新错误兼容、回调 URL 解析和重试。
- `scripts/batch_login/browser_flows.py`：Playwright 登录控件定位、企业流程、Microsoft 两段回调和人工接管检测。
- `scripts/batch_login/runner.py`：逐账号串行编排、取消清理、checkpoint 与结果汇总。
- `scripts/batch_login/cli.py`：argparse、环境变量、预检、脱敏预览和退出码。
- `scripts/batch_login/__init__.py`：Python 包边界。
- `scripts/kiro_batch_login.py`：薄入口文件。
- `scripts/requirements-batch-login.txt`：`httpx` 与 `playwright` 依赖范围。
- `tests/batch_login/test_input_parser.py`：格式解析和输入去重测试。
- `tests/batch_login/test_redaction.py`：敏感信息脱敏测试。
- `tests/batch_login/test_checkpoint.py`：checkpoint 和恢复测试。
- `tests/batch_login/test_rs_client.py`：RS 客户端、重试、错误兼容与回调解析测试。
- `tests/batch_login/test_browser_contract.py`：本地模拟页面的 Playwright 合约测试。
- `tests/batch_login/test_runner.py`：不访问真实浏览器/RS 的串行编排测试。
- `README.md`：安装、格式、两种模式、SSH 转发、安全说明和退出码。
- `.gitignore`：忽略本地 batch 登录结果与 Python 缓存。

## Task 1：扩展认证错误与登录响应契约

**Files:**
- Modify: `src/admin/types.rs:1191-1213, 1380-1422, 1720-1750`
- Modify: `src/admin/error.rs:1-85`
- Modify: `admin-ui/src/types/api.ts:398-430`
- Test: `src/admin/types.rs:1720-1750`
- Test: `src/admin/error.rs` 内新增测试模块

- [ ] **Step 1：先写结构化错误保持旧字段的失败测试**

在 `src/admin/types.rs` 现有测试模块加入：

```rust
#[test]
fn structured_admin_error_keeps_legacy_fields_and_adds_auth_metadata() {
    let value = serde_json::to_value(AdminErrorResponse::structured(
        "invalid_request",
        "OAuth state 不匹配",
        "state_mismatch",
        "social_callback",
        false,
    ))
    .unwrap();

    assert_eq!(value["error"]["type"], "invalid_request");
    assert_eq!(value["error"]["message"], "OAuth state 不匹配");
    assert_eq!(value["error"]["code"], "state_mismatch");
    assert_eq!(value["error"]["stage"], "social_callback");
    assert_eq!(value["error"]["retryable"], false);

    let legacy = serde_json::to_value(AdminErrorResponse::invalid_request("bad")).unwrap();
    assert!(legacy["error"].get("code").is_none());
    assert!(legacy["error"].get("stage").is_none());
    assert!(legacy["error"].get("retryable").is_none());
}
```

- [ ] **Step 2：运行测试并确认因 `structured` 不存在而失败**

Run: `cargo test -j 1 admin::types::tests::structured_admin_error_keeps_legacy_fields_and_adds_auth_metadata -- --exact`

Expected: 编译失败，包含 `no function or associated item named structured`。

- [ ] **Step 3：实现兼容旧客户端的可选认证错误字段**

把 `AdminError` 和构造器改为：

```rust
#[derive(Debug, Serialize)]
pub struct AdminError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

impl AdminErrorResponse {
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: AdminError {
                error_type: error_type.into(),
                message: message.into(),
                code: None,
                stage: None,
                retryable: None,
            },
        }
    }

    pub fn structured(
        error_type: impl Into<String>,
        message: impl Into<String>,
        code: impl Into<String>,
        stage: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            error: AdminError {
                error_type: error_type.into(),
                message: message.into(),
                code: Some(code.into()),
                stage: Some(stage.into()),
                retryable: Some(retryable),
            },
        }
    }
}
```

保留现有 `invalid_request`、`authentication_error`、`not_found`、`api_error`、`internal_error` 方法不变。

- [ ] **Step 4：为 `AdminServiceError` 写认证错误映射失败测试**

在 `src/admin/error.rs` 增加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_error_preserves_status_code_and_structured_fields() {
        let error = AdminServiceError::auth(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "state_mismatch",
            "social_callback",
            false,
            "OAuth state 不匹配",
        );
        assert_eq!(error.status_code(), StatusCode::BAD_REQUEST);

        let value = serde_json::to_value(error.into_response()).unwrap();
        assert_eq!(value["error"]["type"], "invalid_request");
        assert_eq!(value["error"]["code"], "state_mismatch");
        assert_eq!(value["error"]["stage"], "social_callback");
        assert_eq!(value["error"]["retryable"], false);
    }
}
```

- [ ] **Step 5：运行测试并确认因 `Auth` 变体和构造器不存在而失败**

Run: `cargo test -j 1 admin::error::tests::auth_error_preserves_status_code_and_structured_fields -- --exact`

Expected: 编译失败，包含 `no variant or associated item named auth` 或 `Auth` 未定义。

- [ ] **Step 6：实现认证专用错误变体**

在 `AdminServiceError` 增加：

```rust
Auth {
    status: StatusCode,
    error_type: &'static str,
    code: &'static str,
    stage: &'static str,
    retryable: bool,
    message: String,
},
```

在 `Display`、`status_code` 和 `into_response` 中加入对应分支，并新增：

```rust
pub fn auth(
    status: StatusCode,
    error_type: &'static str,
    code: &'static str,
    stage: &'static str,
    retryable: bool,
    message: impl Into<String>,
) -> Self {
    Self::Auth {
        status,
        error_type,
        code,
        stage,
        retryable,
        message: message.into(),
    }
}
```

`into_response` 的新分支必须调用 `AdminErrorResponse::structured`。

- [ ] **Step 7：给登录成功响应增加可选 `duplicate`**

先把现有序列化测试扩展为：

```rust
#[test]
fn poll_success_uses_camel_case_and_omits_false_duplicate() {
    let added = serde_json::to_value(PollIdcLoginResponse::Success {
        credential_id: 7,
        duplicate: false,
    })
    .unwrap();
    assert_eq!(added["credentialId"], 7);
    assert!(added.get("duplicate").is_none());

    let existing = serde_json::to_value(PollIdcLoginResponse::Success {
        credential_id: 7,
        duplicate: true,
    })
    .unwrap();
    assert_eq!(existing["duplicate"], true);
}
```

然后增加：

```rust
fn is_false(value: &bool) -> bool {
    !*value
}

Success {
    credential_id: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    duplicate: bool,
},
```

同步把 `service.rs` 中全部七个 `PollIdcLoginResponse::Success` 构造点显式设为 `duplicate: false`。在 `admin-ui/src/types/api.ts` 把 success 分支改为：

```typescript
| { status: 'success'; credentialId: number; duplicate?: boolean }
```

- [ ] **Step 8：运行契约测试与编译检查**

Run: `cargo test -j 1 admin::types::tests:: -- --nocapture`

Expected: `admin::types::tests` 全部通过。

Run: `cargo test -j 1 admin::error::tests:: -- --nocapture`

Expected: `admin::error::tests` 全部通过。

Run: `cd admin-ui; bun run build`

Expected: TypeScript 和 Vite 构建成功。

- [ ] **Step 9：提交本地变更**

```powershell
git add src/admin/types.rs src/admin/error.rs admin-ui/src/types/api.ts
git commit -m "feat(auth): 扩展自动登录响应契约"
```

## Task 2：增加 IDC/Social 幂等取消接口

**Files:**
- Modify: `src/admin/types.rs:1215-1280`
- Modify: `src/admin/service.rs:521-600, 3683-4340`
- Modify: `src/admin/handlers.rs:846-915`
- Modify: `src/admin/router.rs:8-30, 177-190, 260-390`
- Test: `src/admin/service.rs:4512-end`
- Test: `src/admin/router.rs:260-end`

- [ ] **Step 1：写取消响应与服务方法失败测试**

在 `src/admin/service.rs` 测试模块增加测试服务构造器和 IDC 取消测试：

```rust
fn auth_test_service() -> AdminService {
    let manager = Arc::new(
        MultiTokenManager::new(Config::default(), Vec::new(), None, None, true).unwrap(),
    );
    AdminService::new(
        manager,
        Vec::<String>::new(),
        Arc::new(ProxyPoolManager::new(None, crate::model::config::TlsBackend::Rustls)),
    )
}

#[test]
fn cancel_idc_login_is_idempotent() {
    let service = auth_test_service();
    service.idc_sessions.lock().insert(
        "session-1".to_string(),
        IdcAuthSession {
            region: "us-east-1".to_string(),
            client_id: "client".to_string(),
            client_secret: "secret".to_string(),
            device_code: "device".to_string(),
            expires_at: Utc::now() + Duration::minutes(5),
            poll_interval: 5,
            cred_template: KiroCredentials::default(),
            proxy: None,
            relogin_target_id: None,
        },
    );

    assert!(service.cancel_idc_login("session-1").cancelled);
    assert!(!service.cancel_idc_login("session-1").cancelled);
}
```

- [ ] **Step 2：运行测试并确认方法不存在**

Run: `cargo test -j 1 admin::service::tests::cancel_idc_login_is_idempotent -- --exact`

Expected: 编译失败，包含 `no method named cancel_idc_login`。

- [ ] **Step 3：实现取消响应和两个服务方法**

在 `src/admin/types.rs` 增加：

```rust
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelLoginResponse {
    pub cancelled: bool,
}
```

在 `AdminService` 增加：

```rust
pub fn cancel_idc_login(&self, session_id: &str) -> CancelLoginResponse {
    CancelLoginResponse {
        cancelled: self.idc_sessions.lock().remove(session_id).is_some(),
    }
}

pub fn cancel_social_login(&self, session_id: &str) -> CancelLoginResponse {
    CancelLoginResponse {
        cancelled: self.social_sessions.lock().remove(session_id).is_some(),
    }
}
```

Social session 被移除时 `_server_handle` 随结构体 Drop，自动停止 callback server。

- [ ] **Step 4：写 Social 句柄释放测试**

在同一测试模块加入：

```rust
#[test]
fn cancel_social_login_removes_session_and_drops_server_handle() {
    let service = auth_test_service();
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let (_port, handle) = social::start_callback_server(tx).unwrap();
    let (_callback_tx, callback_rx) = tokio::sync::mpsc::channel(1);
    service.social_sessions.lock().insert(
        "social-1".to_string(),
        SocialAuthSession {
            auth_endpoint: social::KIRO_AUTH_ENDPOINT.to_string(),
            state: "state".to_string(),
            code_verifier: "verifier".to_string(),
            redirect_uri: "http://127.0.0.1:1".to_string(),
            expires_at: Utc::now() + Duration::minutes(5),
            callback_rx: tokio::sync::Mutex::new(callback_rx),
            external_idp: None,
            cred_template: KiroCredentials::default(),
            proxy: None,
            _server_handle: handle,
            relogin_target_id: None,
        },
    );

    assert!(service.cancel_social_login("social-1").cancelled);
    assert!(!service.social_sessions.lock().contains_key("social-1"));
}
```

- [ ] **Step 5：实现 handlers 和 DELETE 路由**

在 `handlers.rs` 增加：

```rust
pub async fn cancel_idc_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    Json(state.service.cancel_idc_login(&session_id))
}

pub async fn cancel_social_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    Json(state.service.cancel_social_login(&session_id))
}
```

在 `router.rs` 导入 handlers 并加入：

```rust
.route("/auth/idc/{session_id}", delete(cancel_idc_login))
.route("/auth/social/{session_id}", delete(cancel_social_login))
```

- [ ] **Step 6：写取消路由鉴权测试**

在 `src/admin/router.rs` 测试模块加入：

```rust
#[tokio::test]
async fn cancel_auth_sessions_require_admin_key_and_are_idempotent() {
    for path in ["/auth/idc/missing", "/auth/social/missing"] {
        let unauthorized = batch_update_test_router()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let authorized = batch_update_test_router()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(path)
                    .header("x-api-key", "test-admin-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
        let body = to_bytes(authorized.into_body(), usize::MAX).await.unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).unwrap()["cancelled"], false);
    }
}
```

- [ ] **Step 7：运行服务和路由测试**

Run: `cargo test -j 1 cancel_ -- --nocapture`

Expected: 三组测试通过。

- [ ] **Step 8：提交本地变更**

```powershell
git add src/admin/types.rs src/admin/service.rs src/admin/handlers.rs src/admin/router.rs
git commit -m "feat(auth): 增加登录会话取消接口"
```

## Task 3：登录身份去重并返回已有凭证 ID

**Files:**
- Modify: `src/kiro/token_manager.rs:3311-3505, tests module`
- Modify: `src/admin/service.rs:3683-4340`
- Test: `src/kiro/token_manager.rs` 测试模块
- Test: `src/admin/service.rs` 测试模块

- [ ] **Step 1：写登录身份匹配失败测试**

在 `src/kiro/token_manager.rs` 测试模块加入：

```rust
#[test]
fn find_existing_login_credential_respects_tenant_scope() {
    let mut existing = KiroCredentials::default();
    existing.id = Some(9);
    existing.auth_method = Some("external_idp".to_string());
    existing.email = Some("User@Example.com".to_string());
    existing.issuer_url = Some("https://login.microsoftonline.com/tenant-a/v2.0".to_string());
    existing.refresh_token = Some("existing-refresh-token-value".repeat(5));

    let manager = MultiTokenManager::new(
        Config::default(),
        vec![existing],
        None,
        None,
        false,
    )
    .unwrap();

    let mut same = KiroCredentials::default();
    same.auth_method = Some("m365".to_string());
    same.email = Some("user@example.com".to_string());
    same.issuer_url = Some("https://login.microsoftonline.com/tenant-a/v2.0/".to_string());
    same.refresh_token = Some("new-refresh-token-value".repeat(5));
    assert_eq!(manager.find_existing_login_credential_id(&same), Some(9));

    same.issuer_url = Some("https://login.microsoftonline.com/tenant-b/v2.0".to_string());
    assert_eq!(manager.find_existing_login_credential_id(&same), None);
}

#[test]
fn find_existing_login_credential_prefers_real_profile_arn() {
    let mut existing = KiroCredentials::default();
    existing.id = Some(4);
    existing.auth_method = Some("social".to_string());
    existing.profile_arn = Some("arn:aws:codewhisperer:us-east-1:1:profile/real".to_string());
    existing.refresh_token = Some("first-refresh-token-value".repeat(5));

    let manager = MultiTokenManager::new(Config::default(), vec![existing], None, None, false).unwrap();
    let mut candidate = KiroCredentials::default();
    candidate.auth_method = Some("social".to_string());
    candidate.profile_arn = Some("arn:aws:codewhisperer:us-east-1:1:profile/real".to_string());
    candidate.refresh_token = Some("second-refresh-token-value".repeat(5));

    assert_eq!(manager.find_existing_login_credential_id(&candidate), Some(4));
}

#[test]
fn find_existing_login_credential_ignores_shared_social_profile_arn() {
    let mut existing = KiroCredentials::default();
    existing.id = Some(2);
    existing.auth_method = Some("social".to_string());
    existing.profile_arn = Some(crate::kiro::model::credentials::SOCIAL_PROFILE_ARN.to_string());
    existing.email = Some("first@example.com".to_string());
    existing.refresh_token = Some("first-social-refresh-token".repeat(5));
    let manager = MultiTokenManager::new(Config::default(), vec![existing], None, None, false).unwrap();

    let mut candidate = KiroCredentials::default();
    candidate.auth_method = Some("social".to_string());
    candidate.profile_arn = Some(crate::kiro::model::credentials::SOCIAL_PROFILE_ARN.to_string());
    candidate.email = Some("second@example.com".to_string());
    candidate.refresh_token = Some("second-social-refresh-token".repeat(5));

    assert_eq!(manager.find_existing_login_credential_id(&candidate), None);
}

#[test]
fn find_existing_social_login_matches_email_only_with_same_provider() {
    let mut existing = KiroCredentials::default();
    existing.id = Some(6);
    existing.auth_method = Some("social".to_string());
    existing.provider = Some("Microsoft".to_string());
    existing.email = Some("user@example.com".to_string());
    existing.refresh_token = Some("first-provider-refresh-token".repeat(5));
    let manager = MultiTokenManager::new(Config::default(), vec![existing], None, None, false).unwrap();

    let mut candidate = KiroCredentials::default();
    candidate.auth_method = Some("social".to_string());
    candidate.provider = Some("microsoft".to_string());
    candidate.email = Some("USER@example.com".to_string());
    candidate.refresh_token = Some("second-provider-refresh-token".repeat(5));
    assert_eq!(manager.find_existing_login_credential_id(&candidate), Some(6));

    candidate.provider = Some("github".to_string());
    assert_eq!(manager.find_existing_login_credential_id(&candidate), None);
}
```

- [ ] **Step 2：运行测试并确认查找方法不存在**

Run: `cargo test -j 1 find_existing_login_credential_ -- --nocapture`

Expected: 编译失败，包含 `no method named find_existing_login_credential_id`。

- [ ] **Step 3：实现登录身份匹配器**

在 `MultiTokenManager` 中增加：

```rust
pub fn find_existing_login_credential_id(&self, candidate: &KiroCredentials) -> Option<u64> {
    use crate::kiro::model::credentials::{
        canonicalize_auth_method_value, is_placeholder_profile_arn, SOCIAL_PROFILE_ARN,
    };

    fn normalized(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.trim_end_matches('/').to_ascii_lowercase())
    }

    let candidate_method = candidate
        .auth_method
        .as_deref()
        .map(canonicalize_auth_method_value)
        .unwrap_or("idc");
    let candidate_profile = candidate
        .profile_arn
        .as_deref()
        .filter(|arn| !is_placeholder_profile_arn(arn) && *arn != SOCIAL_PROFILE_ARN);
    let candidate_email = normalized(candidate.email.as_deref());
    let candidate_scope = if candidate_method == "external_idp" {
        normalized(candidate.issuer_url.as_deref())
    } else if candidate_method == "idc" {
        normalized(candidate.start_url.as_deref())
    } else if candidate_method == "social" {
        normalized(candidate.provider.as_deref()).filter(|provider| provider != "social")
    } else {
        None
    };
    let candidate_refresh_hash = candidate.refresh_token.as_deref().map(sha256_hex);

    self.entries.lock().iter().find_map(|entry| {
        let existing = &entry.credentials;
        let existing_method = existing
            .auth_method
            .as_deref()
            .map(canonicalize_auth_method_value)
            .unwrap_or("idc");
        if existing_method != candidate_method {
            return None;
        }

        if let (Some(left), Some(right)) = (
            candidate_profile,
            existing
                .profile_arn
                .as_deref()
                .filter(|arn| !is_placeholder_profile_arn(arn) && *arn != SOCIAL_PROFILE_ARN),
        ) && left == right
        {
            return Some(entry.id);
        }

        let existing_refresh_hash = existing.refresh_token.as_deref().map(sha256_hex);
        if candidate_refresh_hash.is_some() && candidate_refresh_hash == existing_refresh_hash {
            return Some(entry.id);
        }

        let existing_email = normalized(existing.email.as_deref());
        let existing_scope = if existing_method == "external_idp" {
            normalized(existing.issuer_url.as_deref())
        } else if existing_method == "idc" {
            normalized(existing.start_url.as_deref())
        } else if existing_method == "social" {
            normalized(existing.provider.as_deref()).filter(|provider| provider != "social")
        } else {
            None
        };
        (candidate_email.is_some()
            && candidate_email == existing_email
            && candidate_scope.is_some()
            && candidate_scope == existing_scope)
            .then_some(entry.id)
    })
}
```

Social 登录若没有真实 profile ARN 和租户范围，不使用邮箱单字段去重。

- [ ] **Step 4：写 Admin 登录新增/已有结果测试**

在 `src/admin/service.rs` 测试模块加入纯 helper 测试：

```rust
#[test]
fn login_credential_result_maps_duplicate_flag() {
    assert_eq!(LoginCredentialResult::Added(3).response(), PollIdcLoginResponse::Success {
        credential_id: 3,
        duplicate: false,
    });
    assert_eq!(LoginCredentialResult::Existing(7).response(), PollIdcLoginResponse::Success {
        credential_id: 7,
        duplicate: true,
    });
}
```

为便于断言，给 `PollIdcLoginResponse` 增加 `PartialEq, Eq` derive。

- [ ] **Step 5：实现登录专用“新增或已有”helper**

在 `service.rs` 增加：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginCredentialResult {
    Added(u64),
    Existing(u64),
}

impl LoginCredentialResult {
    fn response(self) -> PollIdcLoginResponse {
        match self {
            Self::Added(credential_id) => PollIdcLoginResponse::Success {
                credential_id,
                duplicate: false,
            },
            Self::Existing(credential_id) => PollIdcLoginResponse::Success {
                credential_id,
                duplicate: true,
            },
        }
    }
}

impl AdminService {
    async fn add_login_credential(
        &self,
        credential: KiroCredentials,
    ) -> Result<LoginCredentialResult, AdminServiceError> {
        if let Some(id) = self
            .token_manager
            .find_existing_login_credential_id(&credential)
        {
            return Ok(LoginCredentialResult::Existing(id));
        }

        let retry_candidate = credential.clone();
        match self.token_manager.add_credential(credential).await {
            Ok(id) => Ok(LoginCredentialResult::Added(id)),
            Err(error) => {
                if let Some(id) = self
                    .token_manager
                    .find_existing_login_credential_id(&retry_candidate)
                {
                    return Ok(LoginCredentialResult::Existing(id));
                }
                Err(self.classify_add_error(error))
            }
        }
    }
}
```

- [ ] **Step 6：把三条新登录入库路径切到 helper**

在 Social、External IdP 和 IDC 的“新建凭据”分支中：

```rust
let result = self.add_login_credential(new_cred).await?;
if let LoginCredentialResult::Added(credential_id) = result {
    if let Err(error) = self.get_balance(credential_id).await {
        tracing::warn!("登录后刷新余额失败（不影响登录）: {}", error);
    }
}
Ok(result.response())
```

Social code 分支在调用 helper 前还要记录提供商：

```rust
if !callback.login_option.trim().is_empty() {
    new_cred.provider = Some(callback.login_option.trim().to_string());
}
```

重新登录已有凭据的分支保持原行为，只把成功响应补成 `duplicate: false`。

- [ ] **Step 7：把认证分支错误改为稳定码**

替换 Social/External IdP 分支中所有 state mismatch、callback missing 和 token exchange 错误，分别使用以下三个构造：

```rust
AdminServiceError::auth(
    StatusCode::BAD_REQUEST,
    "invalid_request",
    "state_mismatch",
    "social_callback",
    false,
    "OAuth state 不匹配，请重新发起登录",
)
```

```rust
AdminServiceError::auth(
    StatusCode::BAD_REQUEST,
    "invalid_request",
    "callback_invalid",
    "social_callback",
    false,
    "OAuth 回调缺少 code",
)
```

```rust
AdminServiceError::auth(
    StatusCode::BAD_GATEWAY,
    "api_error",
    "upstream_error",
    "token_exchange",
    true,
    error.to_string(),
)
```

state 不匹配日志只记录错误码和 session ID 的前 8 位，不记录 expected/actual state、code 或 token。

- [ ] **Step 8：运行去重和登录契约测试**

Run: `cargo test -j 1 find_existing_login_credential_ -- --nocapture`

Expected: 登录身份匹配测试通过。

Run: `cargo test -j 1 login_credential_result_maps_duplicate_flag -- --exact`

Expected: 新增/已有响应映射测试通过。

Run: `cargo test -j 1 poll_success_ -- --nocapture`

Expected: 登录成功 JSON 契约测试通过。

- [ ] **Step 9：运行完整 Rust 测试并提交**

Run: `cargo test -j 1`

Expected: 所有 Rust 测试通过；现有编译 warning 可记录，但不得新增 warning。

```powershell
git add src/kiro/token_manager.rs src/admin/service.rs src/admin/types.rs
git commit -m "feat(auth): 登录重复时返回已有凭证"
```

## Task 4：建立 Python 模型、脱敏和输入解析

**Files:**
- Create: `scripts/batch_login/__init__.py`
- Create: `scripts/batch_login/models.py`
- Create: `scripts/batch_login/redaction.py`
- Create: `scripts/batch_login/input_parser.py`
- Create: `tests/batch_login/__init__.py`
- Create: `tests/batch_login/test_input_parser.py`
- Create: `tests/batch_login/test_redaction.py`

- [ ] **Step 1：先写格式解析失败测试**

创建 `tests/batch_login/test_input_parser.py`：

```python
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.input_parser import parse_accounts
from batch_login.models import LoginMode


class InputParserTests(unittest.TestCase):
    def test_default_format_splits_once_and_preserves_password_separator(self):
        result = parse_accounts(
            "user@example.com----abc----123\n",
            "{account}----{password}",
            LoginMode.MICROSOFT,
        )
        self.assertEqual([], result.issues)
        self.assertEqual("user@example.com", result.entries[0].account)
        self.assertEqual("abc----123", result.entries[0].password)

    def test_password_first_uses_last_separator(self):
        result = parse_accounts(
            "abc----123####user@example.com\n",
            "{password}####{account}",
            LoginMode.MICROSOFT,
        )
        self.assertEqual("abc----123", result.entries[0].password)

    def test_bom_comments_validation_and_duplicates_are_reported_before_browser(self):
        result = parse_accounts(
            "\ufeff# comment\nnot-an-email----pw\nUSER@example.com----one\nuser@example.com----two\n",
            "{account}----{password}",
            LoginMode.MICROSOFT,
        )
        self.assertEqual(["invalid_account", "duplicate_input"], [issue.code for issue in result.issues])
        self.assertEqual(1, len(result.entries))

    def test_template_requires_each_placeholder_once_and_nonempty_separator(self):
        for template in ["{account}", "{account}{password}", "{account}|{account}|{password}"]:
            with self.subTest(template=template):
                with self.assertRaises(ValueError):
                    parse_accounts("a|b", template, LoginMode.ENTERPRISE)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2：运行测试并确认模块不存在**

Run: `python -m unittest tests.batch_login.test_input_parser -v`

Expected: 导入失败，包含 `No module named 'batch_login'`。

- [ ] **Step 3：实现领域模型**

创建 `scripts/batch_login/models.py`：

```python
from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from hashlib import sha256
from typing import Any


class LoginMode(str, Enum):
    ENTERPRISE = "enterprise"
    MICROSOFT = "microsoft"


class ResultStatus(str, Enum):
    SUCCESS = "success"
    DUPLICATE = "duplicate_credential"
    FAILED = "failed"
    MANUAL_REQUIRED = "manual_required"
    CANCELLED = "cancelled"


@dataclass(slots=True)
class AccountEntry:
    line_number: int
    account: str
    password: str = field(repr=False)

    @property
    def account_hash(self) -> str:
        return sha256(self.account.casefold().encode("utf-8")).hexdigest()


@dataclass(slots=True, frozen=True)
class ParseIssue:
    line_number: int
    code: str
    message: str


@dataclass(slots=True)
class ParseResult:
    entries: list[AccountEntry]
    issues: list[ParseIssue]


@dataclass(slots=True)
class LoginOutcome:
    status: ResultStatus
    credential_id: int | None = None
    code: str | None = None
    stage: str | None = None
    retryable: bool = False
    message: str | None = None
    duplicate: bool = False


@dataclass(slots=True)
class RunRecord:
    run_id: str
    line_number: int
    account_hash: str
    account_masked: str
    mode: LoginMode
    status: ResultStatus
    stage: str
    attempts: int
    timestamp: str
    credential_id: int | None = None
    code: str | None = None
    retryable: bool = False
    message: str | None = None

    def as_json(self) -> dict[str, Any]:
        return {
            "runId": self.run_id,
            "lineNumber": self.line_number,
            "accountHash": self.account_hash,
            "accountMasked": self.account_masked,
            "mode": self.mode.value,
            "status": self.status.value,
            "stage": self.stage,
            "attempts": self.attempts,
            "timestamp": self.timestamp,
            "credentialId": self.credential_id,
            "code": self.code,
            "retryable": self.retryable,
            "message": self.message,
        }
```

- [ ] **Step 4：实现格式模板解析**

创建 `scripts/batch_login/input_parser.py`：

```python
from __future__ import annotations

import re
from dataclasses import dataclass

from .models import AccountEntry, LoginMode, ParseIssue, ParseResult


EMAIL_RE = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")


@dataclass(frozen=True, slots=True)
class CompiledFormat:
    separator: str
    account_first: bool


def compile_format(template: str) -> CompiledFormat:
    if template.count("{account}") != 1 or template.count("{password}") != 1:
        raise ValueError("格式模板必须恰好包含一次 {account} 和一次 {password}")
    account_index = template.index("{account}")
    password_index = template.index("{password}")
    if account_index < password_index:
        separator = template[account_index + len("{account}") : password_index]
        prefix = template[:account_index]
        suffix = template[password_index + len("{password}") :]
        account_first = True
    else:
        separator = template[password_index + len("{password}") : account_index]
        prefix = template[:password_index]
        suffix = template[account_index + len("{account}") :]
        account_first = False
    if prefix or suffix or not separator:
        raise ValueError("第一版格式模板只允许两个占位符和一个非空字面分隔符")
    return CompiledFormat(separator=separator, account_first=account_first)


def parse_accounts(text: str, template: str, mode: LoginMode) -> ParseResult:
    compiled = compile_format(template)
    entries: list[AccountEntry] = []
    issues: list[ParseIssue] = []
    seen: set[str] = set()

    for line_number, raw_line in enumerate(text.splitlines(), start=1):
        line = raw_line.lstrip("\ufeff") if line_number == 1 else raw_line
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if compiled.separator not in line:
            issues.append(ParseIssue(line_number, "format_mismatch", "缺少格式分隔符"))
            continue
        if compiled.account_first:
            account, password = line.split(compiled.separator, 1)
        else:
            password, account = line.rsplit(compiled.separator, 1)
        account = account.strip()
        if not account:
            issues.append(ParseIssue(line_number, "empty_account", "账号为空"))
            continue
        if password == "":
            issues.append(ParseIssue(line_number, "empty_password", "密码为空"))
            continue
        if mode is LoginMode.MICROSOFT and not EMAIL_RE.fullmatch(account):
            issues.append(ParseIssue(line_number, "invalid_account", "Microsoft 模式要求邮箱账号"))
            continue
        key = account.casefold()
        if key in seen:
            issues.append(ParseIssue(line_number, "duplicate_input", "输入中账号重复"))
            continue
        seen.add(key)
        entries.append(AccountEntry(line_number=line_number, account=account, password=password))
    return ParseResult(entries=entries, issues=issues)
```

- [ ] **Step 5：写脱敏失败测试**

创建 `tests/batch_login/test_redaction.py`：

```python
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.redaction import mask_account, redact_text, redact_url


class RedactionTests(unittest.TestCase):
    def test_masks_email_and_plain_username(self):
        self.assertEqual("us***@example.com", mask_account("user@example.com"))
        self.assertEqual("ad***", mask_account("admin"))

    def test_redacts_callback_query_and_bearer_tokens(self):
        url = "http://localhost/oauth/callback?code=secret-code&state=secret-state&login_hint=user@example.com"
        self.assertNotIn("secret-code", redact_url(url))
        self.assertNotIn("secret-state", redact_url(url))
        self.assertNotIn("Bearer abc.def.ghi", redact_text("Bearer abc.def.ghi"))
        message = redact_text(f"登录失败 {url} for user@example.com")
        self.assertNotIn("secret-code", message)
        self.assertNotIn("user@example.com", message)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 6：实现脱敏模块**

创建 `scripts/batch_login/redaction.py`：

```python
from __future__ import annotations

import re
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit


SENSITIVE_QUERY_KEYS = {
    "code", "state", "access_token", "refresh_token", "id_token",
    "client_secret", "code_verifier", "password",
}
BEARER_RE = re.compile(r"(?i)\bBearer\s+[^\s,;]+")
TOKEN_ASSIGNMENT_RE = re.compile(
    r"(?i)\b(access_token|refresh_token|id_token|client_secret|password)\s*[:=]\s*[^\s,;]+"
)
URL_RE = re.compile(r"https?://[^\s]+", re.I)
EMAIL_RE = re.compile(r"[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}", re.I)


def mask_account(account: str) -> str:
    if "@" in account:
        local, domain = account.split("@", 1)
        return f"{local[:2]}***@{domain}" if local else f"***@{domain}"
    return f"{account[:2]}***" if account else "***"


def redact_url(raw_url: str) -> str:
    parts = urlsplit(raw_url)
    query = [
        (key, "<redacted>" if key.casefold() in SENSITIVE_QUERY_KEYS else value)
        for key, value in parse_qsl(parts.query, keep_blank_values=True)
    ]
    return urlunsplit((parts.scheme, parts.netloc, parts.path, urlencode(query), ""))


def redact_text(text: str) -> str:
    text = URL_RE.sub(lambda match: redact_url(match.group(0)), text)
    text = EMAIL_RE.sub(lambda match: mask_account(match.group(0)), text)
    text = BEARER_RE.sub("Bearer <redacted>", text)
    return TOKEN_ASSIGNMENT_RE.sub(lambda match: f"{match.group(1)}=<redacted>", text)
```

- [ ] **Step 7：运行 Python 纯逻辑测试并提交**

Run: `python -m unittest tests.batch_login.test_input_parser tests.batch_login.test_redaction -v`

Expected: 所有测试通过。

```powershell
git add scripts/batch_login tests/batch_login
git commit -m "feat(batch-login): 增加账号解析与脱敏"
```

## Task 5：实现无密码 checkpoint 与恢复

**Files:**
- Create: `scripts/batch_login/checkpoint.py`
- Create: `tests/batch_login/test_checkpoint.py`

- [ ] **Step 1：写 checkpoint 内容和恢复失败测试**

创建 `tests/batch_login/test_checkpoint.py`：

```python
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.checkpoint import CheckpointStore, exit_code_for
from batch_login.models import AccountEntry, LoginMode, ResultStatus, RunRecord


class CheckpointTests(unittest.TestCase):
    def test_append_never_serializes_password_and_fsyncs_readable_jsonl(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "result.jsonl"
            store = CheckpointStore(path)
            record = RunRecord(
                run_id="run-1", line_number=3, account_hash="hash",
                account_masked="us***@example.com", mode=LoginMode.MICROSOFT,
                status=ResultStatus.SUCCESS, stage="done", attempts=1,
                timestamp="2026-07-15T00:00:00Z", credential_id=12,
            )
            store.append(record)
            raw = path.read_text(encoding="utf-8")
            self.assertNotIn("password", raw.casefold())
            self.assertEqual(12, json.loads(raw)["credentialId"])

    def test_resume_skips_success_and_retries_retryable_failure_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "result.jsonl"
            path.write_text(
                '\n'.join([
                    json.dumps({"lineNumber": 1, "accountHash": "a", "mode": "enterprise", "status": "success", "retryable": False}),
                    json.dumps({"lineNumber": 2, "accountHash": "b", "mode": "enterprise", "status": "failed", "retryable": True}),
                    json.dumps({"lineNumber": 3, "accountHash": "c", "mode": "enterprise", "status": "failed", "retryable": False}),
                ]) + '\n',
                encoding="utf-8",
            )
            store = CheckpointStore(path)
            self.assertFalse(store.should_run(1, "a", LoginMode.ENTERPRISE, resume=True))
            self.assertTrue(store.should_run(2, "b", LoginMode.ENTERPRISE, resume=True))
            self.assertFalse(store.should_run(3, "c", LoginMode.ENTERPRISE, resume=True))

    def test_exit_codes_match_contract(self):
        self.assertEqual(0, exit_code_for([ResultStatus.SUCCESS, ResultStatus.DUPLICATE]))
        self.assertEqual(2, exit_code_for([ResultStatus.SUCCESS, ResultStatus.FAILED]))
```

- [ ] **Step 2：运行测试并确认模块不存在**

Run: `python -m unittest tests.batch_login.test_checkpoint -v`

Expected: 导入失败，包含 `No module named 'batch_login.checkpoint'`。

- [ ] **Step 3：实现 checkpoint 存储**

创建 `scripts/batch_login/checkpoint.py`：

```python
from __future__ import annotations

import json
import os
from pathlib import Path

from .models import LoginMode, ResultStatus, RunRecord


TERMINAL_SUCCESS = {ResultStatus.SUCCESS.value, ResultStatus.DUPLICATE.value}


class CheckpointStore:
    def __init__(self, path: Path):
        self.path = path
        self._latest: dict[tuple[int, str, str], dict] = {}
        if path.exists():
            for line in path.read_text(encoding="utf-8").splitlines():
                if not line.strip():
                    continue
                item = json.loads(line)
                key = (int(item["lineNumber"]), item["accountHash"], item["mode"])
                self._latest[key] = item

    def append(self, record: RunRecord) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        payload = record.as_json()
        with self.path.open("a", encoding="utf-8", newline="\n") as handle:
            handle.write(json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n")
            handle.flush()
            os.fsync(handle.fileno())
        key = (record.line_number, record.account_hash, record.mode.value)
        self._latest[key] = payload

    def should_run(self, line_number: int, account_hash: str, mode: LoginMode, resume: bool) -> bool:
        if not resume:
            return True
        item = self._latest.get((line_number, account_hash, mode.value))
        if item is None:
            return True
        if item.get("status") in TERMINAL_SUCCESS:
            return False
        return bool(item.get("retryable", False))


def exit_code_for(statuses: list[ResultStatus]) -> int:
    return 0 if all(status in {ResultStatus.SUCCESS, ResultStatus.DUPLICATE} for status in statuses) else 2
```

- [ ] **Step 4：运行测试并提交**

Run: `python -m unittest tests.batch_login.test_checkpoint -v`

Expected: 所有测试通过。

```powershell
git add scripts/batch_login/checkpoint.py tests/batch_login/test_checkpoint.py
git commit -m "feat(batch-login): 增加断点恢复记录"
```

## Task 6：实现 RS Admin API 客户端与回调解析

**Files:**
- Create: `scripts/batch_login/rs_client.py`
- Create: `tests/batch_login/test_rs_client.py`
- Create: `scripts/requirements-batch-login.txt`

- [ ] **Step 1：声明 Python 运行依赖**

创建 `scripts/requirements-batch-login.txt`：

```text
httpx>=0.28,<1
playwright>=1.50,<2
```

- [ ] **Step 2：写回调解析和旧/新错误兼容失败测试**

创建 `tests/batch_login/test_rs_client.py`：

```python
import asyncio
import sys
import unittest
from pathlib import Path

import httpx

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.rs_client import RsApiError, RsClient, parse_callback_url


class RsClientTests(unittest.IsolatedAsyncioTestCase):
    def test_parse_descriptor_and_final_callback(self):
        descriptor = parse_callback_url(
            "http://127.0.0.1/signin/callback?login_option=external_idp&issuer_url=https%3A%2F%2Flogin.microsoftonline.com%2Ft%2Fv2.0&client_id=c&scopes=s&state=p"
        )
        self.assertEqual("/signin/callback", descriptor["path"])
        self.assertEqual("external_idp", descriptor["loginOption"])
        self.assertEqual("c", descriptor["clientId"])

        final = parse_callback_url("http://127.0.0.1/oauth/callback?code=abc&state=xyz")
        self.assertEqual("abc", final["code"])
        self.assertEqual("xyz", final["state"])

        fragment = parse_callback_url("http://127.0.0.1/oauth/callback#code=fragment-code&state=fragment-state")
        self.assertEqual("fragment-code", fragment["code"])

    async def test_structured_error_and_legacy_auth_error_are_normalized(self):
        calls = 0

        def handler(request: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            if request.url.path.endswith("/structured"):
                return httpx.Response(400, json={"error": {"type": "invalid_request", "message": "bad", "code": "state_mismatch", "stage": "social_callback", "retryable": False}})
            return httpx.Response(401, json={"error": {"type": "authentication_error", "message": "Invalid key"}})

        transport = httpx.MockTransport(handler)
        async with RsClient("https://rs.example", "key", transport=transport) as client:
            with self.assertRaises(RsApiError) as structured:
                await client._request("GET", "/structured")
            self.assertEqual("state_mismatch", structured.exception.code)
            with self.assertRaises(RsApiError) as legacy:
                await client._request("GET", "/legacy")
            self.assertEqual("rs_auth_failed", legacy.exception.code)
        self.assertEqual(2, calls)

    async def test_retryable_5xx_retries_twice_then_succeeds(self):
        calls = 0
        def handler(request: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            return httpx.Response(503 if calls < 3 else 200, json={} if calls < 3 else {"ok": True})

        async with RsClient("https://rs.example", "key", transport=httpx.MockTransport(handler), retry_delays=(0, 0)) as client:
            self.assertEqual({"ok": True}, await client._request("GET", "/retry"))
        self.assertEqual(3, calls)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 3：安装依赖并确认测试因模块缺失失败**

Run: `python -m pip install -r scripts/requirements-batch-login.txt`

Expected: 安装成功。

Run: `python -m unittest tests.batch_login.test_rs_client -v`

Expected: 导入失败，包含 `No module named 'batch_login.rs_client'`。

- [ ] **Step 4：实现 RS 客户端核心与回调解析**

创建 `scripts/batch_login/rs_client.py`，核心接口如下：

```python
from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Any, Iterable
from urllib.parse import parse_qs, urlsplit

import httpx


@dataclass(slots=True)
class RsApiError(Exception):
    code: str
    stage: str
    retryable: bool
    status_code: int
    message: str

    def __str__(self) -> str:
        return self.message


def parse_callback_url(raw_url: str) -> dict[str, Any]:
    parts = urlsplit(raw_url)
    params = parse_qs(parts.query, keep_blank_values=True)
    fragment_params = parse_qs(parts.fragment, keep_blank_values=True)
    for key, values in fragment_params.items():
        params.setdefault(key, values)
    one = lambda name: params.get(name, [None])[0]
    payload = {
        "code": one("code"),
        "state": one("state"),
        "loginOption": one("login_option") or "",
        "path": parts.path,
        "issuerUrl": one("issuer_url"),
        "clientId": one("client_id"),
        "scopes": one("scopes") or one("scope"),
        "loginHint": one("login_hint"),
    }
    if not payload["code"] and not (payload["issuerUrl"] and payload["clientId"]):
        raise ValueError("回调 URL 缺少 code 或 external_idp descriptor")
    return {key: value for key, value in payload.items() if value is not None}


class RsClient:
    def __init__(
        self,
        base_url: str,
        admin_key: str,
        *,
        timeout: float = 30,
        transport: httpx.AsyncBaseTransport | None = None,
        retry_delays: Iterable[float] = (0.5, 1.0),
    ):
        self.base_url = base_url.rstrip("/") + "/api/admin"
        self.retry_delays = tuple(retry_delays)
        self.client = httpx.AsyncClient(
            headers={"x-api-key": admin_key, "accept": "application/json"},
            timeout=timeout,
            transport=transport,
        )

    async def __aenter__(self) -> "RsClient":
        return self

    async def __aexit__(self, *_args) -> None:
        await self.client.aclose()

    async def _request(self, method: str, path: str, json: dict | None = None) -> dict:
        delays = (0.0,) + self.retry_delays
        for attempt, delay in enumerate(delays):
            if delay:
                await asyncio.sleep(delay)
            try:
                response = await self.client.request(method, self.base_url + path, json=json)
            except httpx.RequestError as error:
                if attempt + 1 < len(delays):
                    continue
                raise RsApiError("network_error", "rs_request", True, 0, str(error)) from error
            if response.status_code < 400:
                return response.json() if response.content else {}
            if response.status_code >= 500 and attempt + 1 < len(delays):
                continue
            try:
                error = response.json().get("error", {})
            except ValueError:
                error = {}
            error_type = error.get("type", "internal_error")
            code = error.get("code")
            if not code:
                code = "rs_auth_failed" if response.status_code in {401, 403} else (
                    "upstream_error" if response.status_code >= 500 else "rs_internal_error"
                )
            raise RsApiError(
                code=code,
                stage=error.get("stage", "rs_request"),
                retryable=bool(error.get("retryable", response.status_code >= 500)),
                status_code=response.status_code,
                message=error.get("message", f"RS HTTP {response.status_code}"),
            )
        raise AssertionError("unreachable")
```

- [ ] **Step 5：增加完整登录方法**

在同一类中增加：

```python
async def preflight(self) -> None:
    await self._request("GET", "/credentials")

async def start_idc(self, *, region: str, start_url: str, email: str) -> dict:
    return await self._request("POST", "/auth/idc/start", {
        "region": region, "startUrl": start_url, "email": email,
    })

async def poll_idc(self, session_id: str) -> dict:
    return await self._request("POST", f"/auth/idc/poll/{session_id}")

async def start_social(self, *, email: str) -> dict:
    return await self._request("POST", "/auth/social/start", {"email": email})

async def complete_social(self, session_id: str, callback_url: str) -> dict:
    return await self._request(
        "POST", f"/auth/social/complete/{session_id}", parse_callback_url(callback_url)
    )

async def cancel_idc(self, session_id: str) -> dict:
    return await self._request("DELETE", f"/auth/idc/{session_id}")

async def cancel_social(self, session_id: str) -> dict:
    return await self._request("DELETE", f"/auth/social/{session_id}")
```

- [ ] **Step 6：运行 RS 客户端测试并提交**

Run: `python -m unittest tests.batch_login.test_rs_client -v`

Expected: 所有测试通过。

```powershell
git add scripts/batch_login/rs_client.py scripts/requirements-batch-login.txt tests/batch_login/test_rs_client.py
git commit -m "feat(batch-login): 增加 RS 自动登录客户端"
```

## Task 7：实现 Playwright 页面驱动与本地页面合约测试

**Files:**
- Create: `scripts/batch_login/browser_flows.py`
- Create: `tests/batch_login/test_browser_contract.py`

- [ ] **Step 1：安装 Chromium 测试运行时**

Run: `python -m playwright install chromium`

Expected: Chromium 安装成功；不安装系统级服务。

- [ ] **Step 2：写本地企业登录和回调捕获合约测试**

创建 `tests/batch_login/test_browser_contract.py`。测试使用 `ThreadingHTTPServer` 提供以下页面：

```python
PAGES = {
    "/enterprise": """
      <form action='/password'>
        <label>用户名 <input name='username'></label>
        <button>下一步</button>
      </form>
    """,
    "/password": """
      <form action='/done'>
        <label>密码 <input name='password' type='password'></label>
        <button>登录</button>
      </form>
    """,
    "/done": "<h1>授权成功</h1>",
    "/portal": """
      <form action='http://127.0.0.1:9/signin/callback'>
        <input type='hidden' name='login_option' value='external_idp'>
        <input type='hidden' name='issuer_url' value='https://login.microsoftonline.com/t/v2.0'>
        <input type='hidden' name='client_id' value='client'>
        <input type='hidden' name='state' value='portal-state'>
        <label>电子邮件 <input name='email' type='email'></label>
        <button>继续</button>
      </form>
    """,
}
```

核心测试：

```python
class BrowserContractTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.server, self.base_url = start_fixture_server(PAGES)
        self.playwright = await async_playwright().start()
        self.browser = await self.playwright.chromium.launch(headless=True)
        self.driver = BrowserFlows(self.browser, timeout_seconds=5, mfa_timeout_seconds=1)

    async def asyncTearDown(self):
        await self.browser.close()
        await self.playwright.stop()
        self.server.shutdown()

    async def test_enterprise_fills_username_then_password(self):
        async with self.driver.account_context() as session:
            await session.complete_enterprise(self.base_url + "/enterprise", "alice", "secret")
            self.assertTrue(session.page.url.endswith("/done?username=alice") or "/done" in session.page.url)

    async def test_loopback_connection_failure_still_yields_callback_url(self):
        async with self.driver.account_context() as session:
            callback = await session.capture_callback(
                self.base_url + "/portal",
                "user@example.com",
                "secret",
                expected_path="/signin/callback",
            )
            self.assertIn("login_option=external_idp", callback)
            self.assertIn("state=portal-state", callback)
```

测试文件中加入完整 fixture server：

```python
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread
from urllib.parse import urlsplit


def start_fixture_server(pages):
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            path = urlsplit(self.path).path
            body = pages.get(path, "<h1>not found</h1>").encode("utf-8")
            self.send_response(200 if path in pages else 404)
            self.send_header("content-type", "text/html; charset=utf-8")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, _format, *_args):
            return

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    Thread(target=server.serve_forever, daemon=True).start()
    host, port = server.server_address
    return server, f"http://{host}:{port}"
```

- [ ] **Step 3：运行测试并确认浏览器模块不存在**

Run: `python -m unittest tests.batch_login.test_browser_contract -v`

Expected: 导入失败，包含 `No module named 'batch_login.browser_flows'`。

- [ ] **Step 4：实现浏览器错误与账号上下文**

创建 `scripts/batch_login/browser_flows.py`，定义：

```python
from __future__ import annotations

import asyncio
import re
from contextlib import asynccontextmanager
from dataclasses import dataclass
from urllib.parse import urlsplit

from playwright.async_api import Browser, BrowserContext, Error as PlaywrightError, Page


@dataclass(slots=True)
class BrowserFlowError(Exception):
    code: str
    stage: str
    retryable: bool
    message: str

    def __str__(self) -> str:
        return self.message


class BrowserFlows:
    def __init__(self, browser: Browser, *, timeout_seconds: float, mfa_timeout_seconds: float):
        self.browser = browser
        self.timeout_seconds = timeout_seconds
        self.mfa_timeout_seconds = mfa_timeout_seconds

    @asynccontextmanager
    async def account_context(self):
        context = await self.browser.new_context()
        page = await context.new_page()
        try:
            yield AccountBrowserSession(
                context, page,
                timeout_seconds=self.timeout_seconds,
                mfa_timeout_seconds=self.mfa_timeout_seconds,
            )
        finally:
            await context.close()
```

- [ ] **Step 5：实现稳健控件定位和页面分类**

`AccountBrowserSession` 使用以下顺序定位控件：可访问性 label/role、稳定 `name`、最后才是 input type。实现方法：

```python
class AccountBrowserSession:
    ACCOUNT_NAMES = re.compile(r"用户名|账号|电子邮件|邮箱|email|username|user name", re.I)
    PASSWORD_NAMES = re.compile(r"密码|password", re.I)
    NEXT_NAMES = re.compile(r"下一步|继续|打开|next|continue|open", re.I)
    SIGNIN_NAMES = re.compile(r"登录|登入|sign in|log in|submit", re.I)
    CONSENT_NAMES = re.compile(r"同意|允许|接受|accept|allow", re.I)
    DECLINE_PERSIST_NAMES = re.compile(r"否|不保持登录|no|do not stay signed in", re.I)
    MFA_TEXT = re.compile(r"验证码|验证身份|多重身份|批准登录|authenticator|verification code|two.factor|mfa", re.I)
    CAPTCHA_TEXT = re.compile(r"验证码图片|人机验证|captcha|verify you are human", re.I)
    INVALID_TEXT = re.compile(r"密码不正确|账号或密码错误|incorrect password|invalid credentials", re.I)
    LOCKED_TEXT = re.compile(r"账号.*锁定|account.*locked", re.I)

    def __init__(self, context: BrowserContext, page: Page, *, timeout_seconds: float, mfa_timeout_seconds: float):
        self.context = context
        self.page = page
        self.timeout_ms = int(timeout_seconds * 1000)
        self.mfa_timeout_ms = int(mfa_timeout_seconds * 1000)

    async def _first_visible(self, locators):
        for locator in locators:
            try:
                if await locator.first.is_visible(timeout=150):
                    return locator.first
            except PlaywrightError:
                continue
        return None

    async def _fill_account(self, account: str) -> bool:
        locator = await self._first_visible([
            self.page.get_by_label(self.ACCOUNT_NAMES),
            self.page.get_by_role("textbox", name=self.ACCOUNT_NAMES),
            self.page.locator("input[name='loginfmt'], input[name='username'], input[name='email'], input[type='email']"),
        ])
        if locator is None:
            return False
        await locator.fill(account)
        return True

    async def _fill_password(self, password: str) -> bool:
        locator = await self._first_visible([
            self.page.get_by_label(self.PASSWORD_NAMES),
            self.page.locator("input[name='passwd'], input[name='password'], input[type='password']"),
        ])
        if locator is None:
            return False
        await locator.fill(password)
        return True

    async def _click_primary(self, password_stage: bool) -> bool:
        names = self.SIGNIN_NAMES if password_stage else self.NEXT_NAMES
        locator = await self._first_visible([
            self.page.get_by_role("button", name=names),
            self.page.get_by_role("link", name=names),
            self.page.locator("button[type='submit'], input[type='submit']"),
        ])
        if locator is None:
            return False
        await locator.click()
        return True

    async def _click_progress_without_credentials(self) -> bool:
        locator = await self._first_visible([
            self.page.get_by_role("button", name=self.DECLINE_PERSIST_NAMES),
            self.page.get_by_role("button", name=self.CONSENT_NAMES),
            self.page.get_by_role("button", name=self.NEXT_NAMES),
            self.page.get_by_role("link", name=self.NEXT_NAMES),
        ])
        if locator is None:
            return False
        await locator.click()
        return True
```

页面正文命中错误密码或账号锁定时分别抛出 `invalid_credentials`、`account_locked`；命中 MFA 时在有界时间内等待 URL 或正文发生变化，超时抛出 `mfa_timeout` 且 `retryable=False`。

- [ ] **Step 6：实现企业登录和请求级回调捕获**

核心循环：

```python
async def _drive_login(self, account: str, password: str, callback_future=None) -> None:
    deadline = asyncio.get_running_loop().time() + self.timeout_ms / 1000
    account_filled = False
    password_filled = False
    while asyncio.get_running_loop().time() < deadline:
        if callback_future is not None and callback_future.done():
            return
        body = await self.page.locator("body").inner_text(timeout=1000)
        if self.INVALID_TEXT.search(body):
            raise BrowserFlowError("invalid_credentials", "browser_login", False, "账号或密码错误")
        if self.LOCKED_TEXT.search(body):
            raise BrowserFlowError("account_locked", "browser_login", False, "账号已锁定")
        manual_code = (
            "captcha_required" if self.CAPTCHA_TEXT.search(body)
            else "mfa_timeout" if self.MFA_TEXT.search(body)
            else None
        )
        if manual_code:
            print("检测到 MFA/验证码，请在当前浏览器窗口中完成人工验证。")
            try:
                await self.page.wait_for_function(
                    "previous => document.body.innerText !== previous", body,
                    timeout=self.mfa_timeout_ms,
                )
                continue
            except PlaywrightError as error:
                message = "等待人工完成验证码超时" if manual_code == "captcha_required" else "等待人工完成 MFA 超时"
                raise BrowserFlowError(manual_code, "mfa", False, message) from error
        if not account_filled and await self._fill_account(account):
            account_filled = True
            await self._click_primary(False)
            await asyncio.sleep(0.2)
            continue
        if not password_filled and await self._fill_password(password):
            password_filled = True
            await self._click_primary(True)
            await asyncio.sleep(0.2)
            continue
        if callback_future is not None and await self._click_progress_without_credentials():
            await asyncio.sleep(0.2)
            continue
        if password_filled and callback_future is None:
            return
        await asyncio.sleep(0.2)
    raise BrowserFlowError("unknown_page", "browser_login", False, "登录页面在超时前未完成")

async def complete_enterprise(self, url: str, account: str, password: str) -> None:
    await self.page.goto(url, wait_until="domcontentloaded", timeout=self.timeout_ms)
    await self._drive_login(account, password)

async def capture_callback(self, url: str, account: str, password: str, *, expected_path: str) -> str:
    loop = asyncio.get_running_loop()
    callback = loop.create_future()

    def observe(request):
        if urlsplit(request.url).path == expected_path and not callback.done():
            callback.set_result(request.url)

    self.context.on("request", observe)
    try:
        try:
            await self.page.goto(url, wait_until="domcontentloaded", timeout=self.timeout_ms)
        except PlaywrightError:
            if not callback.done():
                raise
        await self._drive_login(account, password, callback)
        return await asyncio.wait_for(callback, timeout=self.timeout_ms / 1000)
    finally:
        self.context.remove_listener("request", observe)
```

- [ ] **Step 7：运行浏览器合约测试并提交**

Run: `python -m unittest tests.batch_login.test_browser_contract -v`

Expected: 企业两页登录和 loopback callback 捕获测试通过。

```powershell
git add scripts/batch_login/browser_flows.py tests/batch_login/test_browser_contract.py
git commit -m "feat(batch-login): 增加 Playwright 登录驱动"
```

## Task 8：实现串行 runner 与两段 Microsoft 编排

**Files:**
- Create: `scripts/batch_login/runner.py`
- Create: `tests/batch_login/test_runner.py`
- Modify: `scripts/batch_login/browser_flows.py`

- [ ] **Step 1：写企业成功、Microsoft 两段和失败取消测试**

创建 `tests/batch_login/test_runner.py`，使用 fake client/browser，不启动真实网络：

```python
class FakeClient:
    def __init__(self):
        self.completed = []
        self.cancelled = []

    async def start_idc(self, **_kwargs):
        return {"sessionId": "idc-1", "verificationUriComplete": "https://aws/login", "pollInterval": 0}

    async def poll_idc(self, _session_id):
        return {"status": "success", "credentialId": 5, "duplicate": False}

    async def start_social(self, **_kwargs):
        return {"sessionId": "social-1", "portalUrl": "https://kiro/signin"}

    async def complete_social(self, session_id, callback):
        self.completed.append((session_id, callback))
        if len(self.completed) == 1:
            return {"status": "continue", "nextUrl": "https://login.microsoftonline.com/authorize"}
        return {"status": "success", "credentialId": 8, "duplicate": True}

    async def cancel_idc(self, session_id):
        self.cancelled.append(("idc", session_id))

    async def cancel_social(self, session_id):
        self.cancelled.append(("social", session_id))


class FakeBrowserSession:
    def __init__(self, fail=False):
        self.fail = fail
        self.callbacks = iter([
            "http://127.0.0.1/signin/callback?login_option=external_idp&issuer_url=https%3A%2F%2Flogin.microsoftonline.com%2Ft%2Fv2.0&client_id=c&state=p",
            "http://127.0.0.1/oauth/callback?code=final&state=s",
        ])

    async def complete_enterprise(self, *_args):
        if self.fail:
            raise BrowserFlowError("invalid_credentials", "browser_login", False, "bad password")

    async def capture_callback(self, *_args, **_kwargs):
        return next(self.callbacks)


class FakeBrowserFactory:
    def __init__(self, session):
        self.session = session

    def account_context(self):
        session = self.session

        class Context:
            async def __aenter__(self):
                return session

            async def __aexit__(self, *_args):
                return False

        return Context()


class SequencedBrowserFactory:
    def __init__(self, sessions):
        self.sessions = iter(sessions)

    def account_context(self):
        return FakeBrowserFactory(next(self.sessions)).account_context()


def entry(account="alice", password="secret"):
    return AccountEntry(line_number=1, account=account, password=password)


def settings():
    return RunnerSettings(region="us-east-1", start_url="https://example.awsapps.com/start")


class RunnerTests(unittest.IsolatedAsyncioTestCase):
    async def test_microsoft_submits_two_callbacks_in_same_session(self):
        client = FakeClient()
        runner = BatchLoginRunner(client, FakeBrowserFactory(FakeBrowserSession()), checkpoint=None)
        outcome = await runner.run_one(LoginMode.MICROSOFT, entry("user@example.com", "pw"), settings())
        self.assertEqual(ResultStatus.DUPLICATE, outcome.status)
        self.assertEqual(2, len(client.completed))
        self.assertEqual({"social-1"}, {item[0] for item in client.completed})

    async def test_browser_failure_cancels_server_session(self):
        client = FakeClient()
        runner = BatchLoginRunner(client, FakeBrowserFactory(FakeBrowserSession(fail=True)), checkpoint=None)
        outcome = await runner.run_one(LoginMode.ENTERPRISE, entry("alice", "wrong"), settings())
        self.assertEqual("invalid_credentials", outcome.code)
        self.assertEqual([("idc", "idc-1")], client.cancelled)

    async def test_batch_continues_after_non_retryable_failure(self):
        client = FakeClient()
        factory = SequencedBrowserFactory([
            FakeBrowserSession(fail=True),
            FakeBrowserSession(fail=False),
        ])
        runner = BatchLoginRunner(client, factory, checkpoint=None)
        outcomes = await runner.run_batch(
            LoginMode.ENTERPRISE,
            [entry("first", "wrong"), AccountEntry(2, "second", "right")],
            settings(),
            resume=False,
            run_id="run-1",
        )
        self.assertEqual([ResultStatus.FAILED, ResultStatus.SUCCESS], [item.status for item in outcomes])
```

另加以下两个明确测试：

```python
async def test_wait_idc_repeats_pending_until_success(self):
    client = FakeClient()
    replies = iter([
        {"status": "pending"},
        {"status": "success", "credentialId": 6, "duplicate": False},
    ])
    client.poll_idc = AsyncMock(side_effect=lambda _session: next(replies))
    runner = BatchLoginRunner(client, FakeBrowserFactory(FakeBrowserSession()), checkpoint=None)
    result = await runner._wait_idc("idc-1", 0)
    self.assertEqual(6, result["credentialId"])
    self.assertEqual(2, client.poll_idc.await_count)

async def test_expired_idc_is_non_retryable_failure(self):
    client = FakeClient()
    client.poll_idc = AsyncMock(return_value={"status": "expired"})
    runner = BatchLoginRunner(client, FakeBrowserFactory(FakeBrowserSession()), checkpoint=None)
    outcome = await runner.run_one(LoginMode.ENTERPRISE, entry(), settings())
    self.assertEqual("session_expired", outcome.code)
    self.assertFalse(outcome.retryable)
```

测试文件顶部从 `unittest.mock` 导入 `AsyncMock`。

- [ ] **Step 2：运行测试并确认 runner 不存在**

Run: `python -m unittest tests.batch_login.test_runner -v`

Expected: 导入失败，包含 `No module named 'batch_login.runner'`。

- [ ] **Step 3：实现单账号编排**

创建 `scripts/batch_login/runner.py`，核心实现：

```python
from __future__ import annotations

import asyncio
from contextlib import suppress

from .browser_flows import BrowserFlowError
from .models import AccountEntry, LoginMode, LoginOutcome, ResultStatus
from .rs_client import RsApiError


@dataclass(slots=True, frozen=True)
class RunnerSettings:
    region: str
    start_url: str | None = None


def outcome_from_success(result: dict) -> LoginOutcome:
    duplicate = bool(result.get("duplicate", False))
    return LoginOutcome(
        status=ResultStatus.DUPLICATE if duplicate else ResultStatus.SUCCESS,
        credential_id=result.get("credentialId"),
        duplicate=duplicate,
    )


class BatchLoginRunner:
    def __init__(self, client, browser_factory, checkpoint):
        self.client = client
        self.browser_factory = browser_factory
        self.checkpoint = checkpoint

    async def _wait_idc(self, session_id: str, poll_interval: float) -> dict:
        while True:
            result = await self.client.poll_idc(session_id)
            if result["status"] == "pending":
                await asyncio.sleep(max(poll_interval, 0.2))
                continue
            if result["status"] == "expired":
                raise RsApiError("session_expired", "idc_poll", False, 410, "IDC 会话已过期")
            return result

    async def _run_enterprise(self, entry: AccountEntry, settings) -> LoginOutcome:
        started = await self.client.start_idc(
            region=settings.region, start_url=settings.start_url, email=entry.account
        )
        session_id = started["sessionId"]
        try:
            async with self.browser_factory.account_context() as browser:
                browser_task = asyncio.create_task(browser.complete_enterprise(
                    started.get("verificationUriComplete") or started["verificationUri"],
                    entry.account,
                    entry.password,
                ))
                poll_task = asyncio.create_task(self._wait_idc(session_id, started.get("pollInterval", 5)))
                tasks = {browser_task, poll_task}
                try:
                    done, _pending = await asyncio.wait(
                        tasks, return_when=asyncio.FIRST_EXCEPTION
                    )
                    for task in done:
                        task.result()
                    result = await poll_task
                finally:
                    for task in tasks:
                        if not task.done():
                            task.cancel()
            return outcome_from_success(result)
        except BaseException:
            with suppress(Exception):
                await self.client.cancel_idc(session_id)
            raise

    async def _run_microsoft(self, entry: AccountEntry, settings) -> LoginOutcome:
        started = await self.client.start_social(email=entry.account)
        session_id = started["sessionId"]
        try:
            async with self.browser_factory.account_context() as browser:
                first = await browser.capture_callback(
                    started["portalUrl"], entry.account, entry.password,
                    expected_path="/signin/callback",
                )
                result = await self.client.complete_social(session_id, first)
                if result["status"] == "continue":
                    final = await browser.capture_callback(
                        result["nextUrl"], entry.account, entry.password,
                        expected_path="/oauth/callback",
                    )
                    result = await self.client.complete_social(session_id, final)
                return outcome_from_success(result)
        except BaseException:
            with suppress(Exception):
                await self.client.cancel_social(session_id)
            raise
```

`run_one` 使用以下精确映射捕获 `BrowserFlowError` 和 `RsApiError`；`asyncio.CancelledError` 不吞掉，清理后重新抛出：

```python
async def run_one(self, mode, entry, settings) -> LoginOutcome:
    try:
        if mode is LoginMode.ENTERPRISE:
            return await self._run_enterprise(entry, settings)
        return await self._run_microsoft(entry, settings)
    except (BrowserFlowError, RsApiError) as error:
        status = (
            ResultStatus.MANUAL_REQUIRED
            if error.code in {"mfa_timeout", "captcha_required"}
            else ResultStatus.FAILED
        )
        return LoginOutcome(
            status=status,
            code=error.code,
            stage=error.stage,
            retryable=error.retryable,
            message=str(error),
        )
```

- [ ] **Step 4：实现逐批串行、checkpoint 和 Ctrl+C 清理**

在 `BatchLoginRunner` 增加 `run_batch`：

```python
async def run_batch(self, mode, entries, settings, *, resume: bool, run_id: str):
    outcomes = []
    for entry in entries:
        if self.checkpoint and not self.checkpoint.should_run(
            entry.line_number, entry.account_hash, mode, resume
        ):
            continue
        outcome = await self.run_one(mode, entry, settings)
        outcomes.append(outcome)
        if self.checkpoint:
            self.checkpoint.append(record_from_outcome(run_id, mode, entry, outcome))
    return outcomes
```

实现 `record_from_outcome`：

```python
def record_from_outcome(run_id, mode, entry, outcome):
    return RunRecord(
        run_id=run_id,
        line_number=entry.line_number,
        account_hash=entry.account_hash,
        account_masked=mask_account(entry.account),
        mode=mode,
        status=outcome.status,
        stage=outcome.stage or "done",
        attempts=1,
        timestamp=datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        credential_id=outcome.credential_id,
        code=outcome.code,
        retryable=outcome.retryable,
        message=redact_text(outcome.message or "") or None,
    )
```

为此在 `runner.py` 导入 `datetime`、`timezone`、`RunRecord`、`mask_account` 和 `redact_text`。`AccountEntry.password`、完整账号、回调 URL 不进入 `RunRecord`。捕获 `asyncio.CancelledError` 时先执行当前 session 的取消逻辑，再重新抛出供 CLI 返回 130。

- [ ] **Step 5：补充 Microsoft 二段同 context 合约测试**

扩展 `test_browser_contract.py`，在同一个 `account_context()` 中先捕获 `/signin/callback`，再打开第二个本地 mock Microsoft URL 并捕获 `/oauth/callback`。断言第二段页面能读取第一段设置的 Cookie，证明没有创建新 BrowserContext。

- [ ] **Step 6：运行 runner 与浏览器合约测试并提交**

Run: `python -m unittest tests.batch_login.test_runner tests.batch_login.test_browser_contract -v`

Expected: 串行编排、取消和两段同 context 测试全部通过。

```powershell
git add scripts/batch_login/runner.py scripts/batch_login/browser_flows.py tests/batch_login/test_runner.py tests/batch_login/test_browser_contract.py
git commit -m "feat(batch-login): 编排企业与微软批量登录"
```

## Task 9：交付 CLI、文档和完整验证

**Files:**
- Create: `scripts/batch_login/cli.py`
- Create: `scripts/kiro_batch_login.py`
- Modify: `scripts/batch_login/__init__.py`
- Modify: `README.md`
- Modify: `.gitignore`
- Test: `tests/batch_login/test_cli.py`

- [ ] **Step 1：写 CLI 参数、安全预检和退出码失败测试**

创建 `tests/batch_login/test_cli.py`，覆盖：

```python
class CliTests(unittest.TestCase):
    def test_enterprise_requires_start_url(self):
        parser = build_parser()
        with self.assertRaises(SystemExit):
            parser.parse_args(["enterprise", "--input", "accounts.txt", "--rs-url", "https://rs"])

    def test_admin_key_must_come_from_environment(self):
        parser = build_parser()
        args = parser.parse_args([
            "microsoft", "--input", "accounts.txt", "--rs-url", "https://rs"
        ])
        with self.assertRaises(SystemExit):
            validate_args(args, environ={})

    def test_parser_has_no_password_command_line_option(self):
        help_text = build_parser().format_help().casefold()
        self.assertNotIn("--password", help_text)
```

- [ ] **Step 2：运行测试并确认 CLI 模块不存在**

Run: `python -m unittest tests.batch_login.test_cli -v`

Expected: 导入失败，包含 `No module named 'batch_login.cli'`。

- [ ] **Step 3：实现 argparse 和配置校验**

创建 `scripts/batch_login/cli.py`：

```python
from __future__ import annotations

import argparse
import asyncio
import os
import sys
from pathlib import Path
from uuid import uuid4

from playwright.async_api import async_playwright

from .browser_flows import BrowserFlows
from .checkpoint import CheckpointStore, exit_code_for
from .input_parser import parse_accounts
from .models import LoginMode
from .redaction import mask_account, redact_text
from .rs_client import RsClient
from .runner import BatchLoginRunner, RunnerSettings


DEFAULT_FORMAT = "{account}----{password}"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Kiro RS 批量企业/Microsoft 自动登录")
    subparsers = parser.add_subparsers(dest="mode", required=True)
    for name in ("enterprise", "microsoft"):
        command = subparsers.add_parser(name)
        command.add_argument("--input", required=True)
        command.add_argument("--format", default=DEFAULT_FORMAT)
        command.add_argument("--rs-url", required=True)
        command.add_argument("--admin-key-env", default="KIRO_RS_ADMIN_KEY")
        command.add_argument("--region", default="us-east-1")
        command.add_argument("--timeout", type=float, default=180)
        command.add_argument("--mfa-timeout", type=float, default=300)
        command.add_argument("--result", type=Path)
        command.add_argument("--resume", action="store_true")
        command.add_argument("--headless", action="store_true")
    subparsers.choices["enterprise"].add_argument("--start-url", required=True)
    return parser


def validate_args(args, environ=os.environ):
    key = environ.get(args.admin_key_env, "").strip()
    if not key:
        raise SystemExit(f"环境变量 {args.admin_key_env} 未设置")
    if args.timeout <= 0 or args.mfa_timeout <= 0:
        raise SystemExit("timeout 和 mfa-timeout 必须大于 0")
    return key


def read_input(path: str) -> str:
    if path == "-":
        return sys.stdin.read()
    return Path(path).read_text(encoding="utf-8-sig")
```

- [ ] **Step 4：实现异步 main 和薄入口**

实现完整 `async_main` 和同步 `main`：

```python
async def async_main(args, admin_key: str) -> int:
    mode = LoginMode(args.mode)
    parsed = parse_accounts(read_input(args.input), args.format, mode)
    for issue in parsed.issues:
        print(f"第 {issue.line_number} 行 [{issue.code}] {issue.message}", file=sys.stderr)
    fatal_issues = [issue for issue in parsed.issues if issue.code != "duplicate_input"]
    if fatal_issues:
        return 1
    if not parsed.entries:
        print("没有可执行账号", file=sys.stderr)
        return 1

    print(f"将串行处理 {len(parsed.entries)} 个账号：")
    for item in parsed.entries:
        print(f"  第 {item.line_number} 行 {mask_account(item.account)}")

    run_id = uuid4().hex
    result_path = args.result or Path(f"batch-login-{run_id}.jsonl")
    checkpoint = CheckpointStore(result_path)
    settings = RunnerSettings(
        region=args.region,
        start_url=getattr(args, "start_url", None),
    )

    async with RsClient(args.rs_url, admin_key) as client:
        await client.preflight()
        async with async_playwright() as playwright:
            browser = await playwright.chromium.launch(headless=args.headless)
            try:
                browser_flows = BrowserFlows(
                    browser,
                    timeout_seconds=args.timeout,
                    mfa_timeout_seconds=args.mfa_timeout,
                )
                runner = BatchLoginRunner(client, browser_flows, checkpoint)
                outcomes = await runner.run_batch(
                    mode,
                    parsed.entries,
                    settings,
                    resume=args.resume,
                    run_id=run_id,
                )
            finally:
                await browser.close()
    return exit_code_for([outcome.status for outcome in outcomes])


def main(argv=None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        admin_key = validate_args(args)
        return asyncio.run(async_main(args, admin_key))
    except KeyboardInterrupt:
        return 130
    except Exception as error:
        print(f"批量登录启动失败：{redact_text(str(error))}", file=sys.stderr)
        return 1
```

实现入口 `scripts/kiro_batch_login.py`：

```python
#!/usr/bin/env python3
from batch_login.cli import main


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 5：更新忽略规则和 README**

`.gitignore` 增加：

```gitignore
/batch-login-*.jsonl
/scripts/__pycache__/
/scripts/batch_login/__pycache__/
/tests/batch_login/__pycache__/
```

`README.md` 新增“批量企业/Microsoft 自动登录”章节，必须包含：

- Python 3.11+ 安装依赖命令。
- `python -m playwright install chromium`。
- 默认/自定义格式示例。
- Enterprise 和 Microsoft 两条完整 PowerShell 命令。
- `KIRO_RS_ADMIN_KEY` 环境变量设置。
- RS 只能通过 HTTPS 或 SSH 本地转发访问时的示例：

```powershell
ssh -N -L 18080:127.0.0.1:8080 user@rs-host
```

- MFA/验证码人工接管说明。
- 密码、结果文件、截图/HAR 的安全警告。
- 退出码 0/1/2/130 和 `--resume` 行为。

- [ ] **Step 6：运行全部 Python 单元与浏览器合约测试**

Run: `python -m unittest discover -s tests/batch_login -t . -v`

Expected: 所有 Python 测试通过，无真实 AWS/Microsoft 网络访问。

- [ ] **Step 7：运行 CLI 只读冒烟检查**

Run: `python scripts/kiro_batch_login.py --help`

Expected: 显示 `enterprise` 和 `microsoft` 子命令，不出现密码参数。

Run: `python scripts/kiro_batch_login.py enterprise --help`

Expected: 显示 `--start-url`、`--region`、`--format`、`--resume`。

- [ ] **Step 8：运行 Rust、前端和格式验证**

Run: `cargo fmt --check`

Expected: Rust 格式检查通过。

Run: `cargo test -j 1`

Expected: 全部 Rust 测试通过。

Run: `cd admin-ui; bun test; bun run build`

Expected: 前端测试和构建通过。

Run: `git diff --check`

Expected: 无空白错误。

- [ ] **Step 9：人工联调前检查敏感信息**

Run: `rg -n -i "password|refresh[_-]?token|access[_-]?token|client[_-]?secret|authorization" batch-login-*.jsonl`

Expected: 若结果文件存在，不得出现明文密码、token 或 client secret；允许字段名 `code`、`status`、`retryable`，不允许敏感值。

人工联调使用一个企业测试账号和一个 Microsoft 测试账号；联调失败时先查看脱敏错误码。只有仓库协议未覆盖页面分支时才显式开启失败截图或采集脱敏 HAR。

- [ ] **Step 10：提交最终本地变更**

```powershell
git add scripts/batch_login scripts/kiro_batch_login.py scripts/requirements-batch-login.txt tests/batch_login README.md .gitignore
git commit -m "feat(batch-login): 交付独立批量登录 CLI"
```

## 最终验收清单

- [ ] `enterprise` 能解析账号文件、自动填写 AWS 登录并由 RS 返回凭证 ID。
- [ ] `microsoft` 能在同一 BrowserContext 内完成 Kiro descriptor 和 Entra final callback 两段提交。
- [ ] 自定义格式、BOM、注释、空值和重复输入均在浏览器启动前处理。
- [ ] 每个账号使用独立 BrowserContext，批次固定串行。
- [ ] MFA/验证码等待人工接管，超时后取消 session 并继续后续账号。
- [ ] Ctrl+C 取消当前 RS session、关闭浏览器并以 130 退出。
- [ ] checkpoint/result 不包含密码、token、OAuth code、Admin Key 或完整 callback URL。
- [ ] 重复登录返回已有 `credentialId` 和 `duplicate=true`，不新增凭证。
- [ ] 新增取消接口和结构化错误继续受 Admin Key 鉴权。
- [ ] Python、Playwright 合约、Rust、前端测试和构建全部通过。
- [ ] 所有提交仅保存在本地，没有推送 GitHub。
