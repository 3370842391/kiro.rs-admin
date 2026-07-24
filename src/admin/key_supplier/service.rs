use std::future::Future;
use std::pin::Pin;

use crate::admin::key_supplier::client::SupplierClient;
use crate::admin::key_supplier::store::{ProcessSummary, StoredSupplierEvent};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::region::API_KEY_AUTH_REGION;
use crate::kiro::token_manager::MultiTokenManager;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountDecision {
    Purchase(u32),
    Skip,
}

pub fn select_purchase_count(
    event_count: u32,
    stock_count: u64,
    configured_max: u32,
    configured_min: u32,
) -> CountDecision {
    let count = event_count
        .min(stock_count.min(u64::from(u32::MAX)) as u32)
        .min(configured_max);
    if count < configured_min {
        CountDecision::Skip
    } else {
        CountDecision::Purchase(count)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum IncomingWebhook {
    NewKeysAvailable {
        event_id: String,
        purchase_order_id: String,
        message: String,
        new_keys: u32,
    },
    AllKeysDead {
        event_id: String,
        message: String,
        dead: u32,
    },
}

impl IncomingWebhook {
    pub fn parse(body: &[u8]) -> Result<Self, SupplierServiceError> {
        let value: Value =
            serde_json::from_slice(body).map_err(|_| SupplierServiceError::InvalidJson)?;
        let object = value
            .as_object()
            .ok_or(SupplierServiceError::InvalidPayload)?;
        let event = object
            .get("event")
            .and_then(Value::as_str)
            .ok_or(SupplierServiceError::InvalidPayload)?;
        let event_id = required_id(object, "event_id")?;
        let message = object
            .get("message")
            .and_then(Value::as_str)
            .ok_or(SupplierServiceError::InvalidPayload)?
            .to_string();

        match event {
            "new_keys_available" => Ok(Self::NewKeysAvailable {
                event_id,
                purchase_order_id: required_id(object, "purchase_order_id")?,
                message,
                new_keys: required_quantity(object, "new_keys")?,
            }),
            "all_keys_dead" => Ok(Self::AllKeysDead {
                event_id,
                message,
                dead: required_quantity(object, "dead")?,
            }),
            _ => Err(SupplierServiceError::InvalidPayload),
        }
    }

    fn into_event(self) -> IncomingSupplierEvent {
        match self {
            Self::NewKeysAvailable {
                event_id,
                purchase_order_id,
                message,
                new_keys,
            } => IncomingSupplierEvent {
                event_id,
                event_type: "new_keys_available".to_string(),
                purchase_order_id: Some(purchase_order_id),
                message: Some(message),
                quantity: i64::from(new_keys),
            },
            Self::AllKeysDead {
                event_id,
                message,
                dead,
            } => IncomingSupplierEvent {
                event_id,
                event_type: "all_keys_dead".to_string(),
                purchase_order_id: None,
                message: Some(message),
                quantity: i64::from(dead),
            },
        }
    }
}

fn required_id(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<String, SupplierServiceError> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or(SupplierServiceError::InvalidPayload)?;
    Ok(value.to_string())
}

fn required_quantity(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<u32, SupplierServiceError> {
    let quantity = object
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(SupplierServiceError::InvalidPayload)?;
    Ok(quantity)
}

pub trait CredentialImporter: Send + Sync {
    fn import(
        &self,
        credential: KiroCredentials,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>>;
}

#[derive(Clone)]
pub struct TokenManagerCredentialImporter {
    token_manager: Arc<MultiTokenManager>,
}

impl TokenManagerCredentialImporter {
    pub fn new(token_manager: Arc<MultiTokenManager>) -> Self {
        Self { token_manager }
    }
}

impl CredentialImporter for TokenManagerCredentialImporter {
    fn import(
        &self,
        credential: KiroCredentials,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.token_manager.add_credential(credential).await?;
            Ok(())
        })
    }
}

pub struct KeySupplierService {
    store: Arc<SupplierEventStore>,
    runtime: parking_lot::RwLock<SupplierRuntimeConfig>,
    importer: Option<Arc<dyn CredentialImporter>>,
    processing_lock: tokio::sync::Mutex<()>,
}

impl KeySupplierService {
    pub fn new(store: Arc<SupplierEventStore>, runtime: SupplierRuntimeConfig) -> Self {
        Self {
            store,
            runtime: parking_lot::RwLock::new(runtime),
            importer: None,
            processing_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn with_importer(
        store: Arc<SupplierEventStore>,
        runtime: SupplierRuntimeConfig,
        importer: Arc<dyn CredentialImporter>,
    ) -> Self {
        Self {
            store,
            runtime: parking_lot::RwLock::new(runtime),
            importer: Some(importer),
            processing_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn new_with_token_manager(
        store: Arc<SupplierEventStore>,
        runtime: SupplierRuntimeConfig,
        token_manager: Arc<MultiTokenManager>,
    ) -> Self {
        Self::with_importer(
            store,
            runtime,
            Arc::new(TokenManagerCredentialImporter::new(token_manager)),
        )
    }

    pub fn runtime_config(&self) -> SupplierRuntimeConfig {
        self.runtime.read().clone()
    }

    pub fn set_runtime_config(&self, runtime: SupplierRuntimeConfig) {
        *self.runtime.write() = runtime;
    }

    pub fn store(&self) -> Arc<SupplierEventStore> {
        self.store.clone()
    }

    pub fn ingest<B: AsRef<[u8]>>(
        &self,
        token: &str,
        body: B,
    ) -> Result<IngestResult, SupplierServiceError> {
        let expected = self.runtime.read().webhook_token.clone();
        if expected.is_empty() || !crate::common::auth::constant_time_eq(&expected, token) {
            return Err(SupplierServiceError::Unauthorized);
        }

        let webhook = IncomingWebhook::parse(body.as_ref())?;
        let event = webhook.into_event();
        let event_id = event.event_id.clone();
        let event_type = event.event_type.clone();
        let outcome = self
            .store
            .insert_event(event)
            .map_err(|_| SupplierServiceError::Store)?;
        Ok(IngestResult {
            duplicate: matches!(outcome, InsertOutcome::Duplicate(_)),
            event_id,
            event_type,
        })
    }

    pub async fn process_pending(&self) -> Result<usize, SupplierServiceError> {
        let _guard = self.processing_lock.lock().await;
        let mut processed = 0;
        while let Some(event) = self
            .store
            .claim_next()
            .map_err(|_| SupplierServiceError::Store)?
        {
            self.process_claimed(event).await?;
            processed += 1;
        }
        Ok(processed)
    }

    pub async fn manual_purchase(
        &self,
        count: u32,
    ) -> Result<ManualPurchaseResult, SupplierServiceError> {
        let runtime = self.runtime_config();
        if count < runtime.min_purchase || count > runtime.max_purchase {
            return Err(SupplierServiceError::InvalidPurchaseQuantity);
        }

        let _guard = self.processing_lock.lock().await;
        let order_id = uuid::Uuid::new_v4().simple().to_string();
        self.store
            .insert_event(IncomingSupplierEvent {
                event_id: order_id.clone(),
                event_type: "manual_purchase".to_owned(),
                purchase_order_id: Some(order_id.clone()),
                message: None,
                quantity: i64::from(count),
            })
            .map_err(|_| SupplierServiceError::Store)?;
        let event = self
            .store
            .claim_by_event_id(&order_id)
            .map_err(|_| SupplierServiceError::Store)?
            .ok_or(SupplierServiceError::Store)?;
        let summary = self.process_claimed(event).await?;
        Ok(ManualPurchaseResult {
            order_id,
            requested: count,
            purchased: summary.purchased_count as u32,
            imported: summary.imported_count as u32,
            duplicate: summary.duplicate_count as u32,
            failed: summary.failed_count as u32,
        })
    }

    async fn process_claimed(
        &self,
        event: StoredSupplierEvent,
    ) -> Result<ProcessSummary, SupplierServiceError> {
        match self.execute_claimed(&event).await {
            Ok(ProcessAction::Complete(summary)) => {
                self.store
                    .complete(event.id, summary.clone())
                    .map_err(|_| SupplierServiceError::Store)?;
                Ok(summary)
            }
            Ok(ProcessAction::Skip) => {
                self.store
                    .skip(event.id, Some("purchase skipped"))
                    .map_err(|_| SupplierServiceError::Store)?;
                Ok(empty_summary())
            }
            Err(error) => {
                self.store
                    .fail(
                        event.id,
                        &sanitize_error(&error.to_string(), &self.runtime_config()),
                    )
                    .map_err(|_| SupplierServiceError::Store)?;
                Ok(ProcessSummary {
                    failed_count: 1,
                    ..empty_summary()
                })
            }
        }
    }

    async fn execute_claimed(
        &self,
        event: &StoredSupplierEvent,
    ) -> Result<ProcessAction, SupplierServiceError> {
        if event.event_type == "all_keys_dead" {
            return Ok(ProcessAction::Complete(empty_summary()));
        }

        let runtime = self.runtime_config();
        if event.event_type == "new_keys_available" && !runtime.auto_purchase {
            return Ok(ProcessAction::Skip);
        }
        if !matches!(
            event.event_type.as_str(),
            "new_keys_available" | "manual_purchase"
        ) {
            return Err(SupplierServiceError::InvalidEvent);
        }
        let importer = self
            .importer
            .as_ref()
            .ok_or(SupplierServiceError::ImporterUnavailable)?;
        let (count, client) = match event.event_type.as_str() {
            "new_keys_available" => {
                let client = SupplierClient::new(&runtime.base_url, &runtime.api_key)
                    .map_err(|_| SupplierServiceError::SupplierConfiguration)?;
                let stock = client
                    .stock()
                    .await
                    .map_err(|_| SupplierServiceError::SupplierApi)?;
                let event_count = u32::try_from(event.quantity)
                    .map_err(|_| SupplierServiceError::InvalidEvent)?;
                match select_purchase_count(
                    event_count,
                    stock.max,
                    runtime.max_purchase,
                    runtime.min_purchase,
                ) {
                    CountDecision::Purchase(count) => (count, client),
                    CountDecision::Skip => return Ok(ProcessAction::Skip),
                }
            }
            "manual_purchase" => {
                let count = u32::try_from(event.quantity)
                    .map_err(|_| SupplierServiceError::InvalidEvent)?;
                let client = SupplierClient::new(&runtime.base_url, &runtime.api_key)
                    .map_err(|_| SupplierServiceError::SupplierConfiguration)?;
                (count, client)
            }
            _ => unreachable!("event type was validated before purchase"),
        };

        let order_id = event
            .purchase_order_id
            .as_deref()
            .ok_or(SupplierServiceError::InvalidEvent)?;
        let purchase = client
            .purchase(count, order_id)
            .await
            .map_err(|_| SupplierServiceError::SupplierApi)?;
        let mut summary = ProcessSummary {
            purchased_count: i64::from(purchase.purchased),
            ..empty_summary()
        };
        for (index, key) in purchase.keys.into_iter().enumerate() {
            let credential =
                credential_from_supplier_key(key.into_inner(), &runtime, order_id, index + 1);
            match importer.import(credential).await {
                Ok(()) => summary.imported_count += 1,
                Err(error) if is_duplicate_error(&error.to_string()) => {
                    summary.duplicate_count += 1
                }
                Err(_) => summary.failed_count += 1,
            }
        }
        Ok(ProcessAction::Complete(summary))
    }
}

enum ProcessAction {
    Complete(ProcessSummary),
    Skip,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ManualPurchaseResult {
    pub order_id: String,
    pub requested: u32,
    pub purchased: u32,
    pub imported: u32,
    pub duplicate: u32,
    pub failed: u32,
}

impl fmt::Debug for ManualPurchaseResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManualPurchaseResult")
            .field("order_id", &self.order_id)
            .field("requested", &self.requested)
            .field("purchased", &self.purchased)
            .field("imported", &self.imported)
            .field("duplicate", &self.duplicate)
            .field("failed", &self.failed)
            .finish()
    }
}

fn empty_summary() -> ProcessSummary {
    ProcessSummary {
        purchased_count: 0,
        imported_count: 0,
        duplicate_count: 0,
        failed_count: 0,
        message: None,
    }
}

fn credential_from_supplier_key(
    key: String,
    runtime: &SupplierRuntimeConfig,
    order_id: &str,
    index: usize,
) -> KiroCredentials {
    let suffix = format!("{}-{index}", &order_id[..8]);
    let prefix_len = 128usize.saturating_sub(suffix.chars().count());
    let nickname = format!(
        "{}{}",
        runtime
            .nickname_prefix
            .chars()
            .take(prefix_len)
            .collect::<String>(),
        suffix
    );
    KiroCredentials {
        auth_method: Some("api_key".to_owned()),
        kiro_api_key: Some(key),
        auth_region: Some(API_KEY_AUTH_REGION.to_owned()),
        api_region: Some(runtime.api_region.clone()),
        rpm_limit: runtime.rpm_limit,
        priority: runtime.priority,
        groups: runtime.groups.clone(),
        source_channel: Some(runtime.source_channel.clone()),
        nickname: Some(nickname),
        ..Default::default()
    }
}

fn is_duplicate_error(error: &str) -> bool {
    error.contains("凭据已存在") || error.contains("kiroApiKey 重复")
}

fn sanitize_error(error: &str, runtime: &SupplierRuntimeConfig) -> String {
    let without_runtime_key = error.replace(&runtime.api_key, "[REDACTED]");
    let without_supplier_keys = regex::Regex::new(r#"ksk_[^\s\"'<>]*"#)
        .expect("supplier key redaction pattern is valid")
        .replace_all(&without_runtime_key, "[REDACTED]");
    without_supplier_keys.chars().take(300).collect()
}

#[derive(Clone, PartialEq, Eq)]
pub struct IngestResult {
    pub duplicate: bool,
    pub event_id: String,
    pub event_type: String,
}

impl fmt::Debug for IngestResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IngestResult")
            .field("duplicate", &self.duplicate)
            .field("event_id", &self.event_id)
            .field("event_type", &self.event_type)
            .finish()
    }
}

pub enum SupplierServiceError {
    Unauthorized,
    InvalidJson,
    InvalidPayload,
    InvalidEvent,
    InvalidPurchaseQuantity,
    SupplierConfiguration,
    SupplierApi,
    ImporterUnavailable,
    Store,
}

impl fmt::Display for SupplierServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unauthorized => "webhook authentication failed",
            Self::InvalidJson => "invalid webhook JSON",
            Self::InvalidPayload => "invalid webhook payload",
            Self::InvalidEvent => "invalid supplier event",
            Self::InvalidPurchaseQuantity => {
                "manual purchase quantity is outside configured bounds"
            }
            Self::SupplierConfiguration => "supplier configuration is invalid",
            Self::SupplierApi => "supplier API request failed",
            Self::ImporterUnavailable => "credential importer is unavailable",
            Self::Store => "supplier event store unavailable",
        })
    }
}

impl fmt::Debug for SupplierServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unauthorized => "Unauthorized",
            Self::InvalidJson => "InvalidJson",
            Self::InvalidPayload => "InvalidPayload",
            Self::InvalidEvent => "InvalidEvent",
            Self::InvalidPurchaseQuantity => "InvalidPurchaseQuantity",
            Self::SupplierConfiguration => "SupplierConfiguration",
            Self::SupplierApi => "SupplierApi",
            Self::ImporterUnavailable => "ImporterUnavailable",
            Self::Store => "Store",
        })
    }
}

impl std::error::Error for SupplierServiceError {}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::admin::key_supplier::config::SupplierRuntimeConfig;
    use crate::admin::key_supplier::store::{SupplierEventStatus, SupplierEventStore};
    use crate::kiro::model::credentials::KiroCredentials;
    use axum::{
        Router,
        response::IntoResponse,
        routing::{get, post},
    };
    use tokio::net::TcpListener;

    const TOKEN: &str = "webhook-token-canary";
    const EVENT_ID: &str = "0123456789abcdef0123456789abcdef";
    const ORDER_ID: &str = "fedcba9876543210fedcba9876543210";

    #[derive(Default)]
    struct FakeImporter {
        credentials: Mutex<Vec<KiroCredentials>>,
        outcomes: Mutex<VecDeque<anyhow::Result<()>>>,
    }

    impl FakeImporter {
        fn with_outcomes(outcomes: Vec<anyhow::Result<()>>) -> Self {
            Self {
                credentials: Mutex::new(Vec::new()),
                outcomes: Mutex::new(outcomes.into()),
            }
        }
    }

    impl CredentialImporter for FakeImporter {
        fn import(
            &self,
            credential: KiroCredentials,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>>
        {
            self.credentials.lock().unwrap().push(credential);
            let outcome = self.outcomes.lock().unwrap().pop_front().unwrap_or(Ok(()));
            Box::pin(async move { outcome })
        }
    }

    async fn server(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router).into_future());
        format!("http://{address}")
    }

    fn purchase_json(order_id: &str, keys: &[&str]) -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "client_order_id": order_id,
            "purchased": keys.len(),
            "remaining": 9,
            "keys": keys.iter().map(|key| serde_json::json!({"key": key})).collect::<Vec<_>>(),
        }))
    }

    fn runtime(token: &str) -> SupplierRuntimeConfig {
        SupplierRuntimeConfig {
            base_url: String::new(),
            api_key: "ksk-canary".to_string(),
            public_base_url: String::new(),
            webhook_token: token.to_string(),
            auto_purchase: false,
            min_purchase: 1,
            max_purchase: 10,
            api_region: "us-east-1".to_string(),
            rpm_limit: 0,
            priority: 0,
            groups: Vec::new(),
            source_channel: String::new(),
            nickname_prefix: String::new(),
        }
    }

    fn service(token: &str) -> (KeySupplierService, Arc<SupplierEventStore>) {
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::new(store.clone(), runtime(token));
        (service, store)
    }

    fn queued_event(
        store: &SupplierEventStore,
        event_type: &str,
        order_id: Option<&str>,
        quantity: i64,
    ) {
        store
            .insert_event(IncomingSupplierEvent {
                event_id: format!("{:032x}", quantity + 100),
                event_type: event_type.to_owned(),
                purchase_order_id: order_id.map(str::to_owned),
                message: Some("event message".to_owned()),
                quantity,
            })
            .unwrap();
    }

    #[test]
    fn parses_both_supported_webhook_events() {
        let new_keys = IncomingWebhook::parse(
            format!(
                r#"{{"event":"new_keys_available","event_id":"{EVENT_ID}","purchase_order_id":"{ORDER_ID}","message":"ready","new_keys":3}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            new_keys,
            IncomingWebhook::NewKeysAvailable { new_keys: 3, .. }
        ));

        let dead = IncomingWebhook::parse(
            format!(
                r#"{{"event":"all_keys_dead","event_id":"{EVENT_ID}","message":"dead","dead":2}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(matches!(dead, IncomingWebhook::AllKeysDead { dead: 2, .. }));
    }

    #[test]
    fn rejects_malformed_or_unsupported_webhooks() {
        for body in [
            "{",
            r#"{"event":"unknown","event_id":"0123456789abcdef0123456789abcdef","message":"x","new_keys":1}"#,
            r#"{"event":"all_keys_dead","event_id":"bad","message":"x","dead":1}"#,
            r#"{"event":"all_keys_dead","event_id":"","message":"x","dead":1}"#,
            r#"{"event":"all_keys_dead","event_id":"0123456789abcdef0123456789abcdef","message":"x","dead":0}"#,
        ] {
            assert!(IncomingWebhook::parse(body.as_bytes()).is_err(), "{body}");
        }
    }

    #[test]
    fn authenticates_and_ingests_idempotently() {
        let (service, store) = service(TOKEN);
        let body = format!(
            r#"{{"event":"new_keys_available","event_id":"{EVENT_ID}","purchase_order_id":"{ORDER_ID}","message":"ready","new_keys":3}}"#
        );

        let first = service.ingest(TOKEN, body.as_bytes()).unwrap();
        let second = service.ingest(TOKEN, body.as_bytes()).unwrap();

        assert!(!first.duplicate);
        assert!(second.duplicate);
        assert_eq!(first.event_id, EVENT_ID);
        assert_eq!(first.event_type, "new_keys_available");
        let page = store.list(10, None).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].webhook_duplicate_count, 1);
        assert_eq!(page.items[0].quantity, 3);
        assert_eq!(page.items[0].purchase_order_id.as_deref(), Some(ORDER_ID));
    }

    #[test]
    fn rejects_empty_or_wrong_tokens_without_leaking_sensitive_values() {
        let (service, _) = service(TOKEN);
        let body = format!(
            r#"{{"event":"all_keys_dead","event_id":"{EVENT_ID}","message":"body-canary","dead":1}}"#
        );
        for token in ["", "wrong-token"] {
            let error = service.ingest(token, body.as_bytes()).unwrap_err();
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains(TOKEN));
            assert!(!rendered.contains("ksk-canary"));
            assert!(!rendered.contains("body-canary"));
        }
    }

    #[test]
    fn store_truncates_long_messages_and_debug_is_safe() {
        let (service, store) = service(TOKEN);
        let message = "m".repeat(2_001);
        let body = format!(
            r#"{{"event":"all_keys_dead","event_id":"{EVENT_ID}","message":"{message}","dead":1}}"#
        );
        let result = service.ingest(TOKEN, body.as_bytes()).unwrap();
        assert!(!result.duplicate);
        let item = &store.list(10, None).unwrap().items[0];
        assert_eq!(item.message.as_ref().unwrap().chars().count(), 2_000);
        assert_eq!(item.status, SupplierEventStatus::Received);
    }

    #[test]
    fn purchase_count_respects_event_stock_and_configured_bounds() {
        assert_eq!(
            select_purchase_count(20, 8, 5, 2),
            CountDecision::Purchase(5)
        );
        assert_eq!(select_purchase_count(3, 1, 10, 2), CountDecision::Skip);
    }

    #[tokio::test]
    async fn auto_purchase_uses_webhook_order_and_stock_bounds() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen_stock = requests.clone();
        let seen_purchase = requests.clone();
        let app = Router::new()
            .route(
                "/api/my/stock",
                get(move || {
                    let seen = seen_stock.clone();
                    async move {
                        seen.lock().unwrap().push("stock".to_owned());
                        axum::Json(serde_json::json!({"max": 4}))
                    }
                }),
            )
            .route(
                "/api/my/purchase",
                post(move |request: axum::http::Request<axum::body::Body>| {
                    let seen = seen_purchase.clone();
                    async move {
                        let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                            .await
                            .unwrap();
                        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        seen.lock()
                            .unwrap()
                            .push(serde_json::to_string(&value).unwrap());
                        purchase_json(
                            value["client_order_id"].as_str().unwrap(),
                            &["ksk_purchase_canary"],
                        )
                    }
                }),
            );
        let mut config = runtime(TOKEN);
        config.base_url = server(app).await;
        config.auto_purchase = true;
        config.min_purchase = 2;
        config.max_purchase = 3;
        let importer = Arc::new(FakeImporter::default());
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        queued_event(&store, "new_keys_available", Some(ORDER_ID), 8);
        let service = KeySupplierService::with_importer(store.clone(), config, importer);

        assert_eq!(service.process_pending().await.unwrap(), 1);
        let request_log = requests.lock().unwrap();
        assert_eq!(request_log[0], "stock");
        let purchase: serde_json::Value = serde_json::from_str(&request_log[1]).unwrap();
        assert_eq!(purchase["client_order_id"], ORDER_ID);
        assert_eq!(purchase["count"], 3);
        let item = &store.list(1, None).unwrap().items[0];
        assert_eq!(item.status, SupplierEventStatus::Succeeded);
        assert_eq!(item.purchased_count, 1);
    }

    #[tokio::test]
    async fn imported_credentials_use_the_runtime_template() {
        let app = Router::new()
            .route(
                "/api/my/stock",
                get(|| async { axum::Json(serde_json::json!({"max": 1})) }),
            )
            .route(
                "/api/my/purchase",
                post(|| async { purchase_json(ORDER_ID, &["ksk_template_canary"]) }),
            );
        let mut config = runtime(TOKEN);
        config.base_url = server(app).await;
        config.auto_purchase = true;
        config.rpm_limit = 37;
        config.priority = 9;
        config.groups = vec!["g1".to_owned(), "g2".to_owned()];
        config.source_channel = "supplier-a".to_owned();
        config.nickname_prefix = "supplier-".to_owned();
        let importer = Arc::new(FakeImporter::default());
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        queued_event(&store, "new_keys_available", Some(ORDER_ID), 1);
        let service = KeySupplierService::with_importer(store, config.clone(), importer.clone());

        service.process_pending().await.unwrap();
        let credentials = importer.credentials.lock().unwrap();
        let credential = &credentials[0];
        assert_eq!(credential.auth_method.as_deref(), Some("api_key"));
        assert_eq!(
            credential.kiro_api_key.as_deref(),
            Some("ksk_template_canary")
        );
        assert_eq!(
            credential.auth_region.as_deref(),
            Some(crate::kiro::region::API_KEY_AUTH_REGION)
        );
        assert_eq!(
            credential.api_region.as_deref(),
            Some(config.api_region.as_str())
        );
        assert_eq!(credential.rpm_limit, 37);
        assert_eq!(credential.priority, 9);
        assert_eq!(credential.groups, config.groups);
        assert_eq!(credential.source_channel.as_deref(), Some("supplier-a"));
        assert_eq!(credential.nickname.as_deref(), Some("supplier-fedcba98-1"));
    }

    #[tokio::test]
    async fn import_outcomes_are_counted_without_rolling_back_purchase() {
        let app = Router::new()
            .route(
                "/api/my/stock",
                get(|| async { axum::Json(serde_json::json!({"max": 3})) }),
            )
            .route(
                "/api/my/purchase",
                post(|| async {
                    purchase_json(
                        ORDER_ID,
                        &[
                            "ksk_success_canary",
                            "ksk_duplicate_canary",
                            "ksk_failed_canary",
                        ],
                    )
                }),
            );
        let mut config = runtime(TOKEN);
        config.base_url = server(app).await;
        config.auto_purchase = true;
        let importer = Arc::new(FakeImporter::with_outcomes(vec![
            Ok(()),
            Err(anyhow::anyhow!("凭据已存在")),
            Err(anyhow::anyhow!("other failure")),
        ]));
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        queued_event(&store, "new_keys_available", Some(ORDER_ID), 3);
        let service = KeySupplierService::with_importer(store.clone(), config, importer);

        service.process_pending().await.unwrap();
        let item = &store.list(1, None).unwrap().items[0];
        assert_eq!(
            (
                item.purchased_count,
                item.imported_count,
                item.duplicate_count,
                item.failed_count
            ),
            (3, 1, 1, 1)
        );
        assert_eq!(item.status, SupplierEventStatus::Succeeded);
    }

    #[tokio::test]
    async fn disabled_and_below_minimum_events_skip_without_purchase() {
        let calls = Arc::new(Mutex::new(0));
        let stock_calls = calls.clone();
        let purchase_calls = calls.clone();
        let app = Router::new()
            .route(
                "/api/my/stock",
                get(move || {
                    let calls = stock_calls.clone();
                    async move {
                        *calls.lock().unwrap() += 1;
                        axum::Json(serde_json::json!({"max": 1}))
                    }
                }),
            )
            .route(
                "/api/my/purchase",
                post(move || {
                    let calls = purchase_calls.clone();
                    async move {
                        *calls.lock().unwrap() += 100;
                        purchase_json(ORDER_ID, &["ksk_should_not_import"])
                    }
                }),
            );
        let mut disabled = runtime(TOKEN);
        disabled.base_url = server(app).await;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        queued_event(&store, "new_keys_available", Some(ORDER_ID), 3);
        let service = KeySupplierService::with_importer(
            store.clone(),
            disabled,
            Arc::new(FakeImporter::default()),
        );
        service.process_pending().await.unwrap();
        assert_eq!(
            store.list(1, None).unwrap().items[0].status,
            SupplierEventStatus::Skipped
        );

        let mut below = service.runtime_config();
        below.auto_purchase = true;
        below.min_purchase = 2;
        service.set_runtime_config(below);
        store
            .insert_event(IncomingSupplierEvent {
                event_id: "1123456789abcdef0123456789abcdef".to_owned(),
                event_type: "new_keys_available".to_owned(),
                purchase_order_id: Some("0123456789abcdef0123456789abcdef".to_owned()),
                message: Some("below minimum".to_owned()),
                quantity: 3,
            })
            .unwrap();
        service.process_pending().await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(
            store
                .list(10, None)
                .unwrap()
                .items
                .iter()
                .all(|event| event.status == SupplierEventStatus::Skipped)
        );
    }

    #[tokio::test]
    async fn all_keys_dead_completes_without_supplier_or_importer_access() {
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        queued_event(&store, "all_keys_dead", None, 2);
        let importer = Arc::new(FakeImporter::default());
        let service =
            KeySupplierService::with_importer(store.clone(), runtime(TOKEN), importer.clone());
        assert_eq!(service.process_pending().await.unwrap(), 1);
        let item = &store.list(1, None).unwrap().items[0];
        assert_eq!(item.status, SupplierEventStatus::Succeeded);
        assert_eq!(item.purchased_count, 0);
        assert!(importer.credentials.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn manual_purchase_generates_ids_records_history_and_skips_stock() {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let purchase_paths = paths.clone();
        let app = Router::new().route("/api/my/purchase", post(move |request: axum::http::Request<axum::body::Body>| { let paths = purchase_paths.clone(); async move { paths.lock().unwrap().push(request.uri().path().to_owned()); let body = axum::body::to_bytes(request.into_body(), usize::MAX).await.unwrap(); let order = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["client_order_id"].as_str().unwrap().to_owned(); purchase_json(&order, &["ksk_manual_canary"]) } }));
        let mut config = runtime(TOKEN);
        config.base_url = server(app).await;
        config.min_purchase = 2;
        config.max_purchase = 5;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_importer(
            store.clone(),
            config,
            Arc::new(FakeImporter::default()),
        );
        let result = service.manual_purchase(3).await.unwrap();
        assert_eq!(result.requested, 3);
        assert_eq!(result.purchased, 1);
        assert_eq!(result.imported, 1);
        assert_eq!(result.order_id.len(), 32);
        assert!(result.order_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(*paths.lock().unwrap(), vec!["/api/my/purchase"]);
        let item = &store.list(1, None).unwrap().items[0];
        assert_eq!(item.event_type, "manual_purchase");
        assert_eq!(
            item.purchase_order_id.as_deref(),
            Some(result.order_id.as_str())
        );
    }

    #[tokio::test]
    async fn api_failures_are_retriable_and_errors_never_contain_keys() {
        let attempts = Arc::new(Mutex::new(0));
        let state = attempts.clone();
        let app = Router::new()
            .route(
                "/api/my/stock",
                get(move || {
                    let attempts = state.clone();
                    async move {
                        let mut attempts = attempts.lock().unwrap();
                        *attempts += 1;
                        if *attempts <= 3 {
                            (
                                axum::http::StatusCode::BAD_GATEWAY,
                                "ksk_api_failure_canary",
                            )
                                .into_response()
                        } else {
                            axum::Json(serde_json::json!({"max": 1})).into_response()
                        }
                    }
                }),
            )
            .route(
                "/api/my/purchase",
                post(|| async { purchase_json(ORDER_ID, &["ksk_retried_canary"]) }),
            );
        let mut config = runtime(TOKEN);
        config.base_url = server(app).await;
        config.auto_purchase = true;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        queued_event(&store, "new_keys_available", Some(ORDER_ID), 1);
        let service = KeySupplierService::with_importer(
            store.clone(),
            config,
            Arc::new(FakeImporter::default()),
        );
        service.process_pending().await.unwrap();
        let failed = store.list(1, None).unwrap().items.remove(0);
        assert_eq!(failed.status, SupplierEventStatus::Failed);
        assert!(!format!("{failed:?}").contains("ksk_api_failure_canary"));
        store.retry(failed.id).unwrap();
        service.process_pending().await.unwrap();
        assert_eq!(
            store.list(1, None).unwrap().items[0].status,
            SupplierEventStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn missing_importer_and_configuration_errors_are_redacted() {
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        queued_event(&store, "new_keys_available", Some(ORDER_ID), 1);
        let mut config = runtime(TOKEN);
        config.auto_purchase = true;
        config.base_url = "http://bad-path/ksk_config_canary".to_owned();
        let service = KeySupplierService::new(store.clone(), config);
        service.process_pending().await.unwrap();
        let item = &store.list(1, None).unwrap().items[0];
        assert_eq!(item.status, SupplierEventStatus::Failed);
        let rendered = format!("{item:?}");
        assert!(!rendered.contains("ksk_config_canary"));
    }

    #[tokio::test]
    async fn missing_importer_fails_without_contacting_the_supplier() {
        let calls = Arc::new(Mutex::new(0));
        let seen = calls.clone();
        let app = Router::new().fallback(move || {
            let seen = seen.clone();
            async move {
                *seen.lock().unwrap() += 1;
                axum::http::StatusCode::NO_CONTENT
            }
        });
        let mut config = runtime(TOKEN);
        config.base_url = server(app).await;
        config.auto_purchase = true;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        queued_event(&store, "new_keys_available", Some(ORDER_ID), 1);
        let service = KeySupplierService::new(store.clone(), config);

        service.process_pending().await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 0);
        assert_eq!(
            store.list(1, None).unwrap().items[0].status,
            SupplierEventStatus::Failed
        );
    }
}
use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::admin::key_supplier::config::SupplierRuntimeConfig;
use crate::admin::key_supplier::store::{IncomingSupplierEvent, InsertOutcome, SupplierEventStore};
