#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        routing::{get, post, put},
    };
    use std::sync::{Arc, Mutex};
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
                    seen.lock().unwrap().push((request.uri().path().to_owned(), request.headers().get("x-api-key").unwrap().to_str().unwrap().to_owned(), request.headers().get("content-type").map(|v| v.to_str().unwrap().to_owned())));
                    axum::Json(serde_json::json!({"name":"demo","quota":10,"remaining":7,"used_quota":3,"webhook_url":"http://hook"}))
                }
            }))
            .route("/api/my/stock", get(|| async { axum::Json(serde_json::json!({"max": 9})) }))
            .route("/api/status", get(|| async { axum::Json(serde_json::json!({"ok":true})) }));
        let client = SupplierClient::new(server(app).await, "secret").unwrap();
        assert_eq!(client.profile().await.unwrap().remaining, 7);
        assert_eq!(client.stock().await.unwrap().max, 9);
        assert_eq!(client.status().await.unwrap()["ok"], true);
        assert_eq!(seen.lock().unwrap()[0].1, "secret");
    }

    #[tokio::test]
    async fn retries_purchase_after_server_error_with_same_order_id() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = seen.clone();
        let app = Router::new().route("/api/my/purchase", post(move |request: axum::http::Request<axum::body::Body>| {
            let state = state.clone();
            async move {
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
            .route("/api/my/webhook", put(move |request: axum::http::Request<axum::body::Body>| { let paths = paths_clone.clone(); async move { paths.lock().unwrap().push(request.uri().path().to_owned()); axum::http::StatusCode::NO_CONTENT } }))
            .route("/api/my/webhook/test", post(move |request: axum::http::Request<axum::body::Body>| { let paths = paths_test.clone(); async move { paths.lock().unwrap().push(request.uri().path().to_owned()); axum::http::StatusCode::NO_CONTENT } }));
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

    #[test]
    fn validates_constructor_and_debug_redacts_secrets() {
        assert!(SupplierClient::new("ftp://localhost", "secret").is_err());
        assert!(SupplierClient::new("http://localhost/", " ").is_err());
        let client = SupplierClient::new("http://localhost/", "secret").unwrap();
        assert!(!format!("{client:?}").contains("secret"));
    }
}
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{fmt, sync::OnceLock};

const MAX_ATTEMPTS: usize = 3;

#[derive(Clone)]
pub struct SupplierClient {
    client: reqwest::Client,
    base_url: String,
    api_key: Secret,
}

impl fmt::Debug for SupplierClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupplierClient")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

impl SupplierClient {
    pub fn new(base_url: impl AsRef<str>, api_key: impl AsRef<str>) -> Result<Self, SupplierError> {
        let raw_url = base_url.as_ref().trim();
        let url = Url::parse(raw_url)
            .map_err(|_| SupplierError::invalid("base_url must be a valid http(s) URL"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(SupplierError::invalid("base_url must use http or https"));
        }
        let key = api_key.as_ref().trim();
        if key.is_empty() {
            return Err(SupplierError::invalid("api_key must not be empty"));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .no_proxy()
            .build()
            .map_err(|error| SupplierError::network(&error.to_string(), key))?;
        Ok(Self {
            client,
            base_url: raw_url.trim_end_matches('/').to_owned(),
            api_key: Secret(key.to_owned()),
        })
    }

    pub async fn profile(&self) -> Result<Profile, SupplierError> {
        self.request(Method::GET, "/api/my/profile", None).await
    }

    pub async fn stock(&self) -> Result<Stock, SupplierError> {
        self.request(Method::GET, "/api/my/stock", None).await
    }

    pub async fn status(&self) -> Result<serde_json::Value, SupplierError> {
        self.request(Method::GET, "/api/status", None).await
    }

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
        let response: PurchaseWire = self
            .request(
                Method::POST,
                "/api/my/purchase",
                Some(serde_json::json!({
                    "count": count,
                    "client_order_id": client_order_id,
                })),
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
        let keys = response
            .keys
            .into_iter()
            .map(|key| {
                if key.key.is_empty() || !key.key.starts_with("ksk_") {
                    Err(SupplierError::invalid(
                        "purchase response contains an invalid key",
                    ))
                } else {
                    Ok(SupplierKey(key.key))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Purchase {
            client_order_id: response.client_order_id,
            purchased: response.purchased,
            remaining: response.remaining,
            keys,
        })
    }

    pub async fn register_webhook(&self, webhook_url: &str) -> Result<(), SupplierError> {
        validate_http_url(webhook_url)?;
        self.request_empty(
            Method::PUT,
            "/api/my/webhook",
            Some(serde_json::json!({ "webhook_url": webhook_url })),
        )
        .await
    }

    pub async fn test_webhook(&self) -> Result<(), SupplierError> {
        self.request_empty(Method::POST, "/api/my/webhook/test", None)
            .await
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, SupplierError> {
        let text = self.send(method, path, body).await?;
        serde_json::from_str(&text)
            .map_err(|error| SupplierError::decode(&error.to_string(), &self.api_key.0))
    }

    async fn request_empty(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(), SupplierError> {
        self.send(method, path, body).await.map(|_| ())
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<String, SupplierError> {
        let url = format!("{}{}", self.base_url, path);
        let mut last_network = None;
        for attempt in 0..MAX_ATTEMPTS {
            let mut request = self
                .client
                .request(method.clone(), &url)
                .header("X-API-Key", &self.api_key.0);
            if let Some(json) = body.clone() {
                request = request.json(&json);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    last_network =
                        Some(SupplierError::network(&error.to_string(), &self.api_key.0));
                    if attempt + 1 < MAX_ATTEMPTS {
                        continue;
                    }
                    return Err(last_network.unwrap());
                }
            };
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|error| SupplierError::network(&error.to_string(), &self.api_key.0))?;
            if status.is_server_error() && attempt + 1 < MAX_ATTEMPTS {
                continue;
            }
            if !status.is_success() {
                return Err(SupplierError::http(status.as_u16(), &text, &self.api_key.0));
            }
            return Ok(text);
        }
        Err(last_network.unwrap_or_else(|| SupplierError::invalid("request failed")))
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

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Stock {
    pub max: u64,
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
    pub remaining: u64,
    pub keys: Vec<SupplierKey>,
}

impl fmt::Debug for Purchase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Purchase")
            .field("client_order_id", &self.client_order_id)
            .field("purchased", &self.purchased)
            .field("remaining", &self.remaining)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupplierError {
    InvalidInput(String),
    Http { status: u16, message: String },
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
            Self::Http { status, message } => write!(f, "supplier HTTP {status}: {message}"),
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
    let token = TOKEN.get_or_init(|| regex::Regex::new(r#"ksk_[^\s"'<>]+"#).unwrap());
    let replaced_secret = value.replace(secret, "[REDACTED]");
    let redacted = token.replace_all(&replaced_secret, "[REDACTED]");
    redacted.chars().take(300).collect()
}
