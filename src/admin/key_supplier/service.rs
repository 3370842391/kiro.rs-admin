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

pub struct KeySupplierService {
    store: Arc<SupplierEventStore>,
    runtime: parking_lot::RwLock<SupplierRuntimeConfig>,
}

impl KeySupplierService {
    pub fn new(store: Arc<SupplierEventStore>, runtime: SupplierRuntimeConfig) -> Self {
        Self {
            store,
            runtime: parking_lot::RwLock::new(runtime),
        }
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
    Store,
}

impl fmt::Display for SupplierServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unauthorized => "webhook authentication failed",
            Self::InvalidJson => "invalid webhook JSON",
            Self::InvalidPayload => "invalid webhook payload",
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
            Self::Store => "Store",
        })
    }
}

impl std::error::Error for SupplierServiceError {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::admin::key_supplier::config::SupplierRuntimeConfig;
    use crate::admin::key_supplier::store::{SupplierEventStatus, SupplierEventStore};

    const TOKEN: &str = "webhook-token-canary";
    const EVENT_ID: &str = "0123456789abcdef0123456789abcdef";
    const ORDER_ID: &str = "fedcba9876543210fedcba9876543210";

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
}
use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::admin::key_supplier::config::SupplierRuntimeConfig;
use crate::admin::key_supplier::store::{IncomingSupplierEvent, InsertOutcome, SupplierEventStore};
