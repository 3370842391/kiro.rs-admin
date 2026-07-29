#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        routing::{get, post, put},
    };
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn server(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router).into_future());
        format!("http://{address}/")
    }

    #[tokio::test]
    async fn reads_profile_stock_and_status_with_authentication() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        let app = Router::new()
            .route("/api/my/profile", get(move |request: axum::http::Request<axum::body::Body>| {
                let seen = seen_clone.clone();
                async move {
                    assert_eq!(
                        request.headers().get("user-agent").unwrap(),
                        "kiro-rs-key-supplier/1.0"
                    );
                    seen.lock().unwrap().push((request.uri().path().to_owned(), request.headers().get("x-api-key").unwrap().to_str().unwrap().to_owned(), request.headers().get("content-type").map(|v| v.to_str().unwrap().to_owned())));
                    axum::Json(serde_json::json!({"name":"demo","quota":10,"remaining":7,"used_quota":3,"webhook_url":"http://hook"}))
                }
            }))
            .route(
                "/api/my/stock",
                get(|request: axum::http::Request<axum::body::Body>| async move {
                    assert_eq!(request.headers().get("x-api-key").unwrap(), "secret");
                    axum::Json(serde_json::json!({"max": 9}))
                }),
            )
            .route(
                "/api/status",
                get(|request: axum::http::Request<axum::body::Body>| async move {
                    assert_eq!(request.headers().get("x-api-key").unwrap(), "secret");
                    axum::Json(serde_json::json!({"keys_active":2,"extra_state":"kept"}))
                }),
            );
        let client = SupplierClient::new(server(app).await, "secret").unwrap();
        assert_eq!(client.profile().await.unwrap().remaining, 7);
        assert_eq!(client.stock().await.unwrap().max, 9);
        let status = client.status().await.unwrap();
        assert_eq!(status.keys_active, 2);
        assert_eq!(status.keys_dead, 0);
        assert_eq!(status.extra["extra_state"], "kept");
        assert_eq!(seen.lock().unwrap()[0].1, "secret");
    }

    #[tokio::test]
    async fn retries_purchase_after_server_error_with_same_order_id() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = seen.clone();
        let app = Router::new().route("/api/my/purchase", post(move |request: axum::http::Request<axum::body::Body>| {
            let state = state.clone();
            async move {
                assert_eq!(request.headers().get("x-api-key").unwrap(), "secret");
                let content_type = request.headers().get("content-type").unwrap().to_str().unwrap().to_owned();
                let body = axum::body::to_bytes(request.into_body(), usize::MAX).await.unwrap();
                state.lock().unwrap().push((String::from_utf8(body.to_vec()).unwrap(), content_type));
                if state.lock().unwrap().len() < 2 { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "retry") } else { (axum::http::StatusCode::OK, r#"{"client_order_id":"0123456789abcdef0123456789abcdef","purchased":1,"remaining":2,"keys":[{"key":"ksk_good"}]}"#) }
            }
        }));
        let client = SupplierClient::new(server(app).await, "secret").unwrap();
        let result = client
            .purchase(1, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap();
        assert_eq!(result.purchased, 1);
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, requests[1].0);
        assert_eq!(requests[0].1, "application/json");
    }

    #[tokio::test]
    async fn does_not_retry_client_errors_and_validates_purchase_response() {
        let calls = Arc::new(Mutex::new(0));
        let calls_clone = calls.clone();
        let app = Router::new().route(
            "/api/my/purchase",
            post(move || {
                let calls = calls_clone.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    (axum::http::StatusCode::BAD_REQUEST, "ksk_bad secret")
                }
            }),
        );
        let client = SupplierClient::new(server(app).await, "secret").unwrap();
        let error = client
            .purchase(1, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap_err();
        assert_eq!(*calls.lock().unwrap(), 1);
        let text = error.to_string();
        assert!(!text.contains("ksk_bad") && !text.contains("secret"));
    }

    #[tokio::test]
    async fn validates_purchase_order_and_keys_and_supports_webhooks() {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let paths_clone = paths.clone();
        let paths_test = paths.clone();
        let app = Router::new()
            .route("/api/my/purchase", post(|| async { axum::Json(serde_json::json!({"client_order_id":"fedcba9876543210fedcba9876543210","purchased":1,"remaining":2,"keys":[{"key":"bad"}]})) }))
            .route("/api/my/webhook", put(move |request: axum::http::Request<axum::body::Body>| { let paths = paths_clone.clone(); async move { assert_eq!(request.headers().get("x-api-key").unwrap(), "secret"); assert_eq!(request.headers().get("content-type").unwrap(), "application/json"); paths.lock().unwrap().push(request.uri().path().to_owned()); axum::http::StatusCode::NO_CONTENT } }))
            .route("/api/my/webhook/test", post(move |request: axum::http::Request<axum::body::Body>| { let paths = paths_test.clone(); async move { assert_eq!(request.headers().get("x-api-key").unwrap(), "secret"); assert!(request.headers().get("content-type").is_none()); paths.lock().unwrap().push(request.uri().path().to_owned()); axum::http::StatusCode::NO_CONTENT } }));
        let client = SupplierClient::new(server(app).await, "secret").unwrap();
        assert!(
            client
                .purchase(1, "0123456789abcdef0123456789abcdef")
                .await
                .is_err()
        );
        client.register_webhook("http://hook").await.unwrap();
        client.test_webhook().await.unwrap();
        assert_eq!(
            *paths.lock().unwrap(),
            vec!["/api/my/webhook", "/api/my/webhook/test"]
        );
    }

    #[tokio::test]
    async fn kiro_app_uses_bearer_auth_and_openapi_endpoints() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let app = Router::new()
            .route(
                "/openapi/stock",
                get(|request: axum::http::Request<axum::body::Body>| async move {
                    assert_eq!(
                        request.headers().get("authorization").unwrap(),
                        "Bearer app-secret"
                    );
                    assert!(request.headers().get("x-api-key").is_none());
                    axum::Json(serde_json::json!({"availableKeys": 12, "keyPrice": 2.5}))
                }),
            )
            .route(
                "/openapi/balance",
                get(|| async { axum::Json(serde_json::json!({"balance": 480})) }),
            )
            .route(
                "/openapi/claim",
                post(move |body: axum::body::Bytes| {
                    let observed = observed.clone();
                    async move {
                        observed
                            .lock()
                            .unwrap()
                            .push(String::from_utf8(body.to_vec()).unwrap());
                        // 官方文档：claim 返回 {key, pointsCost, balance}。
                        axum::Json(serde_json::json!({
                            "key": "ksk_single", "pointsCost": 100, "balance": 900
                        }))
                    }
                }),
            );
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot.stock_available, Some(12));
        assert_eq!(snapshot.key_price, Some(2.5));
        assert_eq!(snapshot.balance, Some(480));
        // kiro-app 读不到 profile/status，也读不到对方登记的回调地址。
        assert!(snapshot.profile.is_none() && snapshot.status.is_none());
        assert!(snapshot.webhook_url.is_none());
        assert_eq!(client.available_stock().await.unwrap(), 12);

        // 取 1 个时对方返回 {key} 而不是 {keys:[...]}，也要能收下。
        let purchase = client
            .purchase(1, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap();
        assert_eq!(purchase.purchased, 1);
        // remaining 是扣费后余额，points_cost 是本次花掉的积分。
        assert_eq!(purchase.remaining, 900);
        assert_eq!(purchase.points_cost, Some(100));
        assert_eq!(seen.lock().unwrap()[0], r#"{"count":1}"#);
        assert!(!format!("{purchase:?}").contains("ksk_single"));
    }

    #[tokio::test]
    async fn kiro_app_claim_is_never_retried_and_rejects_empty_or_bad_keys() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/openapi/claim",
            post(move || {
                let observed = observed.clone();
                async move {
                    let mut calls = observed.lock().unwrap();
                    *calls += 1;
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        r#"{"error":{"type":"server_error","message":"boom"}}"#,
                    )
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        assert!(
            client
                .purchase(1, "0123456789abcdef0123456789abcdef")
                .await
                .is_err()
        );
        // claim 没有幂等键，5xx 也只能打一次。
        assert_eq!(*calls.lock().unwrap(), 1);

        // 一个 key 都没拿到、或者给多了，才算错。
        for body in [r#"{}"#, r#"{"keys":[]}"#, r#"{"keys":["  "]}"#, r#"{"keys":["ksk_a","ksk_b"]}"#] {
            let app = Router::new().route(
                "/openapi/claim",
                post(move || async move { (axum::http::StatusCode::OK, body) }),
            );
            let client =
                SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                    .unwrap();
            assert!(
                client
                    .purchase(1, "0123456789abcdef0123456789abcdef")
                    .await
                    .is_err(),
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn kiro_app_keeps_paid_keys_even_when_the_prefix_is_unexpected() {
        // 积分已经扣了。前缀不是 ksk_ 也必须收下，不能钱花了还把 key 扔掉。
        let app = Router::new().route(
            "/openapi/claim",
            post(|| async {
                axum::Json(serde_json::json!({
                    "keys": ["key-1", "key-2"], "pointsCost": 200, "balance": 800
                }))
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        let purchase = client
            .purchase(2, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap();

        assert_eq!(purchase.purchased, 2);
        assert_eq!(purchase.points_cost, Some(200));
        assert!(!format!("{purchase:?}").contains("key-1"));

        // kiro-rs 那边保持严格：它的响应格式已经验证过，非 ksk_ 就是拿错东西了。
        let strict = Router::new().route(
            "/api/my/purchase",
            post(|| async {
                axum::Json(serde_json::json!({
                    "client_order_id": "0123456789abcdef0123456789abcdef",
                    "purchased": 1, "remaining": 0, "keys": [{"key": "key-1"}]
                }))
            }),
        );
        let strict_client =
            SupplierClient::with_kind(server(strict).await, "secret", SupplierKind::KiroRs).unwrap();
        assert!(
            strict_client
                .purchase(1, "0123456789abcdef0123456789abcdef")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn out_of_stock_is_reported_as_its_own_outcome_not_a_failure() {
        let app = Router::new().route(
            "/openapi/claim",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    r#"{"error":{"type":"out_of_stock","message":"库存不足：需要 1 个，当前可售 0 个"}}"#,
                )
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        // 按 error.type 判定，不依赖中文文案。
        assert!(matches!(
            client
                .purchase(1, "0123456789abcdef0123456789abcdef")
                .await,
            Err(SupplierError::OutOfStock)
        ));
    }

    #[tokio::test]
    async fn rate_limits_are_mapped_with_retry_after_and_not_retried() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/openapi/stock",
            get(move || {
                let observed = observed.clone();
                async move {
                    *observed.lock().unwrap() += 1;
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        r#"{"error":{"type":"rate_limit_exceeded","message":"slow down","retryAfter":42}}"#,
                    )
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        let error = client.available_stock().await.unwrap_err();

        assert!(matches!(
            error,
            SupplierError::RateLimited {
                retry_after: Some(42),
                ..
            }
        ));
        assert!(error.to_string().contains("retry after 42s"));
        assert_eq!(*calls.lock().unwrap(), 1, "429 不重试");
    }

    #[tokio::test]
    async fn kiro_app_cannot_register_or_test_webhooks() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().fallback(move || {
            let observed = observed.clone();
            async move {
                *observed.lock().unwrap() += 1;
                axum::http::StatusCode::NO_CONTENT
            }
        });
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        assert!(matches!(
            client.register_webhook("https://admin.example/hook").await,
            Err(SupplierError::Unsupported(_))
        ));
        assert!(matches!(
            client.test_webhook().await,
            Err(SupplierError::Unsupported(_))
        ));
        // 不支持就不该发请求出去。
        assert_eq!(*calls.lock().unwrap(), 0);
        assert!(!SupplierKind::KiroApp.supports_webhook_registration());
        assert!(SupplierKind::KiroRs.supports_webhook_registration());
    }

    #[test]
    fn default_constructor_keeps_the_legacy_protocol() {
        assert_eq!(
            SupplierClient::new("http://localhost", "secret")
                .unwrap()
                .kind(),
            SupplierKind::KiroRs
        );
    }

    #[test]
    fn validates_constructor_and_debug_redacts_secrets() {
        assert!(SupplierClient::new("ftp://localhost", "secret").is_err());
        assert!(SupplierClient::new("http://localhost/", " ").is_err());
        let client = SupplierClient::new("http://localhost/", "secret").unwrap();
        assert!(!format!("{client:?}").contains("secret"));
    }

    #[test]
    fn rejects_non_origin_supplier_base_urls_and_builds_endpoints() {
        for base in [
            "http://localhost/api",
            "http://localhost/api/",
            "http://user:pass@localhost/",
            "http://localhost/?query=1",
            "http://localhost/#fragment",
        ] {
            assert!(SupplierClient::new(base, "secret").is_err(), "{base}");
        }
        let client = SupplierClient::new("http://localhost", "secret").unwrap();
        assert_eq!(
            client.endpoint("/api/status").unwrap().as_str(),
            "http://localhost/api/status"
        );
    }

    #[test]
    fn debug_redacts_profile_webhook_and_status_extra_values() {
        let profile = Profile {
            name: "demo".into(),
            quota: 1,
            remaining: 1,
            used_quota: 0,
            webhook_url: "https://canary.invalid/hook?secret=canary".into(),
        };
        let mut extra = serde_json::Map::new();
        extra.insert("ksk_secret".into(), serde_json::json!("ksk_value"));
        extra.insert("usr_secret".into(), serde_json::json!("usr_value"));
        let status = SupplierStatus {
            keys_active: 1,
            keys_dead: 0,
            keys_stock: 0,
            generating: false,
            extra,
        };
        for output in [format!("{profile:?}"), format!("{status:?}")] {
            assert!(!output.contains("canary"));
            assert!(!output.contains("ksk_secret"));
            assert!(!output.contains("usr_secret"));
        }
    }

    #[test]
    fn deserializes_documented_boolean_generating_status() {
        let status: SupplierStatus = serde_json::from_value(serde_json::json!({
            "keys_active": 10,
            "keys_dead": 2,
            "keys_stock": 4,
            "generating": false,
            "auto_check": true
        }))
        .unwrap();

        assert!(!status.generating);
        assert_eq!(status.keys_active, 10);
    }

    #[test]
    fn requires_a_nonempty_supplier_key_suffix_and_redacts_empty_token() {
        assert!(sanitize("ksk_", "secret").contains("[REDACTED]"));
        assert!(sanitize("ksk_good", "secret").contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn retries_server_errors_exactly_three_times_and_returns_last_error() {
        let calls = Arc::new(Mutex::new(0));
        let state = calls.clone();
        let app = Router::new().route(
            "/api/my/webhook/test",
            post(move || {
                let state = state.clone();
                async move {
                    let mut calls = state.lock().unwrap();
                    *calls += 1;
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("last body {calls}"),
                    )
                }
            }),
        );
        let client = SupplierClient::new(server(app).await, "usr secret").unwrap();
        let error = client.test_webhook().await.unwrap_err();
        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(error.to_string().contains("last body 1"));
    }

    #[tokio::test]
    async fn retries_transport_failures_until_third_request_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _attempt in 1..=1 {
                let (stream, _) = listener.accept().await.unwrap();
                drop(stream);
            }
        });
        let client = SupplierClient::new(format!("http://{address}"), "secret").unwrap();
        assert!(client.test_webhook().await.is_err());
    }

    #[tokio::test]
    async fn retries_response_body_read_failures() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for attempt in 1..=3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 512];
                let _ = stream.read(&mut request).await;
                if attempt < 3 {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort")
                        .await
                        .unwrap();
                } else {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                        .await
                        .unwrap();
                }
            }
        });
        let client = SupplierClient::new(format!("http://{address}"), "secret").unwrap();
        assert_eq!(client.status().await.unwrap().keys_active, 0);
    }

    #[tokio::test]
    async fn rejects_invalid_webhook_urls_before_sending() {
        let calls = Arc::new(Mutex::new(0));
        let state = calls.clone();
        let app = Router::new().fallback(move || {
            let state = state.clone();
            async move {
                *state.lock().unwrap() += 1;
                axum::http::StatusCode::NO_CONTENT
            }
        });
        let client = SupplierClient::new(server(app).await, "secret").unwrap();
        for url in ["ftp://hook", "/relative", ""] {
            assert!(client.register_webhook(url).await.is_err());
        }
        assert_eq!(*calls.lock().unwrap(), 0);
    }

    #[test]
    fn supplier_error_display_and_debug_redact_and_bound_untrusted_body() {
        let secret = "usr secret";
        let token = "ksk_current_token";
        let body = format!("{secret} {token} {}", "界".repeat(400));
        let error = SupplierError::http(500, &body, secret);
        let display = error.to_string();
        let debug = format!("{error:?}");
        for output in [&display, &debug] {
            assert!(!output.contains(secret));
            assert!(!output.contains(token));
            assert!(output.chars().count() <= 384);
        }
    }

    #[test]
    fn purchase_and_supplier_key_debug_redact_key_values() {
        let key = SupplierKey("ksk_private".to_owned());
        let purchase = Purchase {
            client_order_id: "0123456789abcdef0123456789abcdef".to_owned(),
            purchased: 1,
            remaining: 2,
            points_cost: Some(100),
            keys: vec![key.clone()],
        };
        assert!(!format!("{key:?}").contains("ksk_private"));
        assert!(!format!("{purchase:?}").contains("ksk_private"));
    }
}
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{fmt, sync::OnceLock};

use crate::model::config::SupplierKind;

const MAX_ATTEMPTS: usize = 3;
const SUPPLIER_USER_AGENT: &str = "kiro-rs-key-supplier/1.0";

#[derive(Clone)]
pub struct SupplierClient {
    client: reqwest::Client,
    base_url: Url,
    api_key: Secret,
    kind: SupplierKind,
}

impl fmt::Debug for SupplierClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupplierClient")
            .field("base_url", &self.base_url)
            .field("kind", &self.kind)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

impl SupplierClient {
    /// 历史构造器，默认 `kiro-rs` 协议。
    pub fn new(base_url: impl AsRef<str>, api_key: impl AsRef<str>) -> Result<Self, SupplierError> {
        Self::with_kind(base_url, api_key, SupplierKind::KiroRs)
    }

    pub fn with_kind(
        base_url: impl AsRef<str>,
        api_key: impl AsRef<str>,
        kind: SupplierKind,
    ) -> Result<Self, SupplierError> {
        let raw_url = base_url.as_ref().trim();
        let url = Url::parse(raw_url)
            .map_err(|_| SupplierError::invalid("base_url must be a valid http(s) URL"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(SupplierError::invalid("base_url must use http or https"));
        }
        if (url.path() != "" && url.path() != "/")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(SupplierError::invalid(
                "base_url must contain only an http(s) origin",
            ));
        }
        let mut base_url = url;
        base_url.set_path("/");
        let key = api_key.as_ref().trim();
        if key.is_empty() {
            return Err(SupplierError::invalid("api_key must not be empty"));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .no_proxy()
            .user_agent(SUPPLIER_USER_AGENT)
            .build()
            .map_err(|error| SupplierError::network(&error.to_string(), key))?;
        Ok(Self {
            client,
            base_url,
            api_key: Secret(key.to_owned()),
            kind,
        })
    }

    #[cfg(test)]
    pub fn kind(&self) -> SupplierKind {
        self.kind
    }

    pub async fn profile(&self) -> Result<Profile, SupplierError> {
        self.request(Method::GET, "/api/my/profile", None, RetryPolicy::Retryable)
            .await
    }

    pub async fn stock(&self) -> Result<Stock, SupplierError> {
        self.request(Method::GET, "/api/my/stock", None, RetryPolicy::Retryable)
            .await
    }

    pub async fn status(&self) -> Result<SupplierStatus, SupplierError> {
        self.request(Method::GET, "/api/status", None, RetryPolicy::Retryable)
            .await
    }

    /// 跨协议统一的概览。缺的字段留 `None`，由调用方决定怎么展示。
    pub async fn snapshot(&self) -> Result<SupplierSnapshot, SupplierError> {
        match self.kind {
            SupplierKind::KiroRs => {
                let profile = self.profile().await?;
                let stock = self.stock().await?;
                let status = self.status().await?;
                Ok(SupplierSnapshot {
                    stock_available: Some(stock.max),
                    key_price: None,
                    balance: Some(profile.remaining),
                    webhook_url: Some(profile.webhook_url.clone()),
                    profile: Some(profile),
                    status: Some(status),
                })
            }
            SupplierKind::KiroApp => {
                let stock: KiroAppStock = self
                    .request(Method::GET, "/openapi/stock", None, RetryPolicy::Retryable)
                    .await?;
                let balance: KiroAppBalance = self
                    .request(Method::GET, "/openapi/balance", None, RetryPolicy::Retryable)
                    .await?;
                Ok(SupplierSnapshot {
                    stock_available: Some(stock.available_keys),
                    key_price: stock.key_price,
                    balance: Some(balance.balance),
                    webhook_url: None,
                    profile: None,
                    status: None,
                })
            }
        }
    }

    /// 库存可用数。`kiro-rs` 是 `/api/my/stock` 的 `max`，`kiro-app` 是 `availableKeys`。
    pub async fn available_stock(&self) -> Result<u64, SupplierError> {
        match self.kind {
            SupplierKind::KiroRs => Ok(self.stock().await?.max),
            SupplierKind::KiroApp => {
                let stock: KiroAppStock = self
                    .request(Method::GET, "/openapi/stock", None, RetryPolicy::Retryable)
                    .await?;
                Ok(stock.available_keys)
            }
        }
    }

    /// 下单取 Key。
    ///
    /// `kiro-rs` 带 `client_order_id`，服务端幂等，网络抖动可安全重试。
    /// `kiro-app` 的 `/openapi/claim` **没有幂等键**，重试会重复扣积分，
    /// 因此走 `RetryPolicy::Never`：宁可报错让人工重放，也不冒重复购买的风险。
    pub async fn purchase(
        &self,
        count: u32,
        client_order_id: &str,
    ) -> Result<Purchase, SupplierError> {
        if count == 0 {
            return Err(SupplierError::invalid("purchase count must be positive"));
        }
        if client_order_id.len() != 32
            || !client_order_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(SupplierError::invalid(
                "client_order_id must be 32 hexadecimal characters",
            ));
        }
        match self.kind {
            SupplierKind::KiroRs => self.purchase_kiro_rs(count, client_order_id).await,
            SupplierKind::KiroApp => self.claim_kiro_app(count, client_order_id).await,
        }
    }

    async fn purchase_kiro_rs(
        &self,
        count: u32,
        client_order_id: &str,
    ) -> Result<Purchase, SupplierError> {
        let response: PurchaseWire = self
            .request(
                Method::POST,
                "/api/my/purchase",
                Some(serde_json::json!({
                    "count": count,
                    "client_order_id": client_order_id,
                })),
                RetryPolicy::Retryable,
            )
            .await?;
        if response.client_order_id != client_order_id {
            return Err(SupplierError::invalid(
                "purchase response client_order_id mismatch",
            ));
        }
        if response.purchased > count {
            return Err(SupplierError::invalid(
                "purchase response purchased exceeds count",
            ));
        }
        if response.keys.len() != response.purchased as usize {
            return Err(SupplierError::invalid(
                "purchase response key count mismatch",
            ));
        }
        Ok(Purchase {
            client_order_id: response.client_order_id,
            purchased: response.purchased,
            remaining: response.remaining,
            points_cost: None,
            keys: validate_keys(response.keys.into_iter().map(|key| key.key))?,
        })
    }

    async fn claim_kiro_app(
        &self,
        count: u32,
        client_order_id: &str,
    ) -> Result<Purchase, SupplierError> {
        let text = self
            .send(
                Method::POST,
                "/openapi/claim",
                Some(serde_json::json!({ "count": count })),
                RetryPolicy::Never,
            )
            .await?;
        let response: KiroAppClaim = serde_json::from_str(&text)
            .map_err(|error| SupplierError::decode(&error.to_string(), &self.api_key.0))?;
        // 取 1 个时对方可能返回 {key}，批量时返回 {keys:[...]}。
        let raw_keys = match (response.keys, response.key) {
            (Some(keys), _) => keys,
            (None, Some(key)) => vec![key],
            (None, None) => {
                return Err(SupplierError::invalid("claim response contains no keys"));
            }
        };
        // 空数组和缺字段是同一件事（一个 key 都没拿到），必须同样报错。
        // 放过去会记成「成功买了 0 个」，万一积分已经扣了就查不出来了。
        if raw_keys.is_empty() {
            return Err(SupplierError::invalid("claim response contains no keys"));
        }
        if raw_keys.len() > count as usize {
            return Err(SupplierError::invalid(
                "claim response returned more keys than requested",
            ));
        }
        // 走宽松校验：积分已经扣了，不能因为前缀不合预期就把 key 扔掉。
        let keys = accept_paid_keys(raw_keys)?;
        Ok(Purchase {
            client_order_id: client_order_id.to_owned(),
            purchased: keys.len() as u32,
            // claim 响应给的是扣费后余额；库存快照它不返回。
            remaining: response.balance.unwrap_or_default(),
            points_cost: response.points_cost,
            keys,
        })
    }

    pub async fn register_webhook(&self, webhook_url: &str) -> Result<(), SupplierError> {
        if !self.kind.supports_webhook_registration() {
            return Err(SupplierError::Unsupported(
                "该供货商不支持远程注册 webhook，请在对方面板手动填写回调地址".to_owned(),
            ));
        }
        validate_http_url(webhook_url)?;
        self.request_empty(
            Method::PUT,
            "/api/my/webhook",
            Some(serde_json::json!({ "webhook_url": webhook_url })),
            RetryPolicy::Retryable,
        )
        .await
    }

    pub async fn test_webhook(&self) -> Result<(), SupplierError> {
        if !self.kind.supports_webhook_registration() {
            return Err(SupplierError::Unsupported(
                "该供货商不支持 webhook 测试推送".to_owned(),
            ));
        }
        self.request_empty(
            Method::POST,
            "/api/my/webhook/test",
            None,
            RetryPolicy::Never,
        )
        .await
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        policy: RetryPolicy,
    ) -> Result<T, SupplierError> {
        let text = self.send(method, path, body, policy).await?;
        serde_json::from_str(&text)
            .map_err(|error| SupplierError::decode(&error.to_string(), &self.api_key.0))
    }

    async fn request_empty(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        policy: RetryPolicy,
    ) -> Result<(), SupplierError> {
        self.send(method, path, body, policy).await.map(|_| ())
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        policy: RetryPolicy,
    ) -> Result<String, SupplierError> {
        let url = self.endpoint(path)?;
        let mut last_network = None;
        let attempts = policy.attempts();
        for attempt in 0..attempts {
            let mut request = self.client.request(method.clone(), url.clone());
            request = match self.kind {
                SupplierKind::KiroRs => request.header("X-API-Key", &self.api_key.0),
                SupplierKind::KiroApp => request.bearer_auth(&self.api_key.0),
            };
            if let Some(json) = body.clone() {
                request = request.json(&json);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    last_network =
                        Some(SupplierError::network(&error.to_string(), &self.api_key.0));
                    if attempt + 1 < attempts {
                        continue;
                    }
                    return Err(last_network.unwrap());
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(error) => {
                    last_network =
                        Some(SupplierError::network(&error.to_string(), &self.api_key.0));
                    if attempt + 1 < attempts {
                        continue;
                    }
                    return Err(last_network.unwrap());
                }
            };
            if status.is_server_error() && policy.allows_retry() && attempt + 1 < attempts {
                continue;
            }
            if !status.is_success() {
                // 429 一律不重试：kiro-app 的 claim 没有幂等键，盲目重试会重复扣积分。
                if status.as_u16() == 429 {
                    return Err(SupplierError::rate_limited(
                        &text,
                        retry_after_seconds(&text),
                        &self.api_key.0,
                    ));
                }
                // 库存被别人抢完是正常竞争结果，不是故障。按 error.type 判定
                // （对方文档明确说不要依赖中文错误文案）。
                if error_type(&text).is_some_and(|kind| kind == "out_of_stock") {
                    return Err(SupplierError::OutOfStock);
                }
                return Err(SupplierError::http(status.as_u16(), &text, &self.api_key.0));
            }
            return Ok(text);
        }
        Err(last_network.unwrap_or_else(|| SupplierError::invalid("request failed")))
    }

    fn endpoint(&self, path: &str) -> Result<Url, SupplierError> {
        self.base_url
            .join(path)
            .map_err(|_| SupplierError::invalid("invalid supplier endpoint"))
    }
}

#[derive(Clone, Copy)]
enum RetryPolicy {
    Retryable,
    Never,
}

impl RetryPolicy {
    fn attempts(self) -> usize {
        match self {
            Self::Retryable => MAX_ATTEMPTS,
            Self::Never => 1,
        }
    }

    fn allows_retry(self) -> bool {
        matches!(self, Self::Retryable)
    }
}

#[derive(Clone)]
struct Secret(String);

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub quota: u64,
    pub remaining: u64,
    pub used_quota: u64,
    pub webhook_url: String,
}

impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("name", &self.name)
            .field("quota", &self.quota)
            .field("remaining", &self.remaining)
            .field("used_quota", &self.used_quota)
            .field("webhook_url", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Stock {
    pub max: u64,
}

/// 跨协议统一的供货商概览。字段按协议能力取值，取不到的留 `None`。
#[derive(Clone, PartialEq)]
pub struct SupplierSnapshot {
    /// 可采购的库存数量。
    pub stock_available: Option<u64>,
    /// 单个 Key 的价格（`kiro-app` 的 `keyPrice`）。
    pub key_price: Option<f64>,
    /// 剩余额度/积分。
    pub balance: Option<u64>,
    /// 供货商侧登记的回调地址（仅 `kiro-rs` 可读）。
    pub webhook_url: Option<String>,
    pub profile: Option<Profile>,
    pub status: Option<SupplierStatus>,
}

impl fmt::Debug for SupplierSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupplierSnapshot")
            .field("stock_available", &self.stock_available)
            .field("key_price", &self.key_price)
            .field("balance", &self.balance)
            .field("webhook_url_configured", &self.webhook_url.is_some())
            .field("profile", &self.profile)
            .field("status", &self.status)
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SupplierStatus {
    #[serde(default)]
    pub keys_active: u64,
    #[serde(default)]
    pub keys_dead: u64,
    #[serde(default)]
    pub keys_stock: u64,
    #[serde(default)]
    pub generating: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl fmt::Debug for SupplierStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupplierStatus")
            .field("keys_active", &self.keys_active)
            .field("keys_dead", &self.keys_dead)
            .field("keys_stock", &self.keys_stock)
            .field("generating", &self.generating)
            .field("extra_count", &self.extra.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SupplierKey(String);

impl SupplierKey {
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for SupplierKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Purchase {
    pub client_order_id: String,
    pub purchased: u32,
    /// kiro-rs：剩余可采购额度。kiro-app：扣费后剩余积分。
    pub remaining: u64,
    /// 本次消耗积分，仅 kiro-app 返回。
    pub points_cost: Option<u64>,
    pub keys: Vec<SupplierKey>,
}

impl fmt::Debug for Purchase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Purchase")
            .field("client_order_id", &self.client_order_id)
            .field("purchased", &self.purchased)
            .field("remaining", &self.remaining)
            .field("points_cost", &self.points_cost)
            .field("keys", &"[REDACTED]")
            .finish()
    }
}

#[derive(Deserialize)]
struct PurchaseWire {
    client_order_id: String,
    purchased: u32,
    remaining: u64,
    keys: Vec<KeyWire>,
}
#[derive(Deserialize)]
struct KeyWire {
    key: String,
}

/// `GET /openapi/stock` → `{availableKeys, keyPrice}`。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KiroAppStock {
    #[serde(default)]
    available_keys: u64,
    #[serde(default)]
    key_price: Option<f64>,
}

/// `GET /openapi/balance` → `{balance}`。
#[derive(Deserialize)]
struct KiroAppBalance {
    #[serde(default)]
    balance: u64,
}

/// `POST /openapi/claim` → `{key, pointsCost, balance}`（单个）
/// 或 `{keys:[...], pointsCost, balance}`（批量）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KiroAppClaim {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    keys: Option<Vec<String>>,
    /// 本次消耗的积分。车主自投凭据产出的 key 为 0。
    #[serde(default)]
    points_cost: Option<u64>,
    /// 扣费后的剩余积分。
    #[serde(default)]
    balance: Option<u64>,
}

/// `{ "error": { "type", "message" } }` 统一错误信封。
#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    /// 机器可判定的错误类型，例如 `out_of_stock` / `rate_limit_exceeded`。
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// 对方文档写的是 `retryAfter`；`alias` 兼容 snake_case 变体。
    #[serde(default, alias = "retry_after")]
    retry_after: Option<u64>,
}

/// 从限流响应里取 `retryAfter`（秒）。取不到就 `None`，不影响主流程。
fn retry_after_seconds(body: &str) -> Option<u64> {
    serde_json::from_str::<ErrorEnvelope>(body)
        .ok()
        .and_then(|envelope| envelope.error.retry_after)
}

/// 取 `error.type`。对方文档要求按这个字段判定，不要依赖错误文案。
fn error_type(body: &str) -> Option<String> {
    serde_json::from_str::<ErrorEnvelope>(body)
        .ok()
        .and_then(|envelope| envelope.error.kind)
}

/// 严格校验：必须是 `ksk_` 前缀。用于 kiro-rs——它的响应格式已验证过。
fn validate_keys(
    keys: impl IntoIterator<Item = String>,
) -> Result<Vec<SupplierKey>, SupplierError> {
    keys.into_iter()
        .map(|key| {
            let key = key.trim().to_owned();
            if !key.starts_with("ksk_") || key.len() <= "ksk_".len() {
                Err(SupplierError::invalid(
                    "purchase response contains an invalid key",
                ))
            } else {
                Ok(SupplierKey(key))
            }
        })
        .collect()
}

/// 宽松校验：只要非空就收下，前缀不像 Kiro Key 时打日志告警。
///
/// 为什么不严格：kiroapp 的 claim **已经扣了积分**才返回 key。此时因为前缀不符合
/// 我们的预期就整单报错，等于钱花了、key 扔了、还得人工去对方后台捞。对方文档里的
/// 示例是 `"key-1"` / `"实际的 Kiro Key"`，并没有承诺 `ksk_` 前缀，所以这里
/// 不能拿前缀当硬门槛——真正的有效性由后续凭据导入去判。
fn accept_paid_keys(
    keys: impl IntoIterator<Item = String>,
) -> Result<Vec<SupplierKey>, SupplierError> {
    let mut accepted = Vec::new();
    let mut unexpected_prefix = 0_usize;
    for key in keys {
        let key = key.trim().to_owned();
        if key.is_empty() {
            continue;
        }
        if !key.starts_with("ksk_") {
            unexpected_prefix += 1;
        }
        accepted.push(SupplierKey(key));
    }
    if unexpected_prefix > 0 {
        // 只报个数，绝不打 key 本身。
        tracing::warn!(
            count = unexpected_prefix,
            "kiroapp claim 返回的 key 不是 ksk_ 前缀；已照收（积分已扣），请确认是否可用"
        );
    }
    if accepted.is_empty() {
        return Err(SupplierError::invalid("claim response contains no keys"));
    }
    Ok(accepted)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupplierError {
    InvalidInput(String),
    /// 该协议不支持这个操作（例如 kiro-app 无法远程注册 webhook）。
    Unsupported(String),
    /// 库存被别人抢完了。正常竞争结果，不是故障，不该重试也不该记成失败。
    OutOfStock,
    Http {
        status: u16,
        message: String,
    },
    /// 供货商限流。绝不重试，`retry_after` 是对方给的建议等待秒数。
    RateLimited {
        retry_after: Option<u64>,
        message: String,
    },
    Network(String),
    Decode(String),
}

impl SupplierError {
    fn invalid(message: &str) -> Self {
        Self::InvalidInput(message.to_owned())
    }
    fn http(status: u16, body: &str, secret: &str) -> Self {
        Self::Http {
            status,
            message: sanitize(body, secret),
        }
    }
    fn rate_limited(body: &str, retry_after: Option<u64>, secret: &str) -> Self {
        Self::RateLimited {
            retry_after,
            message: sanitize(body, secret),
        }
    }
    fn network(message: &str, secret: &str) -> Self {
        Self::Network(sanitize(message, secret))
    }
    fn decode(message: &str, secret: &str) -> Self {
        Self::Decode(sanitize(message, secret))
    }
}

impl fmt::Display for SupplierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported operation: {message}"),
            Self::OutOfStock => f.write_str("supplier is out of stock"),
            Self::Http { status, message } => write!(f, "supplier HTTP {status}: {message}"),
            Self::RateLimited {
                retry_after,
                message,
            } => match retry_after {
                Some(seconds) => {
                    write!(f, "supplier rate limited (retry after {seconds}s): {message}")
                }
                None => write!(f, "supplier rate limited: {message}"),
            },
            Self::Network(message) => write!(f, "supplier network error: {message}"),
            Self::Decode(message) => write!(f, "supplier response error: {message}"),
        }
    }
}
impl std::error::Error for SupplierError {}

fn validate_http_url(value: &str) -> Result<(), SupplierError> {
    let url = Url::parse(value)
        .map_err(|_| SupplierError::invalid("webhook_url must be a valid http(s) URL"))?;
    if matches!(url.scheme(), "http" | "https") {
        Ok(())
    } else {
        Err(SupplierError::invalid("webhook_url must use http or https"))
    }
}

fn sanitize(value: &str, secret: &str) -> String {
    static TOKEN: OnceLock<regex::Regex> = OnceLock::new();
    let token = TOKEN.get_or_init(|| regex::Regex::new(r#"ksk_[^\s"'<>]*"#).unwrap());
    let replaced_secret = value.replace(secret, "[REDACTED]");
    let redacted = token.replace_all(&replaced_secret, "[REDACTED]");
    redacted.chars().take(300).collect()
}
