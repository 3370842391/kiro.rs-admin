use std::collections::HashSet;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::kiro::region::validate_api_region;
use crate::model::config::{Config, KeySupplierConfig};

const MAX_URL_CHARS: usize = 2_048;
const MAX_SECRET_CHARS: usize = 4_096;
const MAX_GROUPS: usize = 64;
const MAX_GROUP_NAME_CHARS: usize = 64;
const MAX_SOURCE_CHANNEL_CHARS: usize = 128;
const MAX_NICKNAME_PREFIX_CHARS: usize = 128;
const MAX_PURCHASE: u64 = 10_000;
const MAX_RPM_LIMIT: u64 = 100_000;
const MAX_PRIORITY: u64 = u32::MAX as u64;

#[derive(Clone, PartialEq, Eq)]
pub struct SupplierRuntimeConfig {
    pub base_url: String,
    pub api_key: String,
    pub public_base_url: String,
    pub webhook_token: String,
    pub auto_purchase: bool,
    pub min_purchase: u32,
    pub max_purchase: u32,
    pub api_region: String,
    pub rpm_limit: u32,
    pub priority: u32,
    pub groups: Vec<String>,
    pub source_channel: String,
    pub nickname_prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplierConfigView {
    pub base_url: String,
    pub api_key_configured: bool,
    pub public_base_url: String,
    pub webhook_token_configured: bool,
    pub auto_purchase: bool,
    pub min_purchase: u32,
    pub max_purchase: u32,
    pub api_region: String,
    pub rpm_limit: u32,
    pub priority: u32,
    pub groups: Vec<String>,
    pub source_channel: String,
    pub nickname_prefix: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplierConfigUpdate {
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub public_base_url: String,
    #[serde(default)]
    pub webhook_token: Option<String>,
    pub auto_purchase: bool,
    pub min_purchase: u64,
    pub max_purchase: u64,
    pub api_region: String,
    pub rpm_limit: u64,
    pub priority: u64,
    #[serde(default)]
    pub groups: Vec<String>,
    pub source_channel: String,
    pub nickname_prefix: String,
}

impl std::fmt::Debug for SupplierRuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupplierRuntimeConfig")
            .field("base_url", &self.base_url)
            .field("api_key_configured", &(!self.api_key.is_empty()))
            .field("public_base_url", &self.public_base_url)
            .field(
                "webhook_token_configured",
                &(!self.webhook_token.is_empty()),
            )
            .field("auto_purchase", &self.auto_purchase)
            .field("min_purchase", &self.min_purchase)
            .field("max_purchase", &self.max_purchase)
            .field("api_region", &self.api_region)
            .field("rpm_limit", &self.rpm_limit)
            .field("priority", &self.priority)
            .field("groups", &self.groups)
            .field("source_channel", &self.source_channel)
            .field("nickname_prefix", &self.nickname_prefix)
            .finish()
    }
}

impl std::fmt::Debug for SupplierConfigUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupplierConfigUpdate")
            .field("base_url", &self.base_url)
            .field(
                "api_key_configured",
                &self.api_key.as_ref().is_some_and(|v| !v.is_empty()),
            )
            .field("public_base_url", &self.public_base_url)
            .field(
                "webhook_token_configured",
                &self.webhook_token.as_ref().is_some_and(|v| !v.is_empty()),
            )
            .field("auto_purchase", &self.auto_purchase)
            .field("min_purchase", &self.min_purchase)
            .field("max_purchase", &self.max_purchase)
            .field("api_region", &self.api_region)
            .field("rpm_limit", &self.rpm_limit)
            .field("priority", &self.priority)
            .field("groups", &self.groups)
            .field("source_channel", &self.source_channel)
            .field("nickname_prefix", &self.nickname_prefix)
            .finish()
    }
}

impl SupplierRuntimeConfig {
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        normalize_persisted(&config.key_supplier)
    }

    pub fn apply(config: &mut Config, update: SupplierConfigUpdate) -> anyhow::Result<Self> {
        let runtime = Self::normalize(config, update, true)?;
        config.key_supplier = KeySupplierConfig::from(&runtime);
        Ok(runtime)
    }

    fn normalize(
        config: &Config,
        update: SupplierConfigUpdate,
        generate_missing_webhook_token: bool,
    ) -> anyhow::Result<Self> {
        validate_number_range(update.min_purchase, "minPurchase", 1, MAX_PURCHASE)?;
        validate_number_range(update.max_purchase, "maxPurchase", 1, MAX_PURCHASE)?;
        if update.min_purchase > update.max_purchase {
            anyhow::bail!("minPurchase 不能大于 maxPurchase");
        }
        validate_number_range(update.rpm_limit, "rpmLimit", 0, MAX_RPM_LIMIT)?;
        validate_number_range(update.priority, "priority", 0, MAX_PRIORITY)?;

        let base_url = normalize_http_url(&update.base_url, "baseUrl")?;
        let public_base_url = normalize_http_url(&update.public_base_url, "publicBaseUrl")?;
        let api_region = validate_api_region(&update.api_region)?.to_string();
        let api_key = normalize_secret(
            update
                .api_key
                .as_deref()
                .unwrap_or(&config.key_supplier.api_key),
            "apiKey",
        )?;
        let mut webhook_token = normalize_secret(
            update
                .webhook_token
                .as_deref()
                .unwrap_or(&config.key_supplier.webhook_token),
            "webhookToken",
        )?;
        if generate_missing_webhook_token && webhook_token.is_empty() {
            webhook_token = generate_webhook_token();
        }

        let runtime = Self {
            base_url,
            api_key,
            public_base_url,
            webhook_token,
            auto_purchase: update.auto_purchase,
            min_purchase: update.min_purchase as u32,
            max_purchase: update.max_purchase as u32,
            api_region,
            rpm_limit: update.rpm_limit as u32,
            priority: update.priority as u32,
            groups: normalize_groups(update.groups)?,
            source_channel: normalize_text(
                &update.source_channel,
                "sourceChannel",
                MAX_SOURCE_CHANNEL_CHARS,
            )?,
            nickname_prefix: normalize_text(
                &update.nickname_prefix,
                "nicknamePrefix",
                MAX_NICKNAME_PREFIX_CHARS,
            )?,
        };

        Ok(runtime)
    }
}

impl From<&SupplierRuntimeConfig> for KeySupplierConfig {
    fn from(value: &SupplierRuntimeConfig) -> Self {
        Self {
            base_url: value.base_url.clone(),
            api_key: value.api_key.clone(),
            public_base_url: value.public_base_url.clone(),
            webhook_token: value.webhook_token.clone(),
            auto_purchase: value.auto_purchase,
            min_purchase: value.min_purchase,
            max_purchase: value.max_purchase,
            api_region: value.api_region.clone(),
            rpm_limit: value.rpm_limit,
            priority: value.priority,
            groups: value.groups.clone(),
            source_channel: value.source_channel.clone(),
            nickname_prefix: value.nickname_prefix.clone(),
        }
    }
}

impl From<&SupplierRuntimeConfig> for SupplierConfigView {
    fn from(value: &SupplierRuntimeConfig) -> Self {
        Self {
            base_url: value.base_url.clone(),
            api_key_configured: !value.api_key.is_empty(),
            public_base_url: value.public_base_url.clone(),
            webhook_token_configured: !value.webhook_token.is_empty(),
            auto_purchase: value.auto_purchase,
            min_purchase: value.min_purchase,
            max_purchase: value.max_purchase,
            api_region: value.api_region.clone(),
            rpm_limit: value.rpm_limit,
            priority: value.priority,
            groups: value.groups.clone(),
            source_channel: value.source_channel.clone(),
            nickname_prefix: value.nickname_prefix.clone(),
        }
    }
}

pub fn generate_webhook_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn normalize_persisted(value: &KeySupplierConfig) -> anyhow::Result<SupplierRuntimeConfig> {
    let update = SupplierConfigUpdate {
        base_url: value.base_url.clone(),
        api_key: Some(value.api_key.clone()),
        public_base_url: value.public_base_url.clone(),
        webhook_token: Some(value.webhook_token.clone()),
        auto_purchase: value.auto_purchase,
        min_purchase: u64::from(value.min_purchase),
        max_purchase: u64::from(value.max_purchase),
        api_region: value.api_region.clone(),
        rpm_limit: u64::from(value.rpm_limit),
        priority: u64::from(value.priority),
        groups: value.groups.clone(),
        source_channel: value.source_channel.clone(),
        nickname_prefix: value.nickname_prefix.clone(),
    };
    let mut config = Config::default();
    config.key_supplier = value.clone();
    SupplierRuntimeConfig::normalize(&config, update, false)
}

fn normalize_http_url(value: &str, field: &str) -> anyhow::Result<String> {
    let value = normalize_text(value, field, MAX_URL_CHARS)?;
    if value.is_empty() {
        return Ok(value);
    }
    let parsed = reqwest::Url::parse(&value).with_context(|| format!("{field} 不是有效 URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        anyhow::bail!("{field} 必须为空或使用 http(s) URL");
    }
    Ok(value.trim_end_matches('/').to_string())
}

fn normalize_secret(value: &str, field: &str) -> anyhow::Result<String> {
    normalize_text(value, field, MAX_SECRET_CHARS)
}

fn normalize_text(value: &str, field: &str, max_chars: usize) -> anyhow::Result<String> {
    let value = value.trim();
    if value.chars().count() > max_chars {
        anyhow::bail!("{field} 最多允许 {max_chars} 个字符");
    }
    Ok(value.to_string())
}

fn normalize_groups(groups: Vec<String>) -> anyhow::Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for group in groups {
        let group = group.trim();
        if group.is_empty() {
            continue;
        }
        if group.chars().count() > MAX_GROUP_NAME_CHARS {
            anyhow::bail!("分组名最多允许 {MAX_GROUP_NAME_CHARS} 个字符");
        }
        if seen.insert(group.to_string()) {
            normalized.push(group.to_string());
        }
    }
    if normalized.len() > MAX_GROUPS {
        anyhow::bail!("groups 最多允许 {MAX_GROUPS} 个分组");
    }
    Ok(normalized)
}

fn validate_number_range(value: u64, field: &str, min: u64, max: u64) -> anyhow::Result<()> {
    if !(min..=max).contains(&value) {
        anyhow::bail!("{field} 必须在 {min}..={max} 之间");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::Config;

    fn valid_update() -> SupplierConfigUpdate {
        SupplierConfigUpdate {
            base_url: " https://supplier.example/api/ ".to_string(),
            api_key: Some(" supplier-secret ".to_string()),
            public_base_url: " https://public.example/ ".to_string(),
            webhook_token: None,
            auto_purchase: true,
            min_purchase: 2,
            max_purchase: 5,
            api_region: " us-east-1 ".to_string(),
            rpm_limit: 100,
            priority: 10,
            groups: vec![
                " production ".to_string(),
                "production".to_string(),
                " backup ".to_string(),
            ],
            source_channel: " Webhook 自动采购 ".to_string(),
            nickname_prefix: " 自动采购 ".to_string(),
        }
    }

    #[test]
    fn rejects_inverted_purchase_range() {
        let mut update = valid_update();
        update.min_purchase = 6;
        update.max_purchase = 5;

        let mut config = Config::default();

        assert!(SupplierRuntimeConfig::apply(&mut config, update).is_err());
    }

    #[test]
    fn rejects_invalid_urls_and_api_region() {
        for (base_url, public_base_url, api_region) in [
            ("ftp://supplier.example", "", "us-east-1"),
            (
                "https://supplier.example",
                "file:///tmp/callback",
                "us-east-1",
            ),
            (
                "https://supplier.example",
                "https://public.example",
                "ap-southeast-1",
            ),
        ] {
            let mut update = valid_update();
            update.base_url = base_url.to_string();
            update.public_base_url = public_base_url.to_string();
            update.api_region = api_region.to_string();
            let mut config = Config::default();
            assert!(SupplierRuntimeConfig::apply(&mut config, update).is_err());
        }
    }

    #[test]
    fn apply_normalizes_values_and_generates_missing_webhook_token() {
        let mut config = Config::default();
        let runtime = SupplierRuntimeConfig::apply(&mut config, valid_update()).unwrap();

        assert_eq!(runtime.base_url, "https://supplier.example/api");
        assert_eq!(runtime.public_base_url, "https://public.example");
        assert_eq!(runtime.api_region, "us-east-1");
        assert_eq!(runtime.groups, vec!["production", "backup"]);
        assert_eq!(runtime.webhook_token.len(), 64);
        assert!(
            runtime
                .webhook_token
                .chars()
                .all(|ch| ch.is_ascii_hexdigit())
        );
        assert_eq!(config.key_supplier.base_url, runtime.base_url);
        assert_eq!(config.key_supplier.webhook_token, runtime.webhook_token);
    }

    #[test]
    fn view_never_serializes_sensitive_values() {
        let mut config = Config::default();
        let runtime = SupplierRuntimeConfig::apply(&mut config, valid_update()).unwrap();
        let api_key = runtime.api_key.clone();
        let webhook_token = runtime.webhook_token.clone();
        let value = serde_json::to_value(SupplierConfigView::from(&runtime)).unwrap();
        let encoded = value.to_string();

        assert_eq!(value["apiKeyConfigured"], true);
        assert_eq!(value["webhookTokenConfigured"], true);
        assert!(value.get("apiKey").is_none());
        assert!(value.get("webhookToken").is_none());
        assert!(!encoded.contains(&api_key));
        assert!(!encoded.contains(&webhook_token));
    }

    #[test]
    fn loading_config_does_not_claim_an_unpersisted_webhook_token() {
        let runtime = SupplierRuntimeConfig::from_config(&Config::default()).unwrap();
        let view = SupplierConfigView::from(&runtime);

        assert!(runtime.webhook_token.is_empty());
        assert!(!view.webhook_token_configured);
    }

    #[test]
    fn debug_does_not_expose_supplier_runtime_secrets() {
        let mut runtime = SupplierRuntimeConfig::from_config(&Config::default()).unwrap();
        runtime.api_key = "runtime-api-key-canary".to_string();
        runtime.webhook_token = "runtime-webhook-token-canary".to_string();

        let debug = format!("{:?}", runtime);

        assert!(!debug.contains("runtime-api-key-canary"));
        assert!(!debug.contains("runtime-webhook-token-canary"));
    }

    #[test]
    fn debug_does_not_expose_supplier_update_secrets() {
        let mut update = valid_update();
        update.api_key = Some("update-api-key-canary".to_string());
        update.webhook_token = Some("update-webhook-token-canary".to_string());

        let debug = format!("{:?}", update);

        assert!(!debug.contains("update-api-key-canary"));
        assert!(!debug.contains("update-webhook-token-canary"));
    }
}
