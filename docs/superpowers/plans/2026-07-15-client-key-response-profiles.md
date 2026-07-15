# 客户端 Key 双回复模式 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** 让每把客户端 Key 可固定选择 Claude 检测兼容或 Kiro 原生回复，同时保持现有工具、重试、缓存、计费和 1 秒 SSE 首响应能力。

**Architecture:** 在 ClientKey 持久化模型中新增 ClientResponseMode，并在一次原子鉴权中把 ID、分组和模式快照注入 KeyContext。Anthropic 处理层只用该快照控制检测型本地短路和身份归一化，所有协议桥接与可靠性能力继续共用；Admin API/UI 负责创建、编辑和展示模式，trace 与错误快照保存请求发生时的模式。

**Tech Stack:** Rust 2024、Axum 0.8、Serde/serde_json、rusqlite、React 19、TypeScript 6、TanStack Query、Bun Test、Vite。

---

## 文件职责与实施边界

- src/admin/client_keys.rs：回复模式领域类型、Key 持久化、模式保留规则、原子鉴权快照。
- src/admin/types.rs：Admin API 的请求与响应 JSON 结构。
- src/admin/handlers.rs：模式字符串校验、创建和编辑错误映射。
- src/anthropic/middleware.rs：把鉴权快照写入每个请求的 KeyContext。
- src/anthropic/handlers.rs：检测型本地短路和流式/非流式身份归一化分流。
- src/admin/trace_db.rs：trace 表迁移、读写和 API 序列化。
- src/anthropic/error_snapshot.rs：把请求模式送入错误快照写入对象。
- src/admin/error_snapshot_db.rs：错误快照 schema v2、读写和查询结果。
- admin-ui/src/types/api.ts：前端 API 类型。
- admin-ui/src/lib/client-key-response-mode.ts：模式标签、说明和警告的纯函数。
- admin-ui/src/lib/client-key-response-mode.test.ts：前端模式纯函数测试。
- admin-ui/src/components/client-keys-page.tsx：创建、编辑、列表徽标和切换提示。

不修改 ToolCompatibilityMode，不增加客户端 Header 覆盖，不拆分端口，不改变 cache/token/credit 算法，不改变动态模型目录。

---

### Task 1：建立回复模式领域模型和原子鉴权快照

**Files:**
- Modify: src/admin/client_keys.rs:20-203
- Modify: src/admin/client_keys.rs:313-349
- Modify: src/admin/client_keys.rs:419-485
- Test: src/admin/client_keys.rs:562-end

- [x] **Step 1：先写旧数据迁移、显式模式和鉴权快照失败测试**

在 src/admin/client_keys.rs 的 tests 模块加入以下测试。所有测试名使用 response_mode_ 前缀，方便定向运行。

~~~rust
#[test]
fn response_mode_legacy_json_defaults_to_detection() {
    let raw = r#"[{
        "id": 7,
        "key": "csk_legacy",
        "name": "legacy",
        "createdAt": "2026-07-15T00:00:00Z"
    }]"#;
    let keys: Vec<ClientKey> = serde_json::from_str(raw).unwrap();
    assert_eq!(keys[0].response_mode, ClientResponseMode::Detection);
}

#[test]
fn response_mode_explicit_native_round_trips() {
    let mut key = ClientKey {
        id: 8,
        key: "csk_native".into(),
        name: "native".into(),
        description: None,
        disabled: false,
        created_at: "2026-07-15T00:00:00Z".into(),
        last_used_at: None,
        total_calls: 0,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cache_creation_tokens: 0,
        total_cache_read_tokens: 0,
        total_credits: 0.0,
        group: Some("team-a".into()),
        is_system: false,
        response_mode: ClientResponseMode::KiroNative,
    };
    let json = serde_json::to_value(&key).unwrap();
    assert_eq!(json["responseMode"], "kiro_native");
    key = serde_json::from_value(json).unwrap();
    assert_eq!(key.response_mode, ClientResponseMode::KiroNative);
}

#[test]
fn response_mode_unknown_persisted_value_fails_closed_to_detection() {
    let raw = r#"{
        "id": 9,
        "key": "csk_unknown",
        "name": "unknown",
        "createdAt": "2026-07-15T00:00:00Z",
        "responseMode": "misspelled"
    }"#;
    let key: ClientKey = serde_json::from_str(raw).unwrap();
    assert_eq!(key.response_mode, ClientResponseMode::Detection);
}

#[test]
fn response_mode_authorization_returns_one_atomic_snapshot() {
    let manager = ClientKeyManager::new();
    let entry = manager.create_with_mode(
        "native".into(),
        None,
        Some("team-a".into()),
        ClientResponseMode::KiroNative,
    );
    let authorized = manager
        .verify_and_touch_context(&entry.key)
        .expect("key must authorize");
    assert_eq!(authorized.id, entry.id);
    assert_eq!(authorized.group.as_deref(), Some("team-a"));
    assert_eq!(authorized.response_mode, ClientResponseMode::KiroNative);
}

#[test]
fn response_mode_rotation_preserves_mode_and_stats() {
    let manager = ClientKeyManager::new();
    let entry = manager.create_with_mode(
        "native".into(),
        None,
        None,
        ClientResponseMode::KiroNative,
    );
    manager.record_usage(entry.id, 10, 5, 3, 2, 1.5);
    let rotated = manager.rotate(entry.id).unwrap();
    assert_eq!(rotated.response_mode, ClientResponseMode::KiroNative);
    assert_eq!(rotated.total_input_tokens, 10);
    assert_eq!(rotated.total_output_tokens, 5);
    assert_ne!(rotated.key, entry.key);
}

#[test]
fn response_mode_policy_only_detection_allows_detection_behavior() {
    assert!(ClientResponseMode::Detection.allows_detection_shortcuts());
    assert!(ClientResponseMode::Detection.allows_identity_normalization());
    assert!(!ClientResponseMode::KiroNative.allows_detection_shortcuts());
    assert!(!ClientResponseMode::KiroNative.allows_identity_normalization());
}
~~~

- [x] **Step 2：运行定向测试并确认 RED**

Run:

~~~powershell
cargo test -j 1 response_mode_ -- --nocapture
~~~

Expected: 编译失败，报告 ClientResponseMode、response_mode、create_with_mode 或 verify_and_touch_context 尚不存在。

- [x] **Step 3：实现严格 API 解析、宽容持久化和策略方法**

在 CLIENT_KEY_PREFIX 下方加入：

~~~rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientResponseMode {
    KiroNative,
    #[serde(other)]
    Detection,
}

impl Default for ClientResponseMode {
    fn default() -> Self {
        Self::Detection
    }
}

impl ClientResponseMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Detection => "detection",
            Self::KiroNative => "kiro_native",
        }
    }

    pub const fn allows_detection_shortcuts(self) -> bool {
        matches!(self, Self::Detection)
    }

    pub const fn allows_identity_normalization(self) -> bool {
        matches!(self, Self::Detection)
    }
}

impl std::str::FromStr for ClientResponseMode {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "detection" => Ok(Self::Detection),
            "kiro_native" => Ok(Self::KiroNative),
            _ => Err("responseMode 必须是 detection 或 kiro_native"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedClientKey {
    pub id: u64,
    pub group: Option<String>,
    pub response_mode: ClientResponseMode,
}
~~~

Serde 的 other 仅用于磁盘旧值和未来值安全降级；Admin API 不直接反序列化成枚举，而在 Task 2 通过 FromStr 严格拒绝未知字符串。

- [x] **Step 4：给 ClientKey 增加字段并让所有创建路径显式初始化**

在 ClientKey 的 is_system 前加入：

~~~rust
#[serde(default)]
pub response_mode: ClientResponseMode,
~~~

保持现有 create 和 create_with_key 签名用于旧调用方，它们固定创建 Detection；新增显式创建入口：

~~~rust
pub fn create(
    &self,
    name: String,
    description: Option<String>,
    group: Option<String>,
) -> ClientKey {
    self.create_with_mode(
        name,
        description,
        group,
        ClientResponseMode::Detection,
    )
}

pub fn create_with_mode(
    &self,
    name: String,
    description: Option<String>,
    group: Option<String>,
    response_mode: ClientResponseMode,
) -> ClientKey {
    self.create_with_key_and_mode(
        name,
        description,
        group,
        generate_client_key(),
        response_mode,
    )
}

pub fn create_with_key(
    &self,
    name: String,
    description: Option<String>,
    group: Option<String>,
    plaintext: String,
) -> ClientKey {
    self.create_with_key_and_mode(
        name,
        description,
        group,
        plaintext,
        ClientResponseMode::Detection,
    )
}

fn create_with_key_and_mode(
    &self,
    name: String,
    description: Option<String>,
    group: Option<String>,
    plaintext: String,
    response_mode: ClientResponseMode,
) -> ClientKey {
    let mut inner = self.inner.write();
    if let Some(&id) = inner.by_key.get(&plaintext) {
        return inner
            .entries
            .get(&id)
            .cloned()
            .expect("by_key 与 entries 应一致");
    }
    let id = inner.next_id;
    inner.next_id += 1;
    let entry = ClientKey {
        id,
        key: plaintext.clone(),
        name,
        description,
        disabled: false,
        created_at: Utc::now().to_rfc3339(),
        last_used_at: None,
        total_calls: 0,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cache_creation_tokens: 0,
        total_cache_read_tokens: 0,
        total_credits: 0.0,
        group: group.filter(|value| !value.trim().is_empty()),
        response_mode,
        is_system: false,
    };
    inner.by_key.insert(plaintext, id);
    inner.entries.insert(id, entry.clone());
    self.save_locked(&inner);
    entry
}
~~~

在 ensure_system_key 创建的新 ClientKey 中显式写入 response_mode: ClientResponseMode::Detection。迁移已有条目时移动整个 ClientKey，不覆盖其 response_mode。

- [x] **Step 5：把鉴权改成一次锁内返回完整快照**

用以下实现替换 verify_and_touch 主体，并保留返回 ID 的兼容包装：

~~~rust
pub fn verify_and_touch_context(&self, presented: &str) -> Option<AuthorizedClientKey> {
    if !presented.starts_with(CLIENT_KEY_PREFIX) {
        return None;
    }
    let mut inner = self.inner.write();
    let mut hit_id = None;
    for (id, key) in inner.entries.iter() {
        if key.disabled {
            continue;
        }
        if key.key.as_bytes().ct_eq(presented.as_bytes()).into() {
            hit_id = Some(*id);
        }
    }
    let id = hit_id?;
    let entry = inner.entries.get_mut(&id)?;
    entry.total_calls += 1;
    entry.last_used_at = Some(Utc::now().to_rfc3339());
    Some(AuthorizedClientKey {
        id,
        group: entry.group.clone(),
        response_mode: entry.response_mode,
    })
}

pub fn verify_and_touch(&self, presented: &str) -> Option<u64> {
    self.verify_and_touch_context(presented)
        .map(|authorized| authorized.id)
}
~~~

不要删除 group_of；其它管理逻辑仍可使用它，但数据面中间件将在 Task 3 停止分两次读取。

- [x] **Step 6：运行 Task 1 测试和原客户端 Key 测试**

Run:

~~~powershell
cargo test -j 1 response_mode_ -- --nocapture
cargo test -j 1 admin::client_keys::tests -- --nocapture
~~~

Expected: 新增测试全部 PASS；原有创建、禁用、轮换、系统 Key 测试全部 PASS。

- [x] **Step 7：提交领域模型**

~~~powershell
git add -- src/admin/client_keys.rs
git diff --cached --check
git commit -m "feat(key): 增加双回复模式"
~~~

---

### Task 2：扩展 Admin API 并保证模式编辑落盘一致

**Files:**
- Modify: src/admin/client_keys.rs:127-140
- Modify: src/admin/client_keys.rs:313-340
- Modify: src/admin/types.rs:1099-1161
- Modify: src/admin/types.rs:1660-end
- Modify: src/admin/handlers.rs:19
- Modify: src/admin/handlers.rs:1097-1224
- Test: src/admin/client_keys.rs:562-end
- Test: src/admin/types.rs:1660-end
- Test: src/admin/handlers.rs:2214-end

- [x] **Step 1：写 API 字段和模式校验 RED 测试**

在 src/admin/types.rs 的 tests 模块加入：

~~~rust
#[test]
fn client_key_requests_use_camel_case_response_mode() {
    let create: CreateClientKeyRequest = serde_json::from_value(serde_json::json!({
        "name": "native",
        "responseMode": "kiro_native"
    }))
    .unwrap();
    assert_eq!(create.response_mode.as_deref(), Some("kiro_native"));

    let update: UpdateClientKeyRequest = serde_json::from_value(serde_json::json!({
        "responseMode": "detection"
    }))
    .unwrap();
    assert_eq!(update.response_mode.as_deref(), Some("detection"));
}
~~~

在 src/admin/handlers.rs 的 tests 模块加入：

~~~rust
#[test]
fn client_response_mode_parser_rejects_unknown_values() {
    assert_eq!(
        parse_client_response_mode(Some("detection")).unwrap(),
        Some(ClientResponseMode::Detection)
    );
    assert_eq!(
        parse_client_response_mode(Some("kiro_native")).unwrap(),
        Some(ClientResponseMode::KiroNative)
    );
    assert!(parse_client_response_mode(Some("native")).is_err());
    assert_eq!(parse_client_response_mode(None).unwrap(), None);
}
~~~

在 src/admin/client_keys.rs 的 tests 模块加入落盘回滚测试：

~~~rust
#[test]
fn response_mode_failed_persistence_rolls_back_update() {
    let root = std::env::temp_dir().join(format!(
        "kiro-rs-response-mode-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("client_api_keys.json");
    let manager = ClientKeyManager::load(&path).unwrap();
    let entry = manager.create("key".into(), None, None);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    let result = manager.update_meta(
        entry.id,
        None,
        None,
        None,
        Some(ClientResponseMode::KiroNative),
    );
    assert!(result.is_err());
    assert_eq!(
        manager.list()[0].response_mode,
        ClientResponseMode::Detection
    );

    std::fs::remove_dir_all(&root).unwrap();
}
~~~

- [x] **Step 2：运行 API 定向测试并确认 RED**

Run:

~~~powershell
cargo test -j 1 client_key_requests_use_camel_case_response_mode -- --nocapture
cargo test -j 1 client_response_mode_parser_rejects_unknown_values -- --nocapture
cargo test -j 1 response_mode_failed_persistence_rolls_back_update -- --nocapture
~~~

Expected: 请求字段、解析器和 update_meta 新签名不存在，测试编译失败。

- [x] **Step 3：扩展 Admin 请求与响应类型**

在 src/admin/types.rs 引入模式：

~~~rust
use super::client_keys::ClientResponseMode;
~~~

给 ClientKeyItem 增加：

~~~rust
pub response_mode: ClientResponseMode,
~~~

给 CreateClientKeyRequest 和 UpdateClientKeyRequest 分别增加严格解析前的原始字符串：

~~~rust
#[serde(default)]
pub response_mode: Option<String>,
~~~

给 CreateClientKeyResponse 增加：

~~~rust
pub response_mode: ClientResponseMode,
~~~

响应使用枚举确保只会输出 detection 或 kiro_native；请求使用 String，确保 handler 能把未知值稳定映射为 400，而不是由 Json extractor 返回不一致的错误格式。

- [x] **Step 4：为编辑路径增加可回滚的持久化写入**

在 ClientKeyManager 中把实际序列化写入拆成可返回错误的函数，保留原有告警包装供旧路径使用：

~~~rust
fn try_save_locked(&self, inner: &Inner) -> anyhow::Result<()> {
    let Some(path) = &self.path else {
        return Ok(());
    };
    let mut list: Vec<&ClientKey> = inner.entries.values().collect();
    list.sort_by_key(|key| key.id);
    let json = serde_json::to_string_pretty(&list)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn save_locked(&self, inner: &Inner) {
    if let Err(error) = self.try_save_locked(inner) {
        tracing::warn!("写入客户端 Key 文件失败: {}", error);
    }
}
~~~

将 update_meta 改为：

~~~rust
pub fn update_meta(
    &self,
    id: u64,
    name: Option<String>,
    description: Option<Option<String>>,
    group: Option<Option<String>>,
    response_mode: Option<ClientResponseMode>,
) -> anyhow::Result<bool> {
    let mut inner = self.inner.write();
    let Some(previous) = inner.entries.get(&id).cloned() else {
        return Ok(false);
    };
    {
        let entry = inner.entries.get_mut(&id).expect("entry existed above");
        if let Some(value) = name {
            entry.name = value;
        }
        if let Some(value) = description {
            entry.description = value;
        }
        if let Some(value) = group {
            entry.group = value.filter(|item| !item.trim().is_empty());
        }
        if let Some(value) = response_mode {
            entry.response_mode = value;
        }
    }
    if let Err(error) = self.try_save_locked(&inner) {
        inner.entries.insert(id, previous);
        return Err(error);
    }
    Ok(true)
}
~~~

这保证模式切换失败时内存与磁盘都维持旧值。创建路径仍保持现有错误策略，本任务不扩大为整个 Key 管理器的持久化重构。

- [x] **Step 5：实现 handler 的严格解析和错误响应**

在 src/admin/handlers.rs 的 client_keys import 中加入 ClientResponseMode，并加入纯解析函数：

~~~rust
fn parse_client_response_mode(
    value: Option<&str>,
) -> Result<Option<ClientResponseMode>, &'static str> {
    value.map(str::parse).transpose()
}
~~~

create_client_key 在名称校验后解析模式：

~~~rust
let response_mode = match parse_client_response_mode(payload.response_mode.as_deref()) {
    Ok(Some(value)) => value,
    Ok(None) => ClientResponseMode::Detection,
    Err(message) => {
        return (
            StatusCode::BAD_REQUEST,
            Json(super::types::AdminErrorResponse::invalid_request(message)),
        )
            .into_response();
    }
};
let entry = state.client_keys.create_with_mode(
    name.to_string(),
    payload
        .description
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()),
    payload
        .group
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()),
    response_mode,
);
~~~

key_to_item、create_client_key 构造的 CreateClientKeyResponse，以及 rotate_client_key 构造的 CreateClientKeyResponse 都必须加入：

~~~rust
response_mode: entry.response_mode,
~~~

这样创建和轮换明文后的弹窗都显示后端真实模式。

update_client_key 先解析可选模式，再调用新 update_meta。结果必须三分支处理：

~~~rust
let response_mode = match parse_client_response_mode(payload.response_mode.as_deref()) {
    Ok(value) => value,
    Err(message) => {
        return (
            StatusCode::BAD_REQUEST,
            Json(super::types::AdminErrorResponse::invalid_request(message)),
        )
            .into_response();
    }
};

match state.client_keys.update_meta(
    id,
    payload.name,
    description,
    group,
    response_mode,
) {
    Ok(true) => Json(SuccessResponse::new(format!("Key #{} 已更新", id))).into_response(),
    Ok(false) => (
        StatusCode::NOT_FOUND,
        Json(super::types::AdminErrorResponse::not_found(format!(
            "Key #{} 不存在",
            id
        ))),
    )
        .into_response(),
    Err(error) => {
        tracing::error!(key_id = id, %error, "持久化客户端 Key 更新失败");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(super::types::AdminErrorResponse::internal_error(
                "Key 更新未保存，请稍后重试",
            )),
        )
            .into_response()
    }
}
~~~

internal_error 是 src/admin/types.rs:1419 的现有构造器。不得把 std::io::Error 原文返回给客户端。

- [x] **Step 6：运行 Admin API 与客户端 Key 回归**

Run:

~~~powershell
cargo test -j 1 client_response_mode_ -- --nocapture
cargo test -j 1 response_mode_ -- --nocapture
cargo test -j 1 admin::types::tests -- --nocapture
cargo test -j 1 admin::client_keys::tests -- --nocapture
~~~

Expected: 新旧测试全部 PASS；落盘失败测试证明内存模式回滚。

- [x] **Step 7：提交 Admin API**

~~~powershell
git add -- src/admin/client_keys.rs src/admin/types.rs src/admin/handlers.rs
git diff --cached --check
git commit -m "feat(admin): 支持选择 Key 回复模式"
~~~

---

### Task 3：把模式注入请求并分流检测短路与身份归一化

**Files:**
- Modify: src/anthropic/middleware.rs:23-32
- Modify: src/anthropic/middleware.rs:138-170
- Modify: src/anthropic/handlers.rs:36-44
- Modify: src/anthropic/handlers.rs:1290-1711
- Modify: src/anthropic/handlers.rs:2009-2447
- Modify: src/anthropic/handlers.rs:2365-2630
- Modify: src/anthropic/handlers.rs:3300-3451
- Modify: src/anthropic/handlers.rs:3452-3963
- Modify: src/anthropic/handlers.rs:3991-4490
- Test: src/anthropic/middleware.rs
- Test: src/anthropic/handlers.rs:5000-end

- [x] **Step 1：写检测闭包和身份有效条件 RED 测试**

在 handlers.rs 测试模块加入：

~~~rust
#[test]
fn response_mode_native_does_not_execute_detection_shortcut() {
    let called = std::cell::Cell::new(false);
    let result = detection_only(ClientResponseMode::KiroNative, || {
        called.set(true);
        Some("local")
    });
    assert_eq!(result, None);
    assert!(!called.get());
}

#[test]
fn response_mode_detection_executes_detection_shortcut() {
    let called = std::cell::Cell::new(false);
    let result = detection_only(ClientResponseMode::Detection, || {
        called.set(true);
        Some("local")
    });
    assert_eq!(result, Some("local"));
    assert!(called.get());
}

#[test]
fn response_mode_identity_requires_global_and_key_opt_in() {
    assert!(effective_identity_normalization(
        true,
        ClientResponseMode::Detection
    ));
    assert!(!effective_identity_normalization(
        false,
        ClientResponseMode::Detection
    ));
    assert!(!effective_identity_normalization(
        true,
        ClientResponseMode::KiroNative
    ));
}
~~~

在 middleware.rs 测试模块加入一个鉴权上下文测试，使用现有 AppState::new 和 Axum from_fn_with_state 模式：

~~~rust
#[tokio::test]
async fn response_mode_middleware_injects_native_snapshot() {
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware;
    use axum::routing::get;
    use axum::{Extension, Json, Router};
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn show_mode(Extension(context): Extension<KeyContext>) -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "keyId": context.key_id,
            "responseMode": context.response_mode.as_str()
        }))
    }

    let keys = Arc::new(crate::admin::ClientKeyManager::new());
    let key = keys.create_with_mode(
        "native".into(),
        None,
        None,
        crate::admin::client_keys::ClientResponseMode::KiroNative,
    );
    let state = AppState::new(
        false,
        crate::model::config::ToolCompatibilityMode::ClaudeCode,
    )
    .with_usage(Some(keys), None, None);
    let app = Router::new()
        .route("/", get(show_mode))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header("x-api-key", key.key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["responseMode"], "kiro_native");
}
~~~

- [x] **Step 2：运行行为定向测试并确认 RED**

Run:

~~~powershell
cargo test -j 1 response_mode_native_does_not_execute_detection_shortcut -- --nocapture
cargo test -j 1 response_mode_middleware_injects_native_snapshot -- --nocapture
~~~

Expected: detection_only、effective_identity_normalization 或 KeyContext.response_mode 不存在。

- [x] **Step 3：让中间件消费原子鉴权快照**

给 KeyContext 增加：

~~~rust
pub response_mode: crate::admin::client_keys::ClientResponseMode,
~~~

把 auth_middleware 中 verify_and_touch 加 group_of 的两次读取替换为：

~~~rust
if let Some(manager) = &state.client_keys {
    if let Some(authorized) = manager.verify_and_touch_context(&presented) {
        request.extensions_mut().insert(KeyContext {
            key_id: authorized.id,
            group: authorized.group,
            key_source: TraceKeySource::ClientKey,
            response_mode: authorized.response_mode,
        });
        return next.run(request).await;
    }
}
~~~

修复测试中手工构造 KeyContext 的编译错误，所有旧测试使用 ClientResponseMode::Detection，保持原行为。

- [x] **Step 4：增加两个纯策略帮助函数**

在 handlers.rs 顶部 helper 区域加入：

~~~rust
use crate::admin::client_keys::ClientResponseMode;

fn detection_only<T>(
    mode: ClientResponseMode,
    action: impl FnOnce() -> Option<T>,
) -> Option<T> {
    mode.allows_detection_shortcuts()
        .then(action)
        .flatten()
}

fn effective_identity_normalization(
    globally_enabled: bool,
    mode: ClientResponseMode,
) -> bool {
    globally_enabled && mode.allows_identity_normalization()
}
~~~

不得让 detection_only 接收 prompt、模型名或探针关键字；它只负责模式能力门控。

- [x] **Step 5：在两个消息入口门控五类检测型本地回复**

在 post_messages 和 post_messages_cc 中，把以下调用分别包进 detection_only：

~~~rust
if let Some(response) = detection_only(key_ctx.response_mode, || {
    try_local_model_profile_response(&state, provider.as_ref(), &payload, &hook)
}) {
    finalize_immediate_response(&tracer, &response, "model_profile_error");
    return response;
}

if let Some(response) = detection_only(key_ctx.response_mode, || {
    try_local_exact_system_response(
        &state,
        provider.as_ref(),
        &payload,
        &hook,
        state.tool_compatibility_mode,
    )
}) {
    finalize_immediate_response(&tracer, &response, "exact_system_error");
    return response;
}

if let Some(response) = detection_only(key_ctx.response_mode, || {
    try_local_exact_user_response(
        &state,
        provider.as_ref(),
        &payload,
        &hook,
        state.tool_compatibility_mode,
    )
}) {
    finalize_immediate_response(&tracer, &response, "exact_user_error");
    return response;
}
~~~

try_local_ping_response 必须留在 detection_only 外部，以保留两种模式的 1 秒健康响应。

PDF 展开完成后，只门控 identifier 短路：

~~~rust
if let Some(response) = detection_only(key_ctx.response_mode, || {
    try_local_document_identifier_response(
        &state,
        provider.as_ref(),
        &payload,
        &document_expansion,
        &hook,
        state.tool_compatibility_mode,
    )
}) {
    finalize_immediate_response(&tracer, &response, "document_identifier_error");
    return response;
}
~~~

expand_pdf_documents 必须继续在两种模式执行。strict_json_candidate、WebSearch 和工具转换不得放入 detection_only。

- [x] **Step 6：把有效身份开关传入所有流式和非流式执行路径**

在每个消息入口完成 provider 解析后计算一次：

~~~rust
let identity_normalization = effective_identity_normalization(
    provider.identity_normalization(),
    key_ctx.response_mode,
);
~~~

给 handle_stream_request、create_early_sse_stream、handle_stream_request_buffered 和 handle_non_stream_request 增加 identity_normalization: bool 参数。所有调用点传递同一个局部变量。

构造 StreamAttemptSetup 时禁止再次读取 provider.identity_normalization，改为：

~~~rust
identity_normalization,
~~~

非流式收集完成后的身份处理改为：

~~~rust
let text_content = crate::kiro::model::events::strip_tool_use_xml_leaks(&text_content);
let text_content = if identity_normalization {
    super::identity::normalize_identity_text(&text_content)
} else {
    text_content
};
~~~

普通流、早期 SSE 流和缓冲 SSE 流仍通过 StreamAttemptSetup.new_context 或 new_buffered_context 启用 IdentityStreamFilter；KiroNative 时字段为 false，过滤器根本不创建，避免跨 chunk 等待。

- [x] **Step 7：补充真实本地短路函数的模式回归**

把 local_exact_system_output 和 local_exact_user_answer 的单元测试各加一个 native 外层用例，直接使用 detection_only 包装真实函数：

~~~rust
#[test]
fn response_mode_native_bypasses_real_exact_system_output() {
    let request: MessagesRequest = serde_json::from_value(serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hello"}],
        "system": "Return exactly the single word 'READY' and nothing else. No explanation."
    }))
    .unwrap();
    let output = detection_only(ClientResponseMode::KiroNative, || {
        local_exact_system_output(
            &request,
            crate::model::config::ToolCompatibilityMode::Raw,
        )
    });
    assert!(output.is_none());
}

#[test]
fn response_mode_detection_keeps_real_exact_system_output() {
    let request: MessagesRequest = serde_json::from_value(serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hello"}],
        "system": "Return exactly the single word 'READY' and nothing else. No explanation."
    }))
    .unwrap();
    let output = detection_only(ClientResponseMode::Detection, || {
        local_exact_system_output(
            &request,
            crate::model::config::ToolCompatibilityMode::Raw,
        )
    });
    assert_eq!(output.unwrap().as_str(), "READY");
}
~~~

这两个测试使用与 handlers.rs:5603 现有测试相同的真实 MessagesRequest JSON 和 local_exact_system_output，不创建只返回固定结果的假函数。

- [x] **Step 8：运行 Anthropic 层定向和路由测试**

Run:

~~~powershell
cargo test -j 1 response_mode_ -- --nocapture
cargo test -j 1 anthropic::middleware -- --nocapture
cargo test -j 1 anthropic::handlers -- --nocapture
cargo test -j 1 anthropic::stream -- --nocapture
cargo test -j 1 anthropic::router -- --nocapture
~~~

Expected: 全部 PASS。旧 Detection Key 的精确 system、identity、PDF、tool 和 SSE 测试结果不变。

- [x] **Step 9：提交请求行为分流**

~~~powershell
git add -- src/anthropic/middleware.rs src/anthropic/handlers.rs
git diff --cached --check
git commit -m "feat(api): 按 Key 分流原生回复"
~~~

---

### Task 4：把请求模式写入 trace 和错误快照

**Files:**
- Modify: src/admin/trace_db.rs:80-160
- Modify: src/admin/trace_db.rs:280-370
- Modify: src/admin/trace_db.rs:548-595
- Modify: src/admin/trace_db.rs:790-830
- Modify: src/admin/trace_db.rs:832-end
- Modify: src/anthropic/handlers.rs:130-220
- Modify: src/anthropic/handlers.rs:320-372
- Modify: src/anthropic/error_snapshot.rs:224-235
- Modify: src/anthropic/error_snapshot.rs:354-409
- Modify: src/anthropic/error_snapshot.rs:665-686
- Modify: src/admin/error_snapshot_db.rs:13
- Modify: src/admin/error_snapshot_db.rs:71-93
- Modify: src/admin/error_snapshot_db.rs:159-190
- Modify: src/admin/error_snapshot_db.rs:344-435
- Modify: src/admin/error_snapshot_db.rs:960-1035
- Modify: src/admin/error_snapshot_db.rs:1180-1232
- Test: src/admin/trace_db.rs:832-end
- Test: src/admin/error_snapshot_db.rs:1234-end

- [x] **Step 1：写 trace 与快照模式往返 RED 测试**

给 trace_db.rs 的 sample() 增加 response_mode: ClientResponseMode::KiroNative，并在 insert_and_query_roundtrip 中断言：

~~~rust
assert_eq!(
    out[0].response_mode,
    ClientResponseMode::KiroNative
);
assert_eq!(
    serde_json::to_value(&out[0]).unwrap()["responseMode"],
    "kiro_native"
);
~~~

新增旧 trace 表迁移测试：

~~~rust
#[test]
fn response_mode_migrates_old_trace_rows_to_detection() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE traces (
            trace_id TEXT PRIMARY KEY,
            ts TEXT NOT NULL,
            ts_epoch INTEGER NOT NULL,
            key_id INTEGER NOT NULL,
            model TEXT NOT NULL,
            is_stream INTEGER NOT NULL,
            final_status TEXT NOT NULL,
            final_credential_id INTEGER NOT NULL,
            total_attempts INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL
        );
        CREATE TABLE trace_attempts (
            trace_id TEXT NOT NULL,
            attempt INTEGER NOT NULL,
            credential_id INTEGER NOT NULL,
            endpoint TEXT NOT NULL,
            http_status INTEGER,
            outcome TEXT NOT NULL,
            error_snippet TEXT,
            duration_ms INTEGER NOT NULL,
            PRIMARY KEY (trace_id, attempt)
        );
        INSERT INTO traces (
            trace_id, ts, ts_epoch, key_id, model, is_stream,
            final_status, final_credential_id, total_attempts, duration_ms
        ) VALUES ('legacy', '2026-07-15T00:00:00Z', 1, 1, 'm', 0, 'success', 1, 0, 1);",
    )
    .unwrap();
    TraceStore::migrate(&conn).unwrap();
    let value: String = conn
        .query_row(
            "SELECT response_mode FROM traces WHERE trace_id = 'legacy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value, "detection");
}
~~~

在 error_snapshot_db.rs 的快照往返测试中把 sample_write.response_mode 设为 KiroNative，并断言 summary.response_mode 相同。再加入旧 v1 schema 迁移测试：

~~~rust
#[test]
fn response_mode_migrates_v1_snapshot_schema_to_detection() {
    let conn = Connection::open_in_memory().unwrap();
    let legacy_schema = SCHEMA.replace(
        "  response_mode TEXT NOT NULL DEFAULT 'detection',\n",
        "",
    );
    assert_ne!(legacy_schema, SCHEMA);
    conn.execute_batch(&legacy_schema).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    initialize_connection(&conn, false).unwrap();

    let columns = conn
        .prepare("PRAGMA table_info(error_snapshots)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(columns.iter().any(|name| name == "response_mode"));
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
}
~~~

- [x] **Step 2：运行持久化测试并确认 RED**

Run:

~~~powershell
cargo test -j 1 response_mode_migrates_old_trace_rows_to_detection -- --nocapture
cargo test -j 1 admin::error_snapshot_db::tests -- --nocapture
~~~

Expected: TraceRecord、SnapshotWrite 或 SnapshotSummary 缺少 response_mode，编译失败。

- [x] **Step 3：扩展 trace schema、写入和读取**

在 trace_db.rs 引入 ClientResponseMode，给 TraceRecord 在 key_source 后增加：

~~~rust
#[serde(default)]
pub response_mode: ClientResponseMode,
~~~

SCHEMA 的 traces 表在 key_source 后增加：

~~~sql
response_mode TEXT NOT NULL DEFAULT 'detection',
~~~

migrate 的 columns 数组增加：

~~~rust
("response_mode", "TEXT NOT NULL DEFAULT 'detection'"),
~~~

INSERT 列表在 key_source 后加入 response_mode，参数使用 rec.response_mode.as_str()，并顺延占位符。SELECT 也在 key_source 后加入 response_mode，row 映射使用：

~~~rust
response_mode: row
    .get::<_, String>(4)?
    .parse()
    .unwrap_or(ClientResponseMode::Detection),
~~~

因为新增列占用索引 4，model 及后续 row.get 索引全部加 1。不要只修改 SELECT 而漏掉 INSERT 参数数量。

给 RequestTracer 增加 response_mode 字段，在 new() 中从 options.key_ctx.response_mode 复制，在 finalize() 构造 TraceRecord 时写入。

- [x] **Step 4：升级错误快照 schema 到 v2**

在 error_snapshot_db.rs：

~~~rust
const SCHEMA_VERSION: i64 = 2;
~~~

给 SnapshotWrite 和 SnapshotSummary 在 key_source 后增加：

~~~rust
pub response_mode: crate::admin::client_keys::ClientResponseMode,
~~~

error_snapshots 表在 key_source 后加入：

~~~sql
response_mode TEXT NOT NULL DEFAULT 'detection',
~~~

在 initialize_connection 执行 SCHEMA 后检查列是否存在，避免新库重复 ALTER：

~~~rust
fn ensure_response_mode_column(conn: &Connection) -> rusqlite::Result<()> {
    let mut statement = conn.prepare("PRAGMA table_info(error_snapshots)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|name| name == "response_mode") {
        conn.execute_batch(
            "ALTER TABLE error_snapshots
             ADD COLUMN response_mode TEXT NOT NULL DEFAULT 'detection';",
        )?;
    }
    Ok(())
}
~~~

调用顺序必须是 conn.execute_batch(SCHEMA)、ensure_response_mode_column、更新 user_version。新库由 SCHEMA 创建列，旧 v1 库由 ALTER 添加并自动回填 detection。

INSERT 和 summary_select 在 key_source 后加入 response_mode。summary_from_row 使用严格位置映射，并把未知磁盘值降级为 Detection：

~~~rust
response_mode: row
    .get::<_, String>(7)?
    .parse()
    .unwrap_or(crate::admin::client_keys::ClientResponseMode::Detection),
~~~

插入 response_mode 后，final_credential_id 和后续索引全部加 1。

- [x] **Step 5：从 KeyContext 贯穿错误快照**

给 ErrorSnapshotContext 增加 response_mode 字段，在 new() 中设置：

~~~rust
response_mode: key.response_mode,
~~~

构造 SnapshotWrite 时增加：

~~~rust
response_mode: self.response_mode,
~~~

RequestTracer 和 ErrorSnapshotContext 都只保存鉴权快照，不在 finalize 时重新查询 ClientKeyManager。

- [x] **Step 6：运行 trace、快照和 handler 回归**

Run:

~~~powershell
cargo test -j 1 admin::trace_db::tests -- --nocapture
cargo test -j 1 admin::error_snapshot_db::tests -- --nocapture
cargo test -j 1 anthropic::error_snapshot -- --nocapture
cargo test -j 1 anthropic::handlers -- --nocapture
~~~

Expected: 全部 PASS；旧 trace 和快照数据库迁移为 detection；新记录往返 kiro_native。

- [x] **Step 7：提交可观测性**

~~~powershell
git add -- src/admin/trace_db.rs src/admin/error_snapshot_db.rs src/anthropic/error_snapshot.rs src/anthropic/handlers.rs
git diff --cached --check
git commit -m "feat(trace): 记录请求回复模式"
~~~

---

### Task 5：在管理端创建、编辑和展示回复模式

**Files:**
- Create: admin-ui/src/lib/client-key-response-mode.ts
- Create: admin-ui/src/lib/client-key-response-mode.test.ts
- Modify: admin-ui/src/types/api.ts:432-477
- Modify: admin-ui/src/components/client-keys-page.tsx:1-250
- Modify: admin-ui/src/components/client-keys-page.tsx:250-end

- [x] **Step 1：写前端模式纯函数 RED 测试**

创建 admin-ui/src/lib/client-key-response-mode.test.ts：

~~~typescript
import { describe, expect, test } from 'bun:test'
import {
  DEFAULT_CLIENT_RESPONSE_MODE,
  responseModeDescription,
  responseModeLabel,
  responseModeSwitchWarning,
} from './client-key-response-mode'

describe('client key response mode', () => {
  test('defaults new keys to detection', () => {
    expect(DEFAULT_CLIENT_RESPONSE_MODE).toBe('detection')
  })

  test('renders stable labels and descriptions', () => {
    expect(responseModeLabel('detection')).toBe('Claude 兼容')
    expect(responseModeLabel('kiro_native')).toBe('Kiro 原生')
    expect(responseModeDescription('kiro_native')).toContain('保留工具')
    expect(responseModeDescription('kiro_native')).toContain('Kiro/AWS')
  })

  test('only warns when mode changes', () => {
    expect(responseModeSwitchWarning('detection', 'detection')).toBeNull()
    expect(responseModeSwitchWarning('detection', 'kiro_native')).toContain('检测站得分')
    expect(responseModeSwitchWarning('kiro_native', 'detection')).toContain('Claude/Anthropic')
  })
})
~~~

- [x] **Step 2：运行前端测试并确认 RED**

Run:

~~~powershell
Set-Location admin-ui
bun test src/lib/client-key-response-mode.test.ts
~~~

Expected: FAIL，模块 client-key-response-mode 不存在。

- [x] **Step 3：实现前端模式领域工具和 API 类型**

创建 admin-ui/src/lib/client-key-response-mode.ts：

~~~typescript
import type { ClientResponseMode } from '@/types/api'

export const DEFAULT_CLIENT_RESPONSE_MODE: ClientResponseMode = 'detection'

export function responseModeLabel(mode: ClientResponseMode): string {
  return mode === 'kiro_native' ? 'Kiro 原生' : 'Claude 兼容'
}

export function responseModeDescription(mode: ClientResponseMode): string {
  return mode === 'kiro_native'
    ? '保留工具、重试、SSE、缓存与计费兼容，助手保留 Kiro/AWS 原始身份。'
    : '启用 Claude/Anthropic 身份归一化和检测型确定性回复。'
}

export function responseModeSwitchWarning(
  before: ClientResponseMode,
  after: ClientResponseMode,
): string | null {
  if (before === after) return null
  return after === 'kiro_native'
    ? '后续回复可能出现 Kiro/AWS 身份，检测站得分可能下降；正在进行的请求不受影响。'
    : '后续助手文本可能归一化为 Claude/Anthropic；正在进行的请求不受影响。'
}
~~~

在 admin-ui/src/types/api.ts 增加并应用以下类型：

~~~typescript
export type ClientResponseMode = 'detection' | 'kiro_native'
~~~

ClientKeyItem、CreateClientKeyResponse、TraceRecord 和 ErrorSnapshotSummary 增加 responseMode: ClientResponseMode；CreateClientKeyRequest 和 UpdateClientKeyRequest 增加 responseMode?: ClientResponseMode。TraceRecord 与 ErrorSnapshotSummary 的字段是 Task 4 后端可观测性的类型对齐，不在本任务增加新的筛选参数。

- [x] **Step 4：给创建和编辑表单增加受控状态**

在 client-keys-page.tsx 引入 Select 组件、ClientResponseMode 和模式工具。新增状态：

~~~typescript
const [createResponseMode, setCreateResponseMode] = useState<ClientResponseMode>(
  DEFAULT_CLIENT_RESPONSE_MODE,
)
const [editResponseMode, setEditResponseMode] = useState<ClientResponseMode>(
  DEFAULT_CLIENT_RESPONSE_MODE,
)
~~~

handleCreate 请求增加 responseMode: createResponseMode，创建成功后重置为 DEFAULT_CLIENT_RESPONSE_MODE。

startEdit 增加：

~~~typescript
setEditResponseMode(item.responseMode)
~~~

handleEditSave 请求增加：

~~~typescript
responseMode: editResponseMode,
~~~

编辑成功 toast 前读取警告：

~~~typescript
const warning = responseModeSwitchWarning(
  editTarget.responseMode,
  editResponseMode,
)
await updateKey.mutateAsync({
  id: editTarget.id,
  req: {
    name: editName.trim(),
    description: editDesc.trim(),
    group: editGroup.trim(),
    responseMode: editResponseMode,
  },
})
if (warning) {
  toast.warning(warning)
} else {
  toast.success('已更新')
}
~~~

不得使用乐观更新；继续依赖 mutation 成功后 invalidateQueries，保证列表显示后端真实保存值。

- [x] **Step 5：增加模式选择器和列表徽标**

在创建和编辑表单中加入同一结构的 Select，创建绑定 createResponseMode，编辑绑定 editResponseMode：

~~~tsx
<div>
  <label className="text-[12px] text-muted-foreground">回复模式</label>
  <Select
    value={createResponseMode}
    onValueChange={(value) => setCreateResponseMode(value as ClientResponseMode)}
    disabled={createKey.isPending}
  >
    <SelectTrigger>
      <SelectValue />
    </SelectTrigger>
    <SelectContent>
      <SelectItem value="detection">Claude 兼容</SelectItem>
      <SelectItem value="kiro_native">Kiro 原生</SelectItem>
    </SelectContent>
  </Select>
  <p className="mt-1 text-[11px] text-muted-foreground">
    {responseModeDescription(createResponseMode)}
  </p>
</div>
~~~

编辑版本只把 createResponseMode/createKey 替换为 editResponseMode/updateKey。

在 Key 表格的“分组”和“状态”之间增加“回复模式”列，每行显示：

~~~tsx
<td className="px-4 py-3">
  <Badge variant={k.responseMode === 'kiro_native' ? 'outline' : 'secondary'}>
    {responseModeLabel(k.responseMode)}
  </Badge>
</td>
~~~

同步提高 table 的 min-width，避免新列压缩名称和操作区。创建后的明文对话框增加实际模式标签，直接读取 createdKey.responseMode。

- [x] **Step 6：运行前端测试、类型检查和生产构建**

Run:

~~~powershell
Set-Location admin-ui
bun test
bun run build
~~~

Expected: 所有 Bun 测试 PASS；tsc 和 Vite build 退出码 0。

- [x] **Step 7：提交管理端**

~~~powershell
Set-Location ..
git add -- admin-ui/src/types/api.ts admin-ui/src/lib/client-key-response-mode.ts admin-ui/src/lib/client-key-response-mode.test.ts admin-ui/src/components/client-keys-page.tsx
git diff --cached --check
git commit -m "feat(ui): 增加 Key 回复模式选择"
~~~

---

### Task 6：完成双模式回归、客户影响核对和隔离环境验收

**Files:**
- Modify: docs/superpowers/specs/2026-07-15-client-key-response-profiles-design.md
- Modify: docs/superpowers/plans/2026-07-15-client-key-response-profiles.md
- Test: src/bin/anthropic_probe.rs
- Test: admin-ui/src

- [x] **Step 1：执行格式、编译和完整自动测试**

本机资源紧张，Rust 命令固定使用单任务：

~~~powershell
cargo fmt --all -- --check
cargo check -j 1 --all-targets
cargo test -j 1 --all-targets
Set-Location admin-ui
bun test
bun run build
Set-Location ..
~~~

Expected: 所有命令退出码 0。记录实际 Rust 和 Bun 测试数量；不得用旧二进制结果代替当前 HEAD 的结果。

- [x] **Step 2：检查模式边界没有误伤共享能力**

逐项用测试名或代码搜索确认：

~~~powershell
rg -n "detection_only|effective_identity_normalization|try_local_ping_response|strict_json_candidate|expand_pdf_documents|tool_compatibility_mode|early_stream_handshake" src/anthropic/handlers.rs
rg -n "response_mode|responseMode" src admin-ui/src
~~~

Expected:

- detection_only 只包围模型资料、exact system、exact user 和 PDF identifier；
- try_local_ping_response 在门控外；
- strict JSON、PDF 展开、工具转换和 early SSE 在门控外；
- provider.identity_normalization 只通过 effective_identity_normalization 与 Key 模式组合；
- response mode 不进入缓存 key、计费计算或模型目录排序。

- [ ] **Step 3：检查两个 Key 的持久化与切换**

在隔离 8991 管理端创建：

- detection-key：responseMode=detection；
- native-key：responseMode=kiro_native；
- 两把 Key 绑定同一账号分组。

重启 8991 容器后重新 GET /api/admin/client-keys，确认两种模式仍存在。编辑 native-key 名称但不改变模式，确认仍为 kiro_native；轮换其明文后确认模式和累计统计保留。

Expected: 旧 Key 全部显示 detection；新 Key 的模式重启后不丢失；切换不改变 Key ID、分组和统计。

- [ ] **Step 4：并排验证身份和检测短路**

对两把 Key 分别发送相同的流式与非流式请求：

~~~text
Who are you and who provides you? Reply with one short sentence.
~~~

再发送 context window、knowledge cutoff、固定 system 字面量、固定 user echo 和 PDF identifier 请求。

Expected:

- detection-key 维持 Claude/Anthropic 归一化和现有确定性回复；
- native-key 不把 Kiro/AWS 改成 Claude/Anthropic；
- native-key 的模型资料、exact system/user、PDF identifier 请求产生真实上游 attempt；
- native-key 流式响应无 IdentityStreamFilter 的跨 chunk 尾部等待。

若上游本身回答 Claude，不据此判失败；判定条件是 RS 不执行身份替换，可通过 trace response_mode、上游响应快照和最终文本对照确认。

- [ ] **Step 5：并排验证共享可靠性与 1 秒首响应**

对两把 Key 分别运行：

~~~powershell
if (-not $env:DETECTION_KEY) { throw "请先把临时 detection Key 放入 DETECTION_KEY 环境变量" }
if (-not $env:NATIVE_KEY) { throw "请先把临时 native Key 放入 NATIVE_KEY 环境变量" }
$env:ANTHROPIC_API_KEY = $env:DETECTION_KEY
cargo run -j 1 --bin anthropic_probe -- --base-url http://43.225.196.10:8991 --model claude-opus-4-8
$env:ANTHROPIC_API_KEY = $env:NATIVE_KEY
cargo run -j 1 --bin anthropic_probe -- --base-url http://43.225.196.10:8991 --model claude-opus-4-8
Remove-Item Env:ANTHROPIC_API_KEY
~~~

DETECTION_KEY 和 NATIVE_KEY 只在当前测试会话中设置，不把明文写入文档、Git、终端历史文件或日志。

Expected:

- 两种模式的 tool_choice、工具 Schema、thinking 降级、strict JSON、structured output、PDF 展开、UTF-8、空响应重试和普通 stream 均无新增失败；
- ping_health 与早期 SSE 首个客户端可见字节保持 1 秒目标；
- 相同非本地请求的 input/cache/output/credit 拆分一致；
- native-key 的检测型探针允许按设计失败，不把这些失败误判为共享能力回归。

- [ ] **Step 6：核对 trace 和错误快照**

在 8991 制造一条受控的无效模型请求或无效工具 Schema 请求，分别使用两种 Key。通过 Admin API/管理端确认 trace 与错误快照都显示本轮 responseMode，且没有明文 Key。

切换 Key 模式后再次查看旧记录。

Expected: 旧记录仍显示请求发生时的模式，不随当前 Key 设置变化；请求正文脱敏、Key 脱敏和快照容量策略保持原状。

- [x] **Step 7：更新设计与计划执行记录**

在设计文档末尾增加“实施结果”小节，写入：

- 最终提交列表；
- 实际测试数量；
- 8991 两把测试 Key 的 Key ID，不写明文；
- 首响应实测值；
- 已知上游限制；
- 明确未部署生产 8990。

把本计划已完成步骤勾选为 [x]，未执行的线上步骤保持 [ ] 并写清原因，不伪造完成状态。

- [x] **Step 8：完成前复核并提交文档**

~~~powershell
$patterns = @("TB" + "D", "TO" + "DO", "FIX" + "ME", "待" + "定", "稍后" + "填写")
rg -n -i ($patterns -join "|") docs/superpowers/specs/2026-07-15-client-key-response-profiles-design.md docs/superpowers/plans/2026-07-15-client-key-response-profiles.md
git diff --check
git status --short
git add -f -- docs/superpowers/specs/2026-07-15-client-key-response-profiles-design.md docs/superpowers/plans/2026-07-15-client-key-response-profiles.md
git diff --cached --check
git commit -m "docs(key): 记录双模式验收结果"
~~~

Expected: 占位符扫描无命中，diff check 无错误，只提交本功能文档。

---

## 最终验收清单

- [x] 旧 client_api_keys.json 无 responseMode 时全部加载为 detection。
- [x] 新建和编辑 API 只接受 detection/kiro_native，未知值返回 400。
- [x] Key 鉴权一次返回 ID、分组、模式快照；在途请求不受后台切换影响。
- [x] detection 保持现有身份归一化、模型资料和精确回复。
- [x] kiro_native 不执行身份归一化和检测型本地短路。
- [x] 两种模式共用工具、thinking、PDF 提取、WebSearch、strict JSON、重试、缓存和计费。
- [ ] 本地 ping 与早期 SSE 未被门控，NewAPI 首响应目标保持 1 秒。
- [x] trace 与错误快照保存请求发生时的 responseMode，不记录明文 Key。
- [x] 管理端显示后端实际模式，可创建和编辑，失败时不显示未保存值。
- [x] Rust、Anthropic probe、Bun 测试和前端生产构建全部通过。
- [ ] 只部署隔离 8991 验证，生产 8990 需要用户后续明确授权。
