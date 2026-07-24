use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::admin::key_supplier::client::{Profile, Stock, SupplierClient, SupplierStatus};
use crate::admin::key_supplier::config::{
    SupplierConfigUpdate, SupplierConfigView, SupplierRuntimeConfig, is_valid_webhook_token,
};
use crate::admin::key_supplier::store::{
    IncomingSupplierEvent, InsertOutcome, ProcessSummary, StoredSupplierEvent, SupplierEventStore,
};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::region::API_KEY_AUTH_REGION;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::Config;

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
    Test {
        event_id: String,
        message: String,
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
            "test" => Ok(Self::Test { event_id, message }),
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
            Self::Test { event_id, message } => IncomingSupplierEvent {
                event_id,
                event_type: "test".to_string(),
                purchase_order_id: None,
                message: Some(message),
                quantity: 0,
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
    config_path: Option<PathBuf>,
    config_update_lock: parking_lot::Mutex<()>,
    processor_started: AtomicBool,
}

impl fmt::Debug for KeySupplierService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeySupplierService")
            .field("runtime", &*self.runtime.read())
            .field("config_path", &self.config_path)
            .field(
                "processor_started",
                &self.processor_started.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl KeySupplierService {
    pub fn new(store: Arc<SupplierEventStore>, runtime: SupplierRuntimeConfig) -> Self {
        Self {
            store,
            runtime: parking_lot::RwLock::new(runtime),
            importer: None,
            processing_lock: tokio::sync::Mutex::new(()),
            config_path: None,
            config_update_lock: parking_lot::Mutex::new(()),
            processor_started: AtomicBool::new(false),
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
            config_path: None,
            config_update_lock: parking_lot::Mutex::new(()),
            processor_started: AtomicBool::new(false),
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

    pub fn with_config_path(mut self, config_path: impl AsRef<Path>) -> Self {
        self.config_path = Some(config_path.as_ref().to_path_buf());
        self
    }

    pub fn set_runtime_config(&self, runtime: SupplierRuntimeConfig) {
        *self.runtime.write() = runtime;
    }

    pub fn store(&self) -> Arc<SupplierEventStore> {
        self.store.clone()
    }

    async fn run_store_operation<T, F>(&self, operation: F) -> Result<T, SupplierServiceError>
    where
        T: Send + 'static,
        F: FnOnce(&SupplierEventStore) -> rusqlite::Result<T> + Send + 'static,
    {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || operation(store.as_ref()))
            .await
            .map_err(|_| SupplierServiceError::Store)?
            .map_err(|_| SupplierServiceError::Store)
    }

    pub fn has_valid_webhook_token(&self, token: &str) -> bool {
        let runtime = self.runtime_config();
        is_valid_webhook_token(&runtime.webhook_token)
            && crate::common::auth::constant_time_eq(&runtime.webhook_token, token)
    }

    pub fn config_view(&self) -> SupplierConfigView {
        SupplierConfigView::from(&self.runtime_config())
    }

    pub fn update_config(
        &self,
        update: SupplierConfigUpdate,
    ) -> Result<SupplierConfigView, SupplierServiceError> {
        let _guard = self.config_update_lock.lock();
        let path = self
            .config_path
            .as_ref()
            .ok_or(SupplierServiceError::ConfigPathUnavailable)?;
        let mut config = Config::load(path).map_err(|_| SupplierServiceError::ConfigPersistence)?;
        let runtime = SupplierRuntimeConfig::apply(&mut config, update)
            .map_err(|_| SupplierServiceError::SupplierConfiguration)?;
        config
            .save()
            .map_err(|_| SupplierServiceError::ConfigPersistence)?;
        *self.runtime.write() = runtime.clone();
        Ok(SupplierConfigView::from(&runtime))
    }

    fn supplier_client(&self) -> Result<SupplierClient, SupplierServiceError> {
        let runtime = self.runtime_config();
        if runtime.base_url.trim().is_empty() || runtime.api_key.trim().is_empty() {
            return Err(SupplierServiceError::SupplierConfiguration);
        }
        SupplierClient::new(&runtime.base_url, &runtime.api_key)
            .map_err(|_| SupplierServiceError::SupplierConfiguration)
    }

    pub async fn overview(&self) -> Result<SupplierOverview, SupplierServiceError> {
        let client = self.supplier_client()?;
        let profile = client
            .profile()
            .await
            .map_err(SupplierServiceError::supplier_api)?;
        let webhook_registered = self
            .callback_url()
            .ok()
            .is_some_and(|callback| profile.webhook_url == callback);
        let stock = client
            .stock()
            .await
            .map_err(SupplierServiceError::supplier_api)?;
        let status = client
            .status()
            .await
            .map_err(SupplierServiceError::supplier_api)?;
        Ok(SupplierOverview {
            profile,
            stock,
            status,
            webhook_registered,
        })
    }

    pub fn callback_url(&self) -> Result<String, SupplierServiceError> {
        let runtime = self.runtime_config();
        if runtime.webhook_token.len() != 64
            || !runtime
                .webhook_token
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || runtime.public_base_url.trim().is_empty()
        {
            return Err(SupplierServiceError::SupplierConfiguration);
        }
        SupplierClient::new(&runtime.public_base_url, "callback-validation")
            .map_err(|_| SupplierServiceError::SupplierConfiguration)?;
        let mut callback = reqwest::Url::parse(&runtime.public_base_url)
            .map_err(|_| SupplierServiceError::SupplierConfiguration)?;
        callback.set_path(&format!(
            "/api/admin/key-supplier/webhook/{}",
            runtime.webhook_token
        ));
        callback.set_query(None);
        callback.set_fragment(None);
        Ok(callback.into())
    }

    pub async fn register_webhook(&self) -> Result<String, SupplierServiceError> {
        let callback = self.callback_url()?;
        self.supplier_client()?
            .register_webhook(&callback)
            .await
            .map_err(SupplierServiceError::supplier_api)?;
        Ok(callback)
    }

    pub async fn test_webhook(&self) -> Result<(), SupplierServiceError> {
        self.supplier_client()?
            .test_webhook()
            .await
            .map_err(SupplierServiceError::supplier_api)
    }

    pub fn retry_event(&self, id: i64) -> Result<(), SupplierServiceError> {
        self.store
            .retry(id)
            .map_err(|_| SupplierServiceError::Store)
    }

    pub async fn run_processing_cycle(&self) -> Result<usize, SupplierServiceError> {
        let _guard = self.processing_lock.lock().await;
        let cutoff = Utc::now() - ChronoDuration::minutes(5);
        self.run_store_operation(move |store| store.recover_stale_processing(cutoff))
            .await?;
        self.process_pending_locked().await
    }

    pub fn start_processor(self: &Arc<Self>) -> bool {
        if self
            .processor_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let service = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = service.run_processing_cycle().await {
                tracing::warn!(
                    kind = processing_error_kind(&error),
                    "supplier processor cycle failed"
                );
            }
            let mut interval = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(30),
                Duration::from_secs(30),
            );
            loop {
                interval.tick().await;
                if let Err(error) = service.run_processing_cycle().await {
                    tracing::warn!(
                        kind = processing_error_kind(&error),
                        "supplier processor cycle failed"
                    );
                }
            }
        });
        true
    }

    pub fn ingest<B: AsRef<[u8]>>(
        &self,
        token: &str,
        body: B,
    ) -> Result<IngestResult, SupplierServiceError> {
        if !self.has_valid_webhook_token(token) {
            return Err(SupplierServiceError::Unauthorized);
        }
        let runtime = self.runtime_config();

        let webhook = IncomingWebhook::parse(body.as_ref())?;
        let mut event = webhook.into_event();
        event.message = event
            .message
            .map(|message| redact_runtime_secrets(&message, &runtime));
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
        self.process_pending_locked().await
    }

    async fn process_pending_locked(&self) -> Result<usize, SupplierServiceError> {
        let mut processed = 0;
        while let Some(event) = self
            .run_store_operation(SupplierEventStore::claim_next)
            .await?
        {
            if let Err(error) = self.process_claimed(event).await {
                tracing::warn!(
                    kind = processing_error_kind(&error),
                    "supplier event processing failed"
                );
            }
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
        let event = IncomingSupplierEvent {
            event_id: order_id.clone(),
            event_type: "manual_purchase".to_owned(),
            purchase_order_id: Some(order_id.clone()),
            message: None,
            quantity: i64::from(count),
        };
        self.run_store_operation(move |store| store.insert_event(event))
            .await?;
        let lookup_order_id = order_id.clone();
        let event = self
            .run_store_operation(move |store| store.claim_by_event_id(&lookup_order_id))
            .await?
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
                let stored_summary = summary.clone();
                self.run_store_operation(move |store| store.complete(event.id, stored_summary))
                    .await?;
                Ok(summary)
            }
            Ok(ProcessAction::Skip) => {
                self.run_store_operation(move |store| {
                    store.skip(event.id, Some("purchase skipped"))
                })
                .await?;
                Ok(empty_summary())
            }
            Ok(ProcessAction::Failed { summary, error }) => {
                let persistence_error =
                    sanitize_error(&error.persistence_detail(), &self.runtime_config());
                self.run_store_operation(move |store| {
                    store.fail_with_summary(event.id, summary, &persistence_error)
                })
                .await?;
                Err(error)
            }
            Err(error) => {
                let persistence_error =
                    sanitize_error(&error.persistence_detail(), &self.runtime_config());
                self.run_store_operation(move |store| store.fail(event.id, &persistence_error))
                    .await?;
                Err(error)
            }
        }
    }

    async fn execute_claimed(
        &self,
        event: &StoredSupplierEvent,
    ) -> Result<ProcessAction, SupplierServiceError> {
        if matches!(event.event_type.as_str(), "all_keys_dead" | "test") {
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
                let client = self.supplier_client()?;
                let stock = client
                    .stock()
                    .await
                    .map_err(SupplierServiceError::supplier_api)?;
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
                let client = self.supplier_client()?;
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
            .map_err(SupplierServiceError::supplier_api)?;
        let mut summary = ProcessSummary {
            purchased_count: i64::from(purchase.purchased),
            ..empty_summary()
        };
        let mut import_failed = false;
        for (index, key) in purchase.keys.into_iter().enumerate() {
            let credential =
                credential_from_supplier_key(key.into_inner(), &runtime, order_id, index + 1);
            match importer.import(credential).await {
                Ok(()) => summary.imported_count += 1,
                Err(error) if is_duplicate_error(&error.to_string()) => {
                    summary.duplicate_count += 1
                }
                Err(_) => {
                    summary.failed_count += 1;
                    import_failed = true;
                }
            }
        }
        if import_failed {
            Ok(ProcessAction::Failed {
                summary,
                error: SupplierServiceError::ImportFailed,
            })
        } else {
            Ok(ProcessAction::Complete(summary))
        }
    }
}

enum ProcessAction {
    Complete(ProcessSummary),
    Skip,
    Failed {
        summary: ProcessSummary,
        error: SupplierServiceError,
    },
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
        delete_on_forbidden: runtime.auto_delete_forbidden,
        nickname: Some(nickname),
        ..Default::default()
    }
}

fn is_duplicate_error(error: &str) -> bool {
    error.contains("凭据已存在") || error.contains("kiroApiKey 重复")
}

fn sanitize_error(error: &str, runtime: &SupplierRuntimeConfig) -> String {
    redact_runtime_secrets(error, runtime)
        .chars()
        .take(300)
        .collect()
}

fn redact_runtime_secrets(value: &str, runtime: &SupplierRuntimeConfig) -> String {
    let without_runtime_api_key = if runtime.api_key.is_empty() {
        value.to_owned()
    } else {
        value.replace(&runtime.api_key, "[REDACTED]")
    };
    let without_runtime_secrets = if runtime.webhook_token.is_empty() {
        without_runtime_api_key
    } else {
        without_runtime_api_key.replace(&runtime.webhook_token, "[REDACTED]")
    };
    let without_supplier_keys = regex::Regex::new(r#"ksk_[^\s\"'<>]*"#)
        .expect("supplier key redaction pattern is valid")
        .replace_all(&without_runtime_secrets, "[REDACTED]");
    without_supplier_keys.into_owned()
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

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplierOverview {
    pub profile: Profile,
    pub stock: Stock,
    pub status: SupplierStatus,
    pub webhook_registered: bool,
}

impl fmt::Debug for SupplierOverview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SupplierOverview")
            .field("profile", &self.profile)
            .field("stock_max", &self.stock.max)
            .field("status", &self.status)
            .field("webhook_registered", &self.webhook_registered)
            .finish()
    }
}

fn processing_error_kind(error: &SupplierServiceError) -> &'static str {
    match error {
        SupplierServiceError::Store => "store",
        SupplierServiceError::SupplierApi { .. } => "supplier_api",
        SupplierServiceError::SupplierConfiguration => "configuration",
        SupplierServiceError::ImporterUnavailable => "importer",
        _ => "other",
    }
}

pub enum SupplierServiceError {
    Unauthorized,
    InvalidJson,
    InvalidPayload,
    InvalidEvent,
    InvalidPurchaseQuantity,
    SupplierConfiguration,
    SupplierApi { diagnostic: String },
    ImporterUnavailable,
    ImportFailed,
    Store,
    ConfigPathUnavailable,
    ConfigPersistence,
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
            Self::SupplierApi { .. } => "supplier API request failed",
            Self::ImporterUnavailable => "credential importer is unavailable",
            Self::ImportFailed => "credential import failed",
            Self::Store => "supplier event store unavailable",
            Self::ConfigPathUnavailable => "supplier configuration path is unavailable",
            Self::ConfigPersistence => "supplier configuration could not be persisted",
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
            Self::SupplierApi { .. } => "SupplierApi",
            Self::ImporterUnavailable => "ImporterUnavailable",
            Self::ImportFailed => "ImportFailed",
            Self::Store => "Store",
            Self::ConfigPathUnavailable => "ConfigPathUnavailable",
            Self::ConfigPersistence => "ConfigPersistence",
        })
    }
}

impl std::error::Error for SupplierServiceError {}

impl SupplierServiceError {
    fn supplier_api(error: crate::admin::key_supplier::client::SupplierError) -> Self {
        Self::SupplierApi {
            diagnostic: error.to_string(),
        }
    }

    fn persistence_detail(&self) -> String {
        match self {
            Self::SupplierApi { diagnostic } => diagnostic.clone(),
            _ => self.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::admin::key_supplier::config::{SupplierConfigUpdate, SupplierRuntimeConfig};
    use crate::admin::key_supplier::store::{
        IncomingSupplierEvent, SupplierEventStatus, SupplierEventStore,
    };
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::{Config, KeySupplierConfig};
    use axum::{
        Router,
        response::IntoResponse,
        routing::{get, post, put},
    };
    use chrono::{Duration, Utc};
    use tokio::net::TcpListener;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
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

    struct BlockingImporter {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl CredentialImporter for BlockingImporter {
        fn import(
            &self,
            _credential: KiroCredentials,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>>
        {
            let started = self.started.clone();
            let release = self.release.clone();
            Box::pin(async move {
                started.notify_one();
                release.notified().await;
                Ok(())
            })
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
            auto_delete_forbidden: false,
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

    #[tokio::test(flavor = "current_thread")]
    async fn async_store_operations_run_on_the_blocking_pool() {
        let (service, _) = service(TOKEN);
        let worker_thread = std::thread::current().id();

        let store_thread = service
            .run_store_operation(|_| Ok::<_, rusqlite::Error>(std::thread::current().id()))
            .await
            .unwrap();

        assert_ne!(store_thread, worker_thread);
    }

    fn supplier_update(runtime: &SupplierRuntimeConfig) -> SupplierConfigUpdate {
        SupplierConfigUpdate {
            base_url: runtime.base_url.clone(),
            api_key: None,
            public_base_url: runtime.public_base_url.clone(),
            webhook_token: None,
            auto_purchase: runtime.auto_purchase,
            auto_delete_forbidden: runtime.auto_delete_forbidden,
            min_purchase: u64::from(runtime.min_purchase),
            max_purchase: u64::from(runtime.max_purchase),
            api_region: runtime.api_region.clone(),
            rpm_limit: u64::from(runtime.rpm_limit),
            priority: u64::from(runtime.priority),
            groups: runtime.groups.clone(),
            source_channel: runtime.source_channel.clone(),
            nickname_prefix: runtime.nickname_prefix.clone(),
        }
    }

    fn temp_config_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kiro-supplier-service-{label}-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    fn persistent_service(runtime: SupplierRuntimeConfig) -> (Arc<KeySupplierService>, PathBuf) {
        let path = temp_config_path("config");
        let mut config = Config::load(&path).unwrap();
        config.key_supplier = KeySupplierConfig::from(&runtime);
        config.save().unwrap();
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        (
            Arc::new(KeySupplierService::new(store, runtime).with_config_path(&path)),
            path,
        )
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
    fn parses_all_supported_webhook_events() {
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

        let test = IncomingWebhook::parse(
            format!(r#"{{"event":"test","event_id":"{EVENT_ID}","message":"test"}}"#)
                .as_bytes(),
        )
        .unwrap();
        assert!(matches!(test, IncomingWebhook::Test { .. }));
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
    fn ingest_rejects_invalid_runtime_webhook_token() {
        let (service, store) = service("weak-token");
        let body = format!(
            r#"{{"event":"all_keys_dead","event_id":"{EVENT_ID}","message":"body","dead":1}}"#
        );

        assert!(matches!(
            service.ingest("weak-token", body.as_bytes()),
            Err(SupplierServiceError::Unauthorized)
        ));
        assert!(store.list(1, None).unwrap().items.is_empty());
    }

    #[test]
    fn sanitize_error_removes_runtime_webhook_token() {
        let mut config = runtime(TOKEN);
        config.api_key = "supplier-api-key-canary".to_owned();
        let error = format!(
            "supplier={} webhook={} discovered=ksk_untrusted_canary",
            config.api_key, config.webhook_token
        );

        let redacted = sanitize_error(&error, &config);

        for secret in [
            config.api_key.as_str(),
            config.webhook_token.as_str(),
            "ksk_untrusted_canary",
        ] {
            assert!(!redacted.contains(secret), "leaked secret: {secret}");
        }
    }

    #[test]
    fn webhook_message_is_redacted_before_storage_listing_and_debug() {
        let path = temp_config_path("webhook-redaction");
        let store = Arc::new(SupplierEventStore::open(&path).unwrap());
        let mut config = runtime(TOKEN);
        config.api_key = "supplier-api-key-canary".to_owned();
        let service = KeySupplierService::new(store.clone(), config.clone());
        let message = format!(
            "supplier={} webhook={} discovered=ksk_untrusted_canary",
            config.api_key, config.webhook_token
        );
        let body = format!(
            r#"{{"event":"all_keys_dead","event_id":"{EVENT_ID}","message":"{message}","dead":1}}"#
        );

        service
            .ingest(&config.webhook_token, body.as_bytes())
            .unwrap();

        let connection = rusqlite::Connection::open(&path).unwrap();
        let stored: String = connection
            .query_row("SELECT message FROM supplier_events", [], |row| row.get(0))
            .unwrap();
        let listed = store.list(1, None).unwrap().items.remove(0);
        let debug = format!("{listed:?}");

        for secret in [
            config.api_key.as_str(),
            config.webhook_token.as_str(),
            "ksk_untrusted_canary",
        ] {
            assert!(!stored.contains(secret), "storage leaked secret: {secret}");
            assert!(!listed.message.as_deref().unwrap().contains(secret));
            assert!(!debug.contains(secret), "Debug leaked secret: {secret}");
        }

        drop(connection);
        drop(service);
        drop(store);
        let _ = std::fs::remove_file(path);
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
        config.auto_delete_forbidden = true;
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
        assert!(credential.delete_on_forbidden);
        assert_eq!(credential.nickname.as_deref(), Some("supplier-fedcba98-1"));
    }

    #[tokio::test]
    async fn nonduplicate_import_failure_persists_counts_and_retry_is_idempotent() {
        let orders = Arc::new(Mutex::new(Vec::new()));
        let seen_orders = orders.clone();
        let app = Router::new()
            .route(
                "/api/my/stock",
                get(|| async { axum::Json(serde_json::json!({"max": 3})) }),
            )
            .route(
                "/api/my/purchase",
                post(move |request: axum::http::Request<axum::body::Body>| {
                    let seen_orders = seen_orders.clone();
                    async move {
                        let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                            .await
                            .unwrap();
                        let order_id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()
                            ["client_order_id"]
                            .as_str()
                            .unwrap()
                            .to_owned();
                        seen_orders.lock().unwrap().push(order_id.clone());
                        purchase_json(
                            &order_id,
                            &[
                                "ksk_success_canary",
                                "ksk_duplicate_canary",
                                "ksk_failed_canary",
                            ],
                        )
                    }
                }),
            );
        let mut config = runtime(TOKEN);
        config.base_url = server(app).await;
        config.auto_purchase = true;
        let importer = Arc::new(FakeImporter::with_outcomes(vec![
            Ok(()),
            Err(anyhow::anyhow!("凭据已存在")),
            Err(anyhow::anyhow!("other failure")),
            Err(anyhow::anyhow!("凭据已存在")),
            Err(anyhow::anyhow!("凭据已存在")),
            Err(anyhow::anyhow!("凭据已存在")),
        ]));
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        queued_event(&store, "new_keys_available", Some(ORDER_ID), 3);
        let service = KeySupplierService::with_importer(store.clone(), config, importer);

        service.process_pending().await.unwrap();
        let failed = store.list(1, None).unwrap().items.remove(0);
        assert_eq!(
            (
                failed.purchased_count,
                failed.imported_count,
                failed.duplicate_count,
                failed.failed_count
            ),
            (3, 1, 1, 1)
        );
        assert_eq!(failed.status, SupplierEventStatus::Failed);

        service.retry_event(failed.id).unwrap();
        service.process_pending().await.unwrap();
        let retried = &store.list(1, None).unwrap().items[0];
        assert_eq!(retried.status, SupplierEventStatus::Succeeded);
        assert_eq!(
            (
                retried.purchased_count,
                retried.imported_count,
                retried.duplicate_count,
                retried.failed_count
            ),
            (3, 0, 3, 0)
        );
        assert_eq!(*orders.lock().unwrap(), vec![ORDER_ID, ORDER_ID]);
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
    async fn manual_purchase_persists_configuration_api_and_import_failures_before_returning_error()
    {
        let configuration_store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let configuration_service = KeySupplierService::with_importer(
            configuration_store.clone(),
            runtime(TOKEN),
            Arc::new(FakeImporter::default()),
        );
        assert!(configuration_service.manual_purchase(1).await.is_err());
        let configuration_event = &configuration_store.list(1, None).unwrap().items[0];
        assert_eq!(configuration_event.status, SupplierEventStatus::Failed);
        assert_eq!(
            (
                configuration_event.purchased_count,
                configuration_event.imported_count,
                configuration_event.duplicate_count,
                configuration_event.failed_count
            ),
            (0, 0, 0, 1)
        );

        let api_app = Router::new().route(
            "/api/my/purchase",
            post(|| async { (axum::http::StatusCode::BAD_GATEWAY, "supplier unavailable") }),
        );
        let mut api_config = runtime(TOKEN);
        api_config.base_url = server(api_app).await;
        let api_store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let api_service = KeySupplierService::with_importer(
            api_store.clone(),
            api_config,
            Arc::new(FakeImporter::default()),
        );
        assert!(api_service.manual_purchase(1).await.is_err());
        let api_event = &api_store.list(1, None).unwrap().items[0];
        assert_eq!(api_event.status, SupplierEventStatus::Failed);
        assert_eq!(
            (
                api_event.purchased_count,
                api_event.imported_count,
                api_event.duplicate_count,
                api_event.failed_count
            ),
            (0, 0, 0, 1)
        );

        let import_app = Router::new().route(
            "/api/my/purchase",
            post(
                |request: axum::http::Request<axum::body::Body>| async move {
                    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                        .await
                        .unwrap();
                    let order_id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()
                    ["client_order_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                    purchase_json(&order_id, &["ksk_manual_failed_import"])
                },
            ),
        );
        let mut import_config = runtime(TOKEN);
        import_config.base_url = server(import_app).await;
        let import_store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let import_service = KeySupplierService::with_importer(
            import_store.clone(),
            import_config,
            Arc::new(FakeImporter::with_outcomes(vec![Err(anyhow::anyhow!(
                "import failure"
            ))])),
        );
        assert!(import_service.manual_purchase(1).await.is_err());
        let import_event = &import_store.list(1, None).unwrap().items[0];
        assert_eq!(import_event.status, SupplierEventStatus::Failed);
        assert_eq!(
            (
                import_event.purchased_count,
                import_event.imported_count,
                import_event.duplicate_count,
                import_event.failed_count
            ),
            (1, 0, 0, 1)
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
    async fn supplier_http_diagnostics_are_persisted_without_exposing_secrets() {
        let app = Router::new().route(
            "/api/my/purchase",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_GATEWAY,
                    "safe supplier summary supplier-api-secret webhook-token-canary ksk_response_secret",
                )
            }),
        );
        let mut config = runtime("webhook-token-canary");
        config.api_key = "supplier-api-secret".to_owned();
        config.base_url = server(app).await;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_importer(
            store.clone(),
            config,
            Arc::new(FakeImporter::default()),
        );

        let error = service.manual_purchase(1).await.unwrap_err();
        let external = format!("{error} {error:?}");
        let event = &store.list(1, None).unwrap().items[0];
        let stored = event.last_error.as_deref().unwrap();

        assert!(stored.contains("supplier HTTP 502"));
        assert!(stored.contains("safe supplier summary"));
        for secret in [
            "supplier-api-secret",
            "webhook-token-canary",
            "ksk_response_secret",
        ] {
            assert!(!stored.contains(secret));
            assert!(!external.contains(secret));
        }
        assert!(!external.contains("safe supplier summary"));
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

    #[test]
    fn config_view_and_update_persist_without_revealing_secrets() {
        let mut initial = runtime(&"a".repeat(64));
        initial.base_url = "https://supplier.example".to_string();
        initial.public_base_url = "https://admin.example".to_string();
        initial.api_key = "api-key-canary".to_string();
        let (service, path) = persistent_service(initial.clone());

        let view = service.config_view();
        let encoded = serde_json::to_string(&view).unwrap();
        assert!(view.api_key_configured && view.webhook_token_configured);
        assert!(!encoded.contains(&initial.api_key));
        assert!(!encoded.contains(&initial.webhook_token));

        let mut update = supplier_update(&initial);
        update.min_purchase = 2;
        update.max_purchase = 3;
        let updated = service.update_config(update).unwrap();
        let persisted = Config::load(&path).unwrap();
        assert_eq!(updated.min_purchase, 2);
        assert_eq!(persisted.key_supplier.min_purchase, 2);
        assert_eq!(persisted.key_supplier.api_key, initial.api_key);
        assert_eq!(persisted.key_supplier.webhook_token, initial.webhook_token);
        assert_eq!(service.runtime_config().min_purchase, updated.min_purchase);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn failed_config_update_leaves_runtime_and_file_unchanged() {
        let mut initial = runtime(&"b".repeat(64));
        initial.base_url = "https://supplier.example".to_string();
        initial.public_base_url = "https://admin.example".to_string();
        let (service, path) = persistent_service(initial.clone());
        let before_file = std::fs::read_to_string(&path).unwrap();
        let mut update = supplier_update(&initial);
        update.min_purchase = 0;

        assert!(service.update_config(update).is_err());
        assert_eq!(service.runtime_config(), initial);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before_file);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn concurrent_config_updates_leave_runtime_consistent_with_disk() {
        let mut initial = runtime(&"c".repeat(64));
        initial.base_url = "https://supplier.example".to_string();
        initial.public_base_url = "https://admin.example".to_string();
        let (service, path) = persistent_service(initial.clone());
        let mut first = supplier_update(&initial);
        first.min_purchase = 2;
        first.max_purchase = 4;
        let mut second = supplier_update(&initial);
        second.min_purchase = 3;
        second.max_purchase = 5;

        let left = Arc::clone(&service);
        let right = Arc::clone(&service);
        let one = std::thread::spawn(move || left.update_config(first).unwrap());
        let two = std::thread::spawn(move || right.update_config(second).unwrap());
        one.join().unwrap();
        two.join().unwrap();

        let persisted = Config::load(&path).unwrap();
        assert_eq!(
            service.runtime_config().base_url,
            persisted.key_supplier.base_url
        );
        assert_eq!(
            service.runtime_config().min_purchase,
            persisted.key_supplier.min_purchase
        );
        assert_eq!(
            service.runtime_config().max_purchase,
            persisted.key_supplier.max_purchase
        );
        assert_eq!(persisted.key_supplier.api_key, initial.api_key);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn callback_url_requires_an_origin_and_hex_token() {
        let mut config = runtime(&"d".repeat(64));
        config.public_base_url = "https://admin.example/".to_string();
        let service = KeySupplierService::new(
            Arc::new(SupplierEventStore::open_in_memory().unwrap()),
            config.clone(),
        );
        let url = service.callback_url().unwrap();
        assert_eq!(
            url,
            format!(
                "https://admin.example/api/admin/key-supplier/webhook/{}",
                config.webhook_token
            )
        );
        assert!(!format!("{service:?}").contains(&config.webhook_token));

        config.public_base_url = "https://admin.example/path".to_string();
        let service = KeySupplierService::new(
            Arc::new(SupplierEventStore::open_in_memory().unwrap()),
            config.clone(),
        );
        assert!(service.callback_url().is_err());
        config.public_base_url = "https://admin.example".to_string();
        config.webhook_token = "not-hex".to_string();
        let service = KeySupplierService::new(
            Arc::new(SupplierEventStore::open_in_memory().unwrap()),
            config,
        );
        assert!(service.callback_url().is_err());
    }

    #[tokio::test]
    async fn overview_and_webhook_operations_use_the_supplier_client() {
        let registered = Arc::new(Mutex::new(Vec::new()));
        let registered_clone = registered.clone();
        let app = Router::new()
            .route("/api/my/profile", get(|| async { axum::Json(serde_json::json!({"name":"demo","quota":9,"remaining":7,"used_quota":2,"webhook_url":"https://private.example/hook"})) }))
            .route("/api/my/stock", get(|| async { axum::Json(serde_json::json!({"max":4})) }))
            .route("/api/status", get(|| async { axum::Json(serde_json::json!({"keys_active":3,"keys_dead":1})) }))
            .route("/api/my/webhook", put(move |request: axum::http::Request<axum::body::Body>| {
                let registered = registered_clone.clone();
                async move {
                    let body = axum::body::to_bytes(request.into_body(), usize::MAX).await.unwrap();
                    registered.lock().unwrap().push(String::from_utf8(body.to_vec()).unwrap());
                    axum::http::StatusCode::NO_CONTENT
                }
            }))
            .route("/api/my/webhook/test", post(|| async { axum::http::StatusCode::NO_CONTENT }));
        let mut config = runtime(&"e".repeat(64));
        config.base_url = server(app).await;
        config.public_base_url = "https://admin.example".to_string();
        let service = KeySupplierService::new(
            Arc::new(SupplierEventStore::open_in_memory().unwrap()),
            config.clone(),
        );

        let overview = service.overview().await.unwrap();
        assert_eq!(overview.profile.name, "demo");
        assert_eq!(overview.stock.max, 4);
        assert_eq!(overview.status.keys_active, 3);
        assert!(!overview.webhook_registered);
        assert!(!format!("{overview:?}").contains(&config.webhook_token));
        let callback = service.register_webhook().await.unwrap();
        service.test_webhook().await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&registered.lock().unwrap()[0]).unwrap()["webhook_url"],
            callback
        );
    }

    #[tokio::test]
    async fn processing_cycle_recovers_stale_work_and_processor_starts_once() {
        let path = temp_config_path("events");
        let store = Arc::new(SupplierEventStore::open(&path).unwrap());
        store
            .insert_event(IncomingSupplierEvent {
                event_id: "stale-event".to_string(),
                event_type: "all_keys_dead".to_string(),
                purchase_order_id: None,
                message: Some("stale".to_string()),
                quantity: 1,
            })
            .unwrap();
        let claimed = store.claim_next().unwrap().unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE supplier_events SET processing_started_at=?1 WHERE id=?2",
                rusqlite::params![(Utc::now() - Duration::minutes(6)).to_rfc3339(), claimed.id],
            )
            .unwrap();
        let service = Arc::new(KeySupplierService::new(store.clone(), runtime(TOKEN)));

        assert_eq!(service.run_processing_cycle().await.unwrap(), 1);
        assert_eq!(
            store.list(1, None).unwrap().items[0].status,
            SupplierEventStatus::Succeeded
        );
        assert!(service.start_processor());
        assert!(!service.start_processor());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn stale_recovery_does_not_reclaim_event_running_under_processing_lock() {
        let app = Router::new()
            .route(
                "/api/my/stock",
                get(|| async { axum::Json(serde_json::json!({"max": 1})) }),
            )
            .route(
                "/api/my/purchase",
                post(|| async { purchase_json(ORDER_ID, &["ksk_blocking_canary"]) }),
            );
        let path = temp_config_path("stale-lock");
        let store = Arc::new(SupplierEventStore::open(&path).unwrap());
        queued_event(&store, "new_keys_available", Some(ORDER_ID), 1);
        let mut config = runtime(TOKEN);
        config.base_url = server(app).await;
        config.auto_purchase = true;
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let service = Arc::new(KeySupplierService::with_importer(
            store.clone(),
            config,
            Arc::new(BlockingImporter {
                started: started.clone(),
                release: release.clone(),
            }),
        ));

        let started_wait = started.notified();
        let running_service = service.clone();
        let running = tokio::spawn(async move { running_service.process_pending().await });
        started_wait.await;
        let id = store.list(1, None).unwrap().items[0].id;
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE supplier_events SET processing_started_at=?1 WHERE id=?2",
                rusqlite::params![(Utc::now() - Duration::minutes(6)).to_rfc3339(), id],
            )
            .unwrap();

        let recovery_service = service.clone();
        let recovery = tokio::spawn(async move { recovery_service.run_processing_cycle().await });
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(
            store.list(1, None).unwrap().items[0].status,
            SupplierEventStatus::Processing
        );

        release.notify_one();
        assert_eq!(running.await.unwrap().unwrap(), 1);
        assert_eq!(recovery.await.unwrap().unwrap(), 0);
        assert_eq!(
            store.list(1, None).unwrap().items[0].status,
            SupplierEventStatus::Succeeded
        );
        drop(service);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn retry_event_delegates_to_store() {
        let (service, store) = service(TOKEN);
        queued_event(&store, "all_keys_dead", None, 1);
        let claimed = store.claim_next().unwrap().unwrap();
        store.fail(claimed.id, "failed").unwrap();

        service.retry_event(claimed.id).unwrap();
        assert_eq!(
            store.list(1, None).unwrap().items[0].status,
            SupplierEventStatus::Received
        );
    }
}
