use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use serde_json::Value;

use crate::admin::key_supplier::client::{SupplierClient, SupplierSnapshot};
use crate::admin::key_supplier::config::{
    MAX_SUPPLIERS, SupplierConfigUpdate, SupplierConfigView, SupplierEntryRuntime,
    SupplierEntryUpdate, SupplierEntryView, SupplierRuntimeConfig, is_valid_webhook_token,
    normalize_supplier_id, store_suppliers,
};
use crate::admin::key_supplier::store::{
    IncomingSupplierEvent, InsertOutcome, LEGACY_SUPPLIER_ID, ProcessSummary, StoredSupplierEvent,
    SupplierEventStore,
};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::region::API_KEY_AUTH_REGION;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::{Config, SupplierKind};

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
    /// 解析 webhook。按供货商协议分派：`kiro-rs` 走原有严格校验，
    /// `kiro-app` 的推送体格式未文档化，走宽容解析。
    pub fn parse(kind: SupplierKind, body: &[u8]) -> Result<Self, SupplierServiceError> {
        match kind {
            SupplierKind::KiroRs => Self::parse_kiro_rs(body),
            SupplierKind::KiroApp => Self::parse_kiro_app(body),
        }
    }

    fn parse_kiro_rs(body: &[u8]) -> Result<Self, SupplierServiceError> {
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

    /// kiroapp.cc 的到货推送：「一车产出多少 Key 只发一条」，字段名未公开。
    ///
    /// 因此：数量按一组候选字段名取，取不到按 0 处理（下单量再由库存/配置夹逼）；
    /// event id 优先用对方给的稳定 id，没有就退化成 body 指纹——同一车重复推
    /// 仍然命中同一行，靠 `(supplier_id, event_id)` 唯一索引挡住第二次下单。
    fn parse_kiro_app(body: &[u8]) -> Result<Self, SupplierServiceError> {
        let value: Value =
            serde_json::from_slice(body).map_err(|_| SupplierServiceError::InvalidJson)?;
        let object = value
            .as_object()
            .ok_or(SupplierServiceError::InvalidPayload)?;

        let event_name = object
            .get("event")
            .or_else(|| object.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let message = object
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("kiroapp 到货通知")
            .to_string();
        let event_id = optional_id(object, &["event_id", "eventId", "id", "batchId", "batch_id"])
            .unwrap_or_else(|| body_fingerprint(body));

        if event_name.eq_ignore_ascii_case("test") {
            return Ok(Self::Test { event_id, message });
        }

        let new_keys = optional_quantity(
            object,
            &[
                "count",
                "keys",
                "newKeys",
                "new_keys",
                "availableKeys",
                "available_keys",
                "quantity",
            ],
        )
        .unwrap_or(0);
        // claim 没有幂等键，订单号只用于日志与昵称后缀；从 event_id 派生保证重放一致。
        let purchase_order_id = derive_order_id(&event_id);
        Ok(Self::NewKeysAvailable {
            event_id,
            purchase_order_id,
            message,
            new_keys,
        })
    }

    fn into_event(self, supplier_id: &str) -> IncomingSupplierEvent {
        let supplier_id = supplier_id.to_string();
        match self {
            Self::NewKeysAvailable {
                event_id,
                purchase_order_id,
                message,
                new_keys,
            } => IncomingSupplierEvent {
                supplier_id,
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
                supplier_id,
                event_id,
                event_type: "all_keys_dead".to_string(),
                purchase_order_id: None,
                message: Some(message),
                quantity: i64::from(dead),
            },
            Self::Test { event_id, message } => IncomingSupplierEvent {
                supplier_id,
                event_id,
                event_type: "test".to_string(),
                purchase_order_id: None,
                message: Some(message),
                quantity: 0,
            },
        }
    }
}

/// `hex(HMAC-SHA256(secret, 原始请求体))`，与 kiroapp.cc 的 `X-Kiro-Signature` 对齐。
fn sign_webhook_body(secret: &str, body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 用推送体指纹当 event id：同一车重复推 → 同一 id → 唯一索引判重 → 不重复下单。
fn body_fingerprint(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(body);
    hex_prefix(&digest)
}

/// 从 event id 派生 32 hex 订单号，保证同一事件重放得到同一个订单号。
fn derive_order_id(event_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"kiro-supplier-order:");
    hasher.update(event_id.as_bytes());
    hex_prefix(&hasher.finalize())
}

fn hex_prefix(digest: &[u8]) -> String {
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// event id 的最大长度。对方的 `evt_8DX2ZPK9MR7Q4JWH` 远短于此，留足余量即可。
const MAX_EVENT_ID_CHARS: usize = 128;

/// 在候选字段名里找第一个可用的 id。
///
/// **原样保留**对方给的 id（例如 `evt_8DX2ZPK9MR7Q4JWH`），这样事件历史里的 id
/// 能直接和对方后台的投递记录对上。只有超长或含控制字符时才退化成哈希——
/// 去重只要求「同一事件映射到同一个 id」，不要求 id 是 hex。
fn optional_id(object: &serde_json::Map<String, Value>, fields: &[&str]) -> Option<String> {
    for field in fields {
        let raw = match object.get(*field) {
            Some(Value::String(value)) => value.trim().to_string(),
            Some(Value::Number(value)) => value.to_string(),
            _ => continue,
        };
        if raw.is_empty() {
            continue;
        }
        let usable = raw.chars().count() <= MAX_EVENT_ID_CHARS
            && !raw.chars().any(|ch| ch.is_control() || ch.is_whitespace());
        return Some(if usable { raw } else { derive_order_id(&raw) });
    }
    None
}

/// 在候选字段名里找第一个能当数量用的值。数组按长度算（`{"keys":[...]}`）。
fn optional_quantity(object: &serde_json::Map<String, Value>, fields: &[&str]) -> Option<u32> {
    for field in fields {
        let quantity = match object.get(*field) {
            Some(Value::Number(value)) => value.as_u64(),
            Some(Value::Array(values)) => Some(values.len() as u64),
            Some(Value::String(value)) => value.trim().parse::<u64>().ok(),
            _ => None,
        };
        if let Some(quantity) = quantity
            .and_then(|value| u32::try_from(value).ok())
            .filter(|quantity| *quantity > 0)
        {
            return Some(quantity);
        }
    }
    None
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
    suppliers: parking_lot::RwLock<Vec<SupplierEntryRuntime>>,
    importer: Option<Arc<dyn CredentialImporter>>,
    processing_lock: tokio::sync::Mutex<()>,
    config_path: Option<PathBuf>,
    config_update_lock: parking_lot::Mutex<()>,
    processor_started: AtomicBool,
    /// webhook 落库后立刻唤醒后台处理器，不必等 30s tick。抢货拼的就是这点延迟。
    wakeup: tokio::sync::Notify,
}

impl fmt::Debug for KeySupplierService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeySupplierService")
            .field("suppliers", &*self.suppliers.read())
            .field("config_path", &self.config_path)
            .field(
                "processor_started",
                &self.processor_started.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl KeySupplierService {
    /// 单供货商构造器，等价于只挂一家 `default` / `kiro-rs`。
    /// 生产路径走 `new_with_token_manager`（读配置列表），这个只剩测试在用。
    #[cfg(test)]
    pub fn new(store: Arc<SupplierEventStore>, runtime: SupplierRuntimeConfig) -> Self {
        Self::with_suppliers(store, vec![default_entry(runtime)])
    }

    #[cfg(test)]
    pub fn with_suppliers(
        store: Arc<SupplierEventStore>,
        suppliers: Vec<SupplierEntryRuntime>,
    ) -> Self {
        Self {
            store,
            suppliers: parking_lot::RwLock::new(suppliers),
            importer: None,
            processing_lock: tokio::sync::Mutex::new(()),
            config_path: None,
            config_update_lock: parking_lot::Mutex::new(()),
            processor_started: AtomicBool::new(false),
            wakeup: tokio::sync::Notify::new(),
        }
    }

    #[cfg(test)]
    pub fn with_importer(
        store: Arc<SupplierEventStore>,
        runtime: SupplierRuntimeConfig,
        importer: Arc<dyn CredentialImporter>,
    ) -> Self {
        Self::with_suppliers_and_importer(store, vec![default_entry(runtime)], importer)
    }

    pub fn with_suppliers_and_importer(
        store: Arc<SupplierEventStore>,
        suppliers: Vec<SupplierEntryRuntime>,
        importer: Arc<dyn CredentialImporter>,
    ) -> Self {
        Self {
            store,
            suppliers: parking_lot::RwLock::new(suppliers),
            importer: Some(importer),
            processing_lock: tokio::sync::Mutex::new(()),
            config_path: None,
            config_update_lock: parking_lot::Mutex::new(()),
            processor_started: AtomicBool::new(false),
            wakeup: tokio::sync::Notify::new(),
        }
    }

    pub fn new_with_token_manager(
        store: Arc<SupplierEventStore>,
        suppliers: Vec<SupplierEntryRuntime>,
        token_manager: Arc<MultiTokenManager>,
    ) -> Self {
        Self::with_suppliers_and_importer(
            store,
            suppliers,
            Arc::new(TokenManagerCredentialImporter::new(token_manager)),
        )
    }

    pub fn supplier(&self, id: &str) -> Option<SupplierEntryRuntime> {
        let id = normalize_supplier_id(id).ok()?;
        self.suppliers
            .read()
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
    }

    /// 历史单供货商接口的落点：第一个启用的供货商，没有则第一个。
    fn primary(&self) -> Result<SupplierEntryRuntime, SupplierServiceError> {
        let suppliers = self.suppliers.read();
        suppliers
            .iter()
            .find(|entry| entry.enabled)
            .or_else(|| suppliers.first())
            .cloned()
            .ok_or(SupplierServiceError::SupplierConfiguration)
    }

    /// 历史接口用的运行期配置（第一个供货商）。缺供货商时给默认值，避免 panic。
    pub fn runtime_config(&self) -> SupplierRuntimeConfig {
        self.primary()
            .map(|entry| entry.settings)
            .unwrap_or_else(|_| empty_runtime())
    }

    pub fn with_config_path(mut self, config_path: impl AsRef<Path>) -> Self {
        self.config_path = Some(config_path.as_ref().to_path_buf());
        self
    }

    /// 直接热替第一个供货商的连接配置。仅测试用——生产路径一律走
    /// `upsert_supplier`，那条路会同时落盘，不会让内存和磁盘分叉。
    #[cfg(test)]
    fn set_runtime_config(&self, runtime: SupplierRuntimeConfig) {
        let mut suppliers = self.suppliers.write();
        match suppliers.iter_mut().find(|entry| entry.enabled) {
            Some(entry) => entry.settings = runtime,
            None => match suppliers.first_mut() {
                Some(entry) => entry.settings = runtime,
                None => suppliers.push(default_entry(runtime)),
            },
        }
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

    /// 任一供货商的 token 命中即通过（历史行为：只看第一家）。
    pub fn has_valid_webhook_token(&self, token: &str) -> bool {
        self.resolve_webhook_token(token).is_some()
    }

    /// 用 webhook token 反查供货商。每家一个 token，常量时间比较防时序侧信道。
    pub fn resolve_webhook_token(&self, token: &str) -> Option<SupplierEntryRuntime> {
        self.suppliers
            .read()
            .iter()
            .find(|entry| {
                is_valid_webhook_token(&entry.settings.webhook_token)
                    && crate::common::auth::constant_time_eq(&entry.settings.webhook_token, token)
            })
            .cloned()
    }

    pub fn config_view(&self) -> SupplierConfigView {
        SupplierConfigView::from(&self.runtime_config())
    }

    pub fn supplier_views(&self) -> Vec<SupplierEntryView> {
        self.suppliers
            .read()
            .iter()
            .map(SupplierEntryView::from)
            .collect()
    }

    /// 历史单供货商配置接口。落到第一个供货商上，不再写 `config.key_supplier`。
    pub fn update_config(
        &self,
        update: SupplierConfigUpdate,
    ) -> Result<SupplierConfigView, SupplierServiceError> {
        let primary = self.primary().ok();
        let entry = self.upsert_supplier(
            primary.as_ref().map(|entry| entry.id.clone()),
            SupplierEntryUpdate {
                id: Some(
                    primary
                        .as_ref()
                        .map(|entry| entry.id.clone())
                        .unwrap_or_else(|| LEGACY_SUPPLIER_ID.to_owned()),
                ),
                name: primary
                    .as_ref()
                    .map(|entry| entry.name.clone())
                    .unwrap_or_else(|| "默认供货商".to_owned()),
                kind: primary.as_ref().map_or(SupplierKind::KiroRs, |e| e.kind),
                enabled: primary.as_ref().is_none_or(|entry| entry.enabled),
                settings: update,
            },
        )?;
        Ok(entry.settings)
    }

    /// 新增或修改一家供货商，落盘后热更新内存。`id=None` 表示新增。
    pub fn upsert_supplier(
        &self,
        id: Option<String>,
        update: SupplierEntryUpdate,
    ) -> Result<SupplierEntryView, SupplierServiceError> {
        let _guard = self.config_update_lock.lock();
        let mut entries = self.suppliers.read().clone();
        let existing_index = match id.as_deref() {
            Some(id) => {
                let id = normalize_supplier_id(id)
                    .map_err(|_| SupplierServiceError::SupplierConfiguration)?;
                Some(
                    entries
                        .iter()
                        .position(|entry| entry.id == id)
                        .ok_or(SupplierServiceError::SupplierNotFound)?,
                )
            }
            None => None,
        };
        let existing = existing_index.map(|index| entries[index].clone());
        let runtime =
            SupplierEntryRuntime::normalize_update(id.as_deref(), update, existing.as_ref())
                .map_err(|_| SupplierServiceError::SupplierConfiguration)?;

        match existing_index {
            Some(index) => entries[index] = runtime.clone(),
            None => {
                if entries.iter().any(|entry| entry.id == runtime.id) {
                    return Err(SupplierServiceError::SupplierIdConflict);
                }
                if entries.len() >= MAX_SUPPLIERS {
                    return Err(SupplierServiceError::TooManySuppliers);
                }
                entries.push(runtime.clone());
            }
        }
        self.persist_suppliers(&entries)?;
        *self.suppliers.write() = entries;
        Ok(SupplierEntryView::from(&runtime))
    }

    /// 删除一家供货商。事件历史保留（带 supplier_id），只是不再采购。
    pub fn delete_supplier(&self, id: &str) -> Result<(), SupplierServiceError> {
        let _guard = self.config_update_lock.lock();
        let id = normalize_supplier_id(id).map_err(|_| SupplierServiceError::SupplierNotFound)?;
        let mut entries = self.suppliers.read().clone();
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.len() == before {
            return Err(SupplierServiceError::SupplierNotFound);
        }
        self.persist_suppliers(&entries)?;
        *self.suppliers.write() = entries;
        Ok(())
    }

    fn persist_suppliers(
        &self,
        entries: &[SupplierEntryRuntime],
    ) -> Result<(), SupplierServiceError> {
        let path = self
            .config_path
            .as_ref()
            .ok_or(SupplierServiceError::ConfigPathUnavailable)?;
        let mut config = Config::load(path).map_err(|_| SupplierServiceError::ConfigPersistence)?;
        store_suppliers(&mut config, entries);
        config
            .save()
            .map_err(|_| SupplierServiceError::ConfigPersistence)
    }

    fn client_for(
        &self,
        entry: &SupplierEntryRuntime,
    ) -> Result<SupplierClient, SupplierServiceError> {
        if !entry.is_operable() {
            return Err(SupplierServiceError::SupplierConfiguration);
        }
        SupplierClient::with_kind(&entry.settings.base_url, &entry.settings.api_key, entry.kind)
            .map_err(|_| SupplierServiceError::SupplierConfiguration)
    }

    pub async fn overview(&self) -> Result<SupplierOverview, SupplierServiceError> {
        self.supplier_overview(&self.primary()?.id).await
    }

    /// 指定供货商的概览。字段按协议能力给，缺的留 `None`。
    pub async fn supplier_overview(
        &self,
        id: &str,
    ) -> Result<SupplierOverview, SupplierServiceError> {
        let entry = self
            .supplier(id)
            .ok_or(SupplierServiceError::SupplierNotFound)?;
        let snapshot = self
            .client_for(&entry)?
            .snapshot()
            .await
            .map_err(SupplierServiceError::supplier_api)?;
        let webhook_registered = match (&snapshot.webhook_url, self.supplier_callback_url(id).ok()) {
            (Some(registered), Some(callback)) => *registered == callback,
            // kiro-app 读不到对方登记的回调地址，注册状态未知。
            _ => false,
        };
        Ok(SupplierOverview {
            supplier_id: entry.id,
            kind: entry.kind.as_str(),
            snapshot,
            webhook_registered,
        })
    }

    /// 历史单供货商回调地址。生产路径走 `supplier_callback_url`（带 id）。
    #[cfg(test)]
    pub fn callback_url(&self) -> Result<String, SupplierServiceError> {
        self.supplier_callback_url(&self.primary()?.id)
    }

    /// 该供货商专属的回调地址。`kiro-app` 需要把这个地址手填到对方面板。
    pub fn supplier_callback_url(&self, id: &str) -> Result<String, SupplierServiceError> {
        let entry = self
            .supplier(id)
            .ok_or(SupplierServiceError::SupplierNotFound)?;
        let runtime = &entry.settings;
        if !is_valid_webhook_token(&runtime.webhook_token)
            || runtime.public_base_url.trim().is_empty()
        {
            return Err(SupplierServiceError::SupplierConfiguration);
        }
        // 必须是纯 origin：带 path 的 publicBaseUrl 会被 set_path 静默吞掉，
        // 让人以为回调配好了其实地址是错的，所以这里直接拒绝。
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
        self.register_supplier_webhook(&self.primary()?.id).await
    }

    pub async fn register_supplier_webhook(
        &self,
        id: &str,
    ) -> Result<String, SupplierServiceError> {
        let entry = self
            .supplier(id)
            .ok_or(SupplierServiceError::SupplierNotFound)?;
        if !entry.kind.supports_webhook_registration() {
            return Err(SupplierServiceError::WebhookRegistrationUnsupported);
        }
        let callback = self.supplier_callback_url(&entry.id)?;
        self.client_for(&entry)?
            .register_webhook(&callback)
            .await
            .map_err(SupplierServiceError::supplier_api)?;
        Ok(callback)
    }

    pub async fn test_webhook(&self) -> Result<(), SupplierServiceError> {
        self.test_supplier_webhook(&self.primary()?.id).await
    }

    pub async fn test_supplier_webhook(&self, id: &str) -> Result<(), SupplierServiceError> {
        let entry = self
            .supplier(id)
            .ok_or(SupplierServiceError::SupplierNotFound)?;
        if !entry.kind.supports_webhook_registration() {
            return Err(SupplierServiceError::WebhookRegistrationUnsupported);
        }
        self.client_for(&entry)?
            .test_webhook()
            .await
            .map_err(SupplierServiceError::supplier_api)
    }

    /// HTTP 层直接打 `store.retry`；这层包装只剩测试在用。
    #[cfg(test)]
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
                // 抢货是拼延迟的：库存通知一落库就立刻处理，别等下一个 30s tick。
                // 定时 tick 仍然保留，用于回收 stale processing 和兜底重放。
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = service.wakeup.notified() => {}
                }
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

    /// 收 webhook：token 反查供货商 → 按其协议解析 → 落库。
    ///
    /// 重复推送在这里被唯一索引挡住（只累加 `webhook_duplicate_count`），
    /// 不会产生第二条待处理事件，因此不会重复下单。
    pub fn ingest<B: AsRef<[u8]>>(
        &self,
        token: &str,
        body: B,
    ) -> Result<IngestResult, SupplierServiceError> {
        self.ingest_signed(token, body, None)
    }

    /// 收 webhook，并在配了签名密钥时校验 `X-Kiro-Signature`。
    ///
    /// `signature` 是请求头原文；`body` 必须是**原始请求体字节**，
    /// 不能是重新序列化过的 JSON（字段顺序/空格变了签名就不对）。
    pub fn ingest_signed<B: AsRef<[u8]>>(
        &self,
        token: &str,
        body: B,
        signature: Option<&str>,
    ) -> Result<IngestResult, SupplierServiceError> {
        let entry = self
            .resolve_webhook_token(token)
            .ok_or(SupplierServiceError::Unauthorized)?;

        if !entry.settings.webhook_secret.is_empty() {
            let expected = sign_webhook_body(&entry.settings.webhook_secret, body.as_ref());
            let provided = signature.unwrap_or_default();
            if !crate::common::auth::constant_time_eq(&expected, provided) {
                return Err(SupplierServiceError::InvalidSignature);
            }
        }

        let webhook = IncomingWebhook::parse(entry.kind, body.as_ref())?;
        let mut event = webhook.into_event(&entry.id);
        event.message = event
            .message
            .map(|message| redact_runtime_secrets(&message, &entry.settings));
        let event_id = event.event_id.clone();
        let event_type = event.event_type.clone();
        let outcome = self
            .store
            .insert_event(event)
            .map_err(|_| SupplierServiceError::Store)?;
        let duplicate = matches!(outcome, InsertOutcome::Duplicate(_));
        // 只有真正新落库的事件才唤醒处理器：重复推送已经处理过（或正在处理），
        // 再唤醒一次只会空转，而且绝不能因此重复下单。
        if !duplicate {
            self.wakeup.notify_one();
        }
        Ok(IngestResult {
            duplicate,
            supplier_id: entry.id,
            event_id,
            event_type,
        })
    }

    /// 立刻处理一轮待办，不做 stale 回收。生产路径走 `run_processing_cycle`。
    #[cfg(test)]
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
        let primary = self.primary()?;
        self.manual_purchase_from(&primary.id, count).await
    }

    /// 指定供货商手动采购。数量必须落在该供货商自己的 min/max 区间内。
    pub async fn manual_purchase_from(
        &self,
        supplier_id: &str,
        count: u32,
    ) -> Result<ManualPurchaseResult, SupplierServiceError> {
        let entry = self
            .supplier(supplier_id)
            .ok_or(SupplierServiceError::SupplierNotFound)?;
        if count < entry.settings.min_purchase || count > entry.settings.max_purchase {
            return Err(SupplierServiceError::InvalidPurchaseQuantity);
        }

        let _guard = self.processing_lock.lock().await;
        let order_id = uuid::Uuid::new_v4().simple().to_string();
        let event = IncomingSupplierEvent {
            supplier_id: entry.id.clone(),
            event_id: order_id.clone(),
            event_type: "manual_purchase".to_owned(),
            purchase_order_id: Some(order_id.clone()),
            message: None,
            quantity: i64::from(count),
        };
        self.run_store_operation(move |store| store.insert_event(event))
            .await?;
        let lookup_supplier_id = entry.id.clone();
        let lookup_order_id = order_id.clone();
        let event = self
            .run_store_operation(move |store| {
                store.claim_by_event_id(&lookup_supplier_id, &lookup_order_id)
            })
            .await?
            .ok_or(SupplierServiceError::Store)?;
        let summary = self.process_claimed(event).await?;
        Ok(ManualPurchaseResult {
            supplier_id: entry.id,
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
            Ok(ProcessAction::SkipWithReason(reason)) => {
                self.run_store_operation(move |store| store.skip(event.id, Some(reason)))
                    .await?;
                Ok(empty_summary())
            }
            Ok(ProcessAction::Failed { summary, error }) => {
                let persistence_error = self.sanitize_for(&event.supplier_id, &error);
                self.run_store_operation(move |store| {
                    store.fail_with_summary(event.id, summary, &persistence_error)
                })
                .await?;
                Err(error)
            }
            Err(error) => {
                let persistence_error = self.sanitize_for(&event.supplier_id, &error);
                self.run_store_operation(move |store| store.fail(event.id, &persistence_error))
                    .await?;
                Err(error)
            }
        }
    }

    /// 用事件所属供货商的 secret 做脱敏。供货商已删则退化成通用脱敏。
    fn sanitize_for(&self, supplier_id: &str, error: &SupplierServiceError) -> String {
        let detail = error.persistence_detail();
        match self.supplier(supplier_id) {
            Some(entry) => sanitize_error(&detail, &entry.settings),
            None => sanitize_error(&detail, &empty_runtime()),
        }
    }

    async fn execute_claimed(
        &self,
        event: &StoredSupplierEvent,
    ) -> Result<ProcessAction, SupplierServiceError> {
        if matches!(event.event_type.as_str(), "all_keys_dead" | "test") {
            return Ok(ProcessAction::Complete(empty_summary()));
        }

        // 事件带 supplier_id，处理时按它找回供货商；供货商被删掉就跳过而不是报错。
        let entry = self
            .supplier(&event.supplier_id)
            .ok_or(SupplierServiceError::SupplierNotFound)?;
        let runtime = &entry.settings;
        if event.event_type == "new_keys_available" && (!runtime.auto_purchase || !entry.enabled) {
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
                let client = self.client_for(&entry)?;
                let event_count = u32::try_from(event.quantity)
                    .map_err(|_| SupplierServiceError::InvalidEvent)?;
                // kiro-app 的库存通知自带 count，官方文档明确建议「直接尝试领取，
                // 不要先查 /openapi/stock」——查询和领取不是一个事务，多一次往返
                // 只会把货让给别人。kiro-rs 没这个说法，保持先查库存夹逼。
                let available = match entry.kind {
                    SupplierKind::KiroApp => u64::from(runtime.max_purchase),
                    SupplierKind::KiroRs => client
                        .available_stock()
                        .await
                        .map_err(SupplierServiceError::supplier_api)?,
                };
                // 推送没带数量时按配置上限要，实际给多少由对方决定。
                let requested = if event_count == 0 {
                    runtime.max_purchase
                } else {
                    event_count
                };
                match select_purchase_count(
                    requested,
                    available,
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
                let client = self.client_for(&entry)?;
                (count, client)
            }
            _ => unreachable!("event type was validated before purchase"),
        };

        let order_id = event
            .purchase_order_id
            .as_deref()
            .ok_or(SupplierServiceError::InvalidEvent)?;
        let purchase = match client.purchase(count, order_id).await {
            Ok(purchase) => purchase,
            // 被别人抢完了：正常竞争结果，记 skipped 而不是 failed，也不给重试按钮
            // （重试只会再抢一次空气，还可能在真有货时变成额外下单）。
            Err(crate::admin::key_supplier::client::SupplierError::OutOfStock) => {
                return Ok(ProcessAction::SkipWithReason("库存已被抢完"));
            }
            Err(error) => return Err(SupplierServiceError::supplier_api(error)),
        };
        let mut summary = ProcessSummary {
            purchased_count: i64::from(purchase.purchased),
            ..empty_summary()
        };
        let mut import_failed = false;
        for (index, key) in purchase.keys.into_iter().enumerate() {
            let credential =
                credential_from_supplier_key(key.into_inner(), runtime, order_id, index + 1);
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
    /// 跳过并记下原因（例如库存被抢完），让事件历史能看出是竞争失败而非故障。
    SkipWithReason(&'static str),
    Failed {
        summary: ProcessSummary,
        error: SupplierServiceError,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub struct ManualPurchaseResult {
    pub supplier_id: String,
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
            .field("supplier_id", &self.supplier_id)
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
    pub supplier_id: String,
    pub event_id: String,
    pub event_type: String,
}

impl fmt::Debug for IngestResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IngestResult")
            .field("duplicate", &self.duplicate)
            .field("supplier_id", &self.supplier_id)
            .field("event_id", &self.event_id)
            .field("event_type", &self.event_type)
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct SupplierOverview {
    pub supplier_id: String,
    pub kind: &'static str,
    pub snapshot: SupplierSnapshot,
    pub webhook_registered: bool,
}

impl fmt::Debug for SupplierOverview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SupplierOverview")
            .field("supplier_id", &self.supplier_id)
            .field("kind", &self.kind)
            .field("snapshot", &self.snapshot)
            .field("webhook_registered", &self.webhook_registered)
            .finish()
    }
}

/// 只挂一家 `default`/`kiro-rs` 供货商时的条目。测试专用（见 `new`）。
#[cfg(test)]
fn default_entry(runtime: SupplierRuntimeConfig) -> SupplierEntryRuntime {
    SupplierEntryRuntime {
        id: LEGACY_SUPPLIER_ID.to_owned(),
        name: "默认供货商".to_owned(),
        kind: SupplierKind::KiroRs,
        enabled: true,
        settings: runtime,
    }
}

/// 没有任何供货商时给历史接口用的空配置。
fn empty_runtime() -> SupplierRuntimeConfig {
    SupplierRuntimeConfig {
        base_url: String::new(),
        api_key: String::new(),
        public_base_url: String::new(),
        webhook_token: String::new(),
        webhook_secret: String::new(),
        auto_purchase: false,
        auto_delete_forbidden: false,
        min_purchase: 1,
        max_purchase: 1,
        api_region: API_KEY_AUTH_REGION.to_owned(),
        rpm_limit: 0,
        priority: 0,
        groups: Vec::new(),
        source_channel: String::new(),
        nickname_prefix: String::new(),
    }
}

fn processing_error_kind(error: &SupplierServiceError) -> &'static str {
    match error {
        SupplierServiceError::Store => "store",
        SupplierServiceError::SupplierApi { .. } => "supplier_api",
        SupplierServiceError::SupplierConfiguration => "configuration",
        SupplierServiceError::SupplierNotFound => "supplier_missing",
        SupplierServiceError::ImporterUnavailable => "importer",
        _ => "other",
    }
}

pub enum SupplierServiceError {
    Unauthorized,
    /// `X-Kiro-Signature` 缺失或不匹配。
    InvalidSignature,
    InvalidJson,
    InvalidPayload,
    InvalidEvent,
    InvalidPurchaseQuantity,
    SupplierConfiguration,
    SupplierApi { diagnostic: String },
    /// 路径里的供货商 id 不存在（或事件所属供货商已被删除）。
    SupplierNotFound,
    /// 新增时 id 已被占用。
    SupplierIdConflict,
    TooManySuppliers,
    /// 该协议不支持远程注册/测试 webhook（`kiro-app` 只能手填回调地址）。
    WebhookRegistrationUnsupported,
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
            Self::InvalidSignature => "webhook signature verification failed",
            Self::InvalidJson => "invalid webhook JSON",
            Self::InvalidPayload => "invalid webhook payload",
            Self::InvalidEvent => "invalid supplier event",
            Self::InvalidPurchaseQuantity => {
                "manual purchase quantity is outside configured bounds"
            }
            Self::SupplierConfiguration => "supplier configuration is invalid",
            Self::SupplierApi { .. } => "supplier API request failed",
            Self::SupplierNotFound => "supplier not found",
            Self::SupplierIdConflict => "supplier id already exists",
            Self::TooManySuppliers => "too many suppliers configured",
            Self::WebhookRegistrationUnsupported => {
                "supplier protocol does not support webhook registration"
            }
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
            Self::InvalidSignature => "InvalidSignature",
            Self::InvalidJson => "InvalidJson",
            Self::InvalidPayload => "InvalidPayload",
            Self::InvalidEvent => "InvalidEvent",
            Self::InvalidPurchaseQuantity => "InvalidPurchaseQuantity",
            Self::SupplierConfiguration => "SupplierConfiguration",
            Self::SupplierApi { .. } => "SupplierApi",
            Self::SupplierNotFound => "SupplierNotFound",
            Self::SupplierIdConflict => "SupplierIdConflict",
            Self::TooManySuppliers => "TooManySuppliers",
            Self::WebhookRegistrationUnsupported => "WebhookRegistrationUnsupported",
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
            webhook_secret: String::new(),
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
            webhook_secret: None,
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
                supplier_id: LEGACY_SUPPLIER_ID.to_string(),
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
            SupplierKind::KiroRs,
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
            SupplierKind::KiroRs,
            format!(
                r#"{{"event":"all_keys_dead","event_id":"{EVENT_ID}","message":"dead","dead":2}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(matches!(dead, IncomingWebhook::AllKeysDead { dead: 2, .. }));

        let test = IncomingWebhook::parse(
            SupplierKind::KiroRs,
            format!(r#"{{"event":"test","event_id":"{EVENT_ID}","message":"test"}}"#).as_bytes(),
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
            assert!(
                IncomingWebhook::parse(SupplierKind::KiroRs, body.as_bytes()).is_err(),
                "{body}"
            );
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
        let page = store.list(10, None, None).unwrap();
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
        assert!(store.list(1, None, None).unwrap().items.is_empty());
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
        let listed = store.list(1, None, None).unwrap().items.remove(0);
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
        let item = &store.list(10, None, None).unwrap().items[0];
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
        let item = &store.list(1, None, None).unwrap().items[0];
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
        let failed = store.list(1, None, None).unwrap().items.remove(0);
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
        let retried = &store.list(1, None, None).unwrap().items[0];
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
            store.list(1, None, None).unwrap().items[0].status,
            SupplierEventStatus::Skipped
        );

        let mut below = service.runtime_config();
        below.auto_purchase = true;
        below.min_purchase = 2;
        service.set_runtime_config(below);
        store
            .insert_event(IncomingSupplierEvent {
                supplier_id: LEGACY_SUPPLIER_ID.to_string(),
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
                .list(10, None, None)
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
        let item = &store.list(1, None, None).unwrap().items[0];
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
        let item = &store.list(1, None, None).unwrap().items[0];
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
        let configuration_event = &configuration_store.list(1, None, None).unwrap().items[0];
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
        let api_event = &api_store.list(1, None, None).unwrap().items[0];
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
        let import_event = &import_store.list(1, None, None).unwrap().items[0];
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
        let failed = store.list(1, None, None).unwrap().items.remove(0);
        assert_eq!(failed.status, SupplierEventStatus::Failed);
        assert!(!format!("{failed:?}").contains("ksk_api_failure_canary"));
        store.retry(failed.id).unwrap();
        service.process_pending().await.unwrap();
        assert_eq!(
            store.list(1, None, None).unwrap().items[0].status,
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
        let event = &store.list(1, None, None).unwrap().items[0];
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
        let item = &store.list(1, None, None).unwrap().items[0];
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
            store.list(1, None, None).unwrap().items[0].status,
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
        assert_eq!(overview.snapshot.profile.as_ref().unwrap().name, "demo");
        assert_eq!(overview.snapshot.stock_available, Some(4));
        assert_eq!(
            overview.snapshot.status.as_ref().unwrap().keys_active,
            3
        );
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
                supplier_id: LEGACY_SUPPLIER_ID.to_string(),
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
            store.list(1, None, None).unwrap().items[0].status,
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
        let id = store.list(1, None, None).unwrap().items[0].id;
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
            store.list(1, None, None).unwrap().items[0].status,
            SupplierEventStatus::Processing
        );

        release.notify_one();
        assert_eq!(running.await.unwrap().unwrap(), 1);
        assert_eq!(recovery.await.unwrap().unwrap(), 0);
        assert_eq!(
            store.list(1, None, None).unwrap().items[0].status,
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
            store.list(1, None, None).unwrap().items[0].status,
            SupplierEventStatus::Received
        );
    }

    // ============ 多供货商 ============

    fn entry(id: &str, kind: SupplierKind, token: &str) -> SupplierEntryRuntime {
        let mut settings = runtime(token);
        settings.base_url = "https://supplier.example".to_string();
        settings.public_base_url = "https://admin.example".to_string();
        settings.auto_purchase = true;
        SupplierEntryRuntime {
            id: id.to_owned(),
            name: format!("supplier {id}"),
            kind,
            enabled: true,
            settings,
        }
    }

    fn multi_service(entries: Vec<SupplierEntryRuntime>) -> (Arc<KeySupplierService>, PathBuf) {
        let path = temp_config_path("multi");
        let mut config = Config::load(&path).unwrap();
        crate::admin::key_supplier::config::store_suppliers(&mut config, &entries);
        config.save().unwrap();
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        (
            Arc::new(
                KeySupplierService::with_suppliers(store, entries).with_config_path(&path),
            ),
            path,
        )
    }

    #[test]
    fn webhook_token_resolves_the_owning_supplier() {
        let first_token = "a".repeat(64);
        let second_token = "b".repeat(64);
        let (service, path) = multi_service(vec![
            entry("first", SupplierKind::KiroRs, &first_token),
            entry("second", SupplierKind::KiroApp, &second_token),
        ]);

        assert_eq!(
            service.resolve_webhook_token(&first_token).unwrap().id,
            "first"
        );
        let second = service.resolve_webhook_token(&second_token).unwrap();
        assert_eq!(second.id, "second");
        assert_eq!(second.kind, SupplierKind::KiroApp);
        assert!(service.resolve_webhook_token(&"c".repeat(64)).is_none());
        // 每家一个独立回调地址，token 不同 → URL 不同。
        assert_ne!(
            service.supplier_callback_url("first").unwrap(),
            service.supplier_callback_url("second").unwrap()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn events_are_scoped_per_supplier_and_same_event_id_does_not_collide() {
        let first_token = "a".repeat(64);
        let second_token = "b".repeat(64);
        let (service, path) = multi_service(vec![
            entry("first", SupplierKind::KiroRs, &first_token),
            entry("second", SupplierKind::KiroRs, &second_token),
        ]);
        let body = format!(
            r#"{{"event":"new_keys_available","event_id":"{EVENT_ID}","purchase_order_id":"{ORDER_ID}","message":"ready","new_keys":1}}"#
        );

        // 两家供货商推同一个 event_id：必须各自落一条，不能互相判重。
        let first = service.ingest(&first_token, &body).unwrap();
        let second = service.ingest(&second_token, &body).unwrap();
        assert!(!first.duplicate && !second.duplicate);
        assert_eq!(first.supplier_id, "first");
        assert_eq!(second.supplier_id, "second");

        let store = service.store();
        assert_eq!(store.list(10, None, None).unwrap().items.len(), 2);
        let scoped = store.list(10, None, Some("first")).unwrap().items;
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].supplier_id, "first");
        assert_eq!(store.unread_count(Some("second")).unwrap(), 1);
        store.mark_all_read(Some("second")).unwrap();
        assert_eq!(store.unread_count(Some("second")).unwrap(), 0);
        assert_eq!(store.unread_count(Some("first")).unwrap(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn repeated_kiroapp_push_is_deduplicated_so_it_never_buys_twice() {
        let token = "e".repeat(64);
        let (service, path) = multi_service(vec![entry("app", SupplierKind::KiroApp, &token)]);
        // kiroapp 的推送体没有稳定 event id，去重只能靠 body 指纹。
        let body = r#"{"event":"stock.ready","count":3}"#;

        let first = service.ingest(&token, body).unwrap();
        let second = service.ingest(&token, body).unwrap();
        let third = service.ingest(&token, body).unwrap();

        assert!(!first.duplicate);
        assert!(second.duplicate && third.duplicate);
        assert_eq!(first.event_id, second.event_id);
        let items = service.store().list(10, None, None).unwrap().items;
        assert_eq!(items.len(), 1, "重复推送不能产生第二条待处理事件");
        assert_eq!(items[0].webhook_duplicate_count, 2);
        assert_eq!(items[0].quantity, 3);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn kiroapp_webhook_tolerates_unknown_shapes_and_missing_counts() {
        // 数量字段名未文档化，取不到就按 0 落库（下单量再由库存与配置夹逼）。
        let bare = IncomingWebhook::parse(SupplierKind::KiroApp, br#"{"foo":"bar"}"#).unwrap();
        match bare {
            IncomingWebhook::NewKeysAvailable {
                new_keys, event_id, ..
            } => {
                assert_eq!(new_keys, 0);
                assert_eq!(event_id.len(), 32);
            }
            _ => panic!("kiroapp payload should be treated as a stock notification"),
        }

        for (body, expected) in [
            (r#"{"count":5}"#, 5),
            (r#"{"newKeys":2}"#, 2),
            (r#"{"keys":["a","b","c"]}"#, 3),
            (r#"{"availableKeys":"7"}"#, 7),
        ] {
            let parsed = IncomingWebhook::parse(SupplierKind::KiroApp, body.as_bytes()).unwrap();
            let IncomingWebhook::NewKeysAvailable { new_keys, .. } = parsed else {
                panic!("{body}");
            };
            assert_eq!(new_keys, expected, "{body}");
        }

        // 显式 test 事件不触发采购。
        assert!(matches!(
            IncomingWebhook::parse(SupplierKind::KiroApp, br#"{"event":"test"}"#).unwrap(),
            IncomingWebhook::Test { .. }
        ));
        // 非 JSON 仍然拒绝。
        assert!(IncomingWebhook::parse(SupplierKind::KiroApp, b"not json").is_err());
    }

    #[test]
    fn kiroapp_stable_event_id_is_reused_across_body_changes() {
        // 对方给了稳定 id 时以它为准：同一批次即使 body 其它字段变了也判重。
        let first =
            IncomingWebhook::parse(SupplierKind::KiroApp, br#"{"id":"batch-9","count":1}"#).unwrap();
        let second = IncomingWebhook::parse(
            SupplierKind::KiroApp,
            br#"{"id":"batch-9","count":1,"note":"retry"}"#,
        )
        .unwrap();
        let (
            IncomingWebhook::NewKeysAvailable {
                event_id: first_id,
                purchase_order_id: first_order,
                ..
            },
            IncomingWebhook::NewKeysAvailable {
                event_id: second_id,
                purchase_order_id: second_order,
                ..
            },
        ) = (first, second)
        else {
            panic!("both payloads should parse as stock notifications");
        };
        assert_eq!(first_id, second_id);
        assert_eq!(first_order, second_order);
    }

    #[tokio::test]
    async fn kiroapp_claim_imports_keys_and_is_never_retried() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let app = Router::new()
            .route(
                "/openapi/stock",
                get(|| async { axum::Json(serde_json::json!({"availableKeys": 5, "keyPrice": 1.5})) }),
            )
            .route(
                "/openapi/claim",
                post(move |request: axum::http::Request<axum::body::Body>| {
                    let observed = observed.clone();
                    async move {
                        // kiroapp 用 Bearer，不是 X-API-Key。
                        let authorization = request
                            .headers()
                            .get("authorization")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned();
                        let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                            .await
                            .unwrap();
                        observed
                            .lock()
                            .unwrap()
                            .push((authorization, String::from_utf8(body.to_vec()).unwrap()));
                        axum::Json(serde_json::json!({"keys":["ksk_one","ksk_two"]}))
                    }
                }),
            );
        let importer = Arc::new(FakeImporter::default());
        let mut supplier = entry("app", SupplierKind::KiroApp, &"f".repeat(64));
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "kiroapp-secret".to_string();
        supplier.settings.max_purchase = 4;
        supplier.settings.source_channel = "kiroapp".to_string();
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer.clone(),
        );

        let result = service.manual_purchase_from("app", 2).await.unwrap();

        assert_eq!(result.purchased, 2);
        assert_eq!(result.imported, 2);
        assert_eq!(result.supplier_id, "app");
        let requests = observed_requests(&calls);
        assert_eq!(requests.len(), 1, "claim 没有幂等键，绝不能重试");
        assert_eq!(requests[0].0, "Bearer kiroapp-secret");
        assert_eq!(requests[0].1, r#"{"count":2}"#);
        let imported = importer.credentials.lock().unwrap();
        assert_eq!(imported.len(), 2);
        assert_eq!(imported[0].kiro_api_key.as_deref(), Some("ksk_one"));
        assert_eq!(imported[0].source_channel.as_deref(), Some("kiroapp"));
    }

    #[tokio::test]
    async fn kiroapp_claim_failure_is_not_retried_and_records_diagnostics() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new()
            .route(
                "/openapi/stock",
                get(|| async { axum::Json(serde_json::json!({"availableKeys": 5})) }),
            )
            .route(
                "/openapi/claim",
                post(move || {
                    let observed = observed.clone();
                    async move {
                        *observed.lock().unwrap() += 1;
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            r#"{"error":{"type":"server_error","message":"boom"}}"#,
                        )
                    }
                }),
            );
        let mut supplier = entry("app", SupplierKind::KiroApp, &"9".repeat(64));
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "kiroapp-secret".to_string();
        supplier.settings.max_purchase = 4;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            Arc::new(FakeImporter::default()),
        );

        assert!(service.manual_purchase_from("app", 2).await.is_err());

        // 5xx 也不重试：宁可失败让人工重放，也不冒重复扣积分的风险。
        assert_eq!(*calls.lock().unwrap(), 1);
        let stored = store.list(1, None, None).unwrap().items.remove(0);
        assert_eq!(stored.status, SupplierEventStatus::Failed);
        assert!(stored.last_error.unwrap().contains("500"));
    }

    #[tokio::test]
    async fn kiroapp_rate_limit_is_surfaced_without_retrying() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new()
            .route(
                "/openapi/stock",
                get(|| async { axum::Json(serde_json::json!({"availableKeys": 5})) }),
            )
            .route(
                "/openapi/claim",
                post(move || {
                    let observed = observed.clone();
                    async move {
                        *observed.lock().unwrap() += 1;
                        (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            r#"{"error":{"type":"rate_limit_exceeded","message":"slow down","retryAfter":30}}"#,
                        )
                    }
                }),
            );
        let mut supplier = entry("app", SupplierKind::KiroApp, &"8".repeat(64));
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "kiroapp-secret".to_string();
        supplier.settings.max_purchase = 4;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            Arc::new(FakeImporter::default()),
        );

        assert!(service.manual_purchase_from("app", 2).await.is_err());

        assert_eq!(*calls.lock().unwrap(), 1);
        let stored = store.list(1, None, None).unwrap().items.remove(0);
        let error = stored.last_error.unwrap();
        assert!(error.contains("rate limited"), "{error}");
        assert!(error.contains("30"), "{error}");
    }

    #[tokio::test]
    async fn kiroapp_claims_directly_without_a_stock_precheck() {
        let claimed = Arc::new(Mutex::new(Vec::new()));
        let observed = claimed.clone();
        let stock_calls = Arc::new(Mutex::new(0_usize));
        let stock_observed = stock_calls.clone();
        let app = Router::new()
            .route(
                "/openapi/stock",
                get(move || {
                    let stock_observed = stock_observed.clone();
                    async move {
                        *stock_observed.lock().unwrap() += 1;
                        axum::Json(serde_json::json!({"availableKeys": 2}))
                    }
                }),
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
                        axum::Json(serde_json::json!({"keys":["ksk_a","ksk_b"]}))
                    }
                }),
            );
        let token = "7".repeat(64);
        let mut supplier = entry("app", SupplierKind::KiroApp, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "kiroapp-secret".to_string();
        supplier.settings.min_purchase = 1;
        supplier.settings.max_purchase = 5;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            Arc::new(FakeImporter::default()),
        );

        service.ingest(&token, r#"{"event":"stock.ready"}"#).unwrap();
        service.process_pending().await.unwrap();

        // 推送没带数量 → 直接按 max_purchase(5) 领取。
        // 对方文档明确要求「不要先查 /openapi/stock 再领取」：多一次往返就把货让给别人了。
        let requests = claimed.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0], r#"{"count":5}"#);
        assert_eq!(*stock_calls.lock().unwrap(), 0, "领取前不该查库存");
        assert_eq!(
            store.list(1, None, None).unwrap().items[0].status,
            SupplierEventStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn out_of_stock_is_recorded_as_skipped_instead_of_failed() {
        let app = Router::new().route(
            "/openapi/claim",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    r#"{"error":{"type":"out_of_stock","message":"库存不足"}}"#,
                )
            }),
        );
        let token = "6".repeat(64);
        let mut supplier = entry("app", SupplierKind::KiroApp, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "kiroapp-secret".to_string();
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            Arc::new(FakeImporter::default()),
        );

        service.ingest(&token, r#"{"count":2}"#).unwrap();
        service.process_pending().await.unwrap();

        // 被别人抢完是正常竞争结果：记 skipped，不记 failed，也不该有 last_error。
        let stored = store.list(1, None, None).unwrap().items.remove(0);
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        assert_eq!(stored.message.as_deref(), Some("库存已被抢完"));
        assert!(stored.last_error.is_none());
    }

    #[tokio::test]
    async fn webhook_signature_is_verified_when_a_secret_is_configured() {
        let token = "5".repeat(64);
        let secret = "hook-secret";
        let mut supplier = entry("app", SupplierKind::KiroApp, &token);
        supplier.settings.webhook_secret = secret.to_string();
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            Arc::new(FakeImporter::default()),
        );
        let body = r#"{"event":"stock","id":"evt_8DX2ZPK9MR7Q4JWH","count":3}"#;
        let signature = sign_webhook_body(secret, body.as_bytes());

        // 缺签名 / 错签名都必须拒，且不能落库。
        assert!(matches!(
            service.ingest_signed(&token, body, None),
            Err(SupplierServiceError::InvalidSignature)
        ));
        assert!(matches!(
            service.ingest_signed(&token, body, Some("deadbeef")),
            Err(SupplierServiceError::InvalidSignature)
        ));
        assert!(store.list(1, None, None).unwrap().items.is_empty());

        // 正确签名放行。
        let result = service
            .ingest_signed(&token, body, Some(&signature))
            .unwrap();
        assert!(!result.duplicate);
        // 原样保留对方的 event id，方便和对方后台的投递记录对账。
        assert_eq!(result.event_id, "evt_8DX2ZPK9MR7Q4JWH");

        // 同一 body 改一个字节，签名就该失配（证明验的是原始字节而非解析结果）。
        let tampered = r#"{"event":"stock","id":"evt_8DX2ZPK9MR7Q4JWH","count":4}"#;
        assert!(matches!(
            service.ingest_signed(&token, tampered, Some(&signature)),
            Err(SupplierServiceError::InvalidSignature)
        ));
    }

    #[test]
    fn signature_matches_the_documented_hmac_sha256_hex_scheme() {
        // 对方文档：hex(HMAC-SHA256(webhook_secret, raw_request_body))。
        let signature = sign_webhook_body("key", b"The quick brown fox jumps over the lazy dog");
        assert_eq!(
            signature,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[tokio::test]
    async fn unsigned_suppliers_still_accept_webhooks_without_a_signature() {
        // kiro-rs 不签名：没配 secret 就不验签，否则历史供货商会全挂。
        let token = "4".repeat(64);
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store,
            vec![entry("app", SupplierKind::KiroApp, &token)],
            Arc::new(FakeImporter::default()),
        );

        assert!(service.ingest(&token, r#"{"count":1}"#).is_ok());
    }

    #[tokio::test]
    async fn kiroapp_rejects_webhook_registration_and_exposes_a_manual_callback_url() {
        let (service, path) = multi_service(vec![entry(
            "app",
            SupplierKind::KiroApp,
            &"1".repeat(64),
        )]);

        assert!(matches!(
            service.register_supplier_webhook("app").await,
            Err(SupplierServiceError::WebhookRegistrationUnsupported)
        ));
        assert!(matches!(
            service.test_supplier_webhook("app").await,
            Err(SupplierServiceError::WebhookRegistrationUnsupported)
        ));
        // 手填地址仍然要能拿到，否则用户没法在对方面板配置。
        let callback = service.supplier_callback_url("app").unwrap();
        assert!(callback.starts_with("https://admin.example/api/admin/key-supplier/webhook/"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn supplier_crud_persists_and_rejects_conflicts_and_unknown_ids() {
        let (service, path) = multi_service(vec![entry(
            "first",
            SupplierKind::KiroRs,
            &"a".repeat(64),
        )]);

        let mut update = SupplierEntryUpdate {
            id: Some("kiroapp".to_owned()),
            name: "kiroapp.cc".to_owned(),
            kind: SupplierKind::KiroApp,
            enabled: true,
            settings: supplier_update(&entry("x", SupplierKind::KiroApp, &"b".repeat(64)).settings),
        };
        update.settings.api_key = Some("kiroapp-secret".to_owned());
        let created = service.upsert_supplier(None, update.clone()).unwrap();

        assert_eq!(created.id, "kiroapp");
        assert_eq!(created.kind, "kiro-app");
        assert!(!created.supports_webhook_registration);
        assert!(created.settings.api_key_configured);
        // 没给 token 时自动生成，否则收不到回调。
        assert!(created.settings.webhook_token_configured);

        let persisted = Config::load(&path).unwrap();
        assert_eq!(persisted.key_suppliers.len(), 2);
        // 历史字段镜像的是 kiro-rs 那家，回滚旧版本仍可用。
        assert_eq!(persisted.key_supplier.base_url, "https://supplier.example");

        // 同 id 再新增要冲突，未知 id 修改要 404。
        assert!(matches!(
            service.upsert_supplier(None, update.clone()),
            Err(SupplierServiceError::SupplierIdConflict)
        ));
        assert!(matches!(
            service.upsert_supplier(Some("missing".to_owned()), update.clone()),
            Err(SupplierServiceError::SupplierNotFound)
        ));

        // 修改时留空 secret 不覆盖已存的值。
        let mut edit = update.clone();
        edit.settings.api_key = None;
        edit.name = "kiroapp 改名".to_owned();
        let edited = service
            .upsert_supplier(Some("kiroapp".to_owned()), edit)
            .unwrap();
        assert_eq!(edited.name, "kiroapp 改名");
        assert!(edited.settings.api_key_configured);
        assert_eq!(
            service.supplier("kiroapp").unwrap().settings.api_key,
            "kiroapp-secret"
        );

        service.delete_supplier("kiroapp").unwrap();
        assert!(service.supplier("kiroapp").is_none());
        assert_eq!(Config::load(&path).unwrap().key_suppliers.len(), 1);
        assert!(matches!(
            service.delete_supplier("kiroapp"),
            Err(SupplierServiceError::SupplierNotFound)
        ));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn disabled_supplier_stores_events_but_skips_purchasing() {
        let token = "3".repeat(64);
        let mut supplier = entry("app", SupplierKind::KiroApp, &token);
        supplier.enabled = false;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            Arc::new(FakeImporter::default()),
        );

        service.ingest(&token, r#"{"count":2}"#).unwrap();
        service.process_pending().await.unwrap();

        // 关掉的供货商不下单（这里 base_url 指向不存在的服务，真下单会报错而不是 skipped）。
        assert_eq!(
            store.list(1, None, None).unwrap().items[0].status,
            SupplierEventStatus::Skipped
        );
    }

    #[test]
    fn legacy_single_supplier_config_migrates_into_the_list() {
        let mut config = Config::default();
        // 空壳配置不迁移。
        let (empty, migrated) =
            crate::admin::key_supplier::config::load_suppliers(&config).unwrap();
        assert!(empty.is_empty() && !migrated);

        config.key_supplier.base_url = "https://legacy.example".to_string();
        config.key_supplier.api_key = "legacy-secret".to_string();
        config.key_supplier.webhook_token = "a".repeat(64);
        let (entries, migrated) =
            crate::admin::key_supplier::config::load_suppliers(&config).unwrap();

        assert!(migrated);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, LEGACY_SUPPLIER_ID);
        assert_eq!(entries[0].kind, SupplierKind::KiroRs);
        assert!(entries[0].enabled);
        assert_eq!(entries[0].settings.base_url, "https://legacy.example");
        assert_eq!(entries[0].settings.webhook_token, "a".repeat(64));

        // 迁移后列表优先，历史字段被忽略。
        crate::admin::key_supplier::config::store_suppliers(&mut config, &entries);
        config.key_supplier.base_url = "https://stale.example".to_string();
        let (after, migrated) = crate::admin::key_supplier::config::load_suppliers(&config).unwrap();
        assert!(!migrated);
        assert_eq!(after[0].settings.base_url, "https://legacy.example");
    }

    #[test]
    fn duplicate_supplier_ids_on_disk_are_rejected() {
        let mut config = Config::default();
        let entries = vec![
            entry("same", SupplierKind::KiroRs, &"a".repeat(64)),
            entry("same", SupplierKind::KiroApp, &"b".repeat(64)),
        ];
        crate::admin::key_supplier::config::store_suppliers(&mut config, &entries);

        assert!(crate::admin::key_supplier::config::load_suppliers(&config).is_err());
    }

    fn observed_requests(calls: &Arc<Mutex<Vec<(String, String)>>>) -> Vec<(String, String)> {
        calls.lock().unwrap().clone()
    }
}
