use std::collections::HashSet;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::kiro::region::validate_api_region;
use crate::model::config::{Config, KeySupplierConfig, KeySupplierEntryConfig, SupplierKind};

const MAX_URL_CHARS: usize = 2_048;
const MAX_SECRET_CHARS: usize = 4_096;
const MAX_GROUPS: usize = 64;
const MAX_GROUP_NAME_CHARS: usize = 64;
const MAX_SOURCE_CHANNEL_CHARS: usize = 128;
const MAX_NICKNAME_PREFIX_CHARS: usize = 128;
const MAX_PURCHASE: u64 = 10_000;
const MAX_RPM_LIMIT: u64 = 100_000;
const MAX_PRIORITY: u64 = u32::MAX as u64;
const MAX_SUPPLIER_ID_CHARS: usize = 64;
const MAX_SUPPLIER_NAME_CHARS: usize = 128;
/// 单实例能挂的供货商上限。够用又不至于让 webhook token 反查退化成长列表扫描。
pub const MAX_SUPPLIERS: usize = 32;

/// 一家供货商的运行期配置：身份（id/name/kind/enabled）+ 连接与导入预设。
#[derive(Clone, PartialEq, Eq)]
pub struct SupplierEntryRuntime {
    pub id: String,
    pub name: String,
    pub kind: SupplierKind,
    pub enabled: bool,
    pub settings: SupplierRuntimeConfig,
}

impl std::fmt::Debug for SupplierEntryRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupplierEntryRuntime")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("enabled", &self.enabled)
            .field("settings", &self.settings)
            .finish()
    }
}

impl SupplierEntryRuntime {
    /// 从持久化条目读取，校验但不生成缺失的 webhook token。
    pub fn from_persisted(entry: &KeySupplierEntryConfig) -> anyhow::Result<Self> {
        Ok(Self {
            id: normalize_supplier_id(&entry.id)?,
            name: normalize_text(&entry.name, "name", MAX_SUPPLIER_NAME_CHARS)?,
            kind: entry.kind,
            enabled: entry.enabled,
            settings: normalize_persisted(&entry.settings)?,
        })
    }

    /// 该供货商是否具备发起采购的最小条件。
    pub fn is_operable(&self) -> bool {
        !self.settings.base_url.trim().is_empty() && !self.settings.api_key.trim().is_empty()
    }
}

impl From<&SupplierEntryRuntime> for KeySupplierEntryConfig {
    fn from(value: &SupplierEntryRuntime) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            kind: value.kind,
            enabled: value.enabled,
            settings: KeySupplierConfig::from(&value.settings),
        }
    }
}

/// 供货商列表项的对外视图，secret 只报「是否已配置」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplierEntryView {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub enabled: bool,
    pub supports_webhook_registration: bool,
    #[serde(flatten)]
    pub settings: SupplierConfigView,
}

impl From<&SupplierEntryRuntime> for SupplierEntryView {
    fn from(value: &SupplierEntryRuntime) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            kind: value.kind.as_str(),
            enabled: value.enabled,
            supports_webhook_registration: value.kind.supports_webhook_registration(),
            settings: SupplierConfigView::from(&value.settings),
        }
    }
}

/// 新增/修改一家供货商的入参。`id` 仅新增时使用，修改时以路径参数为准。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplierEntryUpdate {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: SupplierKind,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(flatten)]
    pub settings: SupplierConfigUpdate,
}

fn default_enabled() -> bool {
    true
}

impl std::fmt::Debug for SupplierEntryUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupplierEntryUpdate")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("enabled", &self.enabled)
            .field("settings", &self.settings)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SupplierRuntimeConfig {
    pub base_url: String,
    pub api_key: String,
    pub public_base_url: String,
    pub webhook_token: String,
    /// 留空 = 不验签。配上则校验 `X-Kiro-Signature`。
    pub webhook_secret: String,
    pub auto_purchase: bool,
    pub auto_delete_forbidden: bool,
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
    pub webhook_secret_configured: bool,
    pub auto_purchase: bool,
    pub auto_delete_forbidden: bool,
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
    #[serde(default)]
    pub webhook_secret: Option<String>,
    pub auto_purchase: bool,
    #[serde(default)]
    pub auto_delete_forbidden: bool,
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
            .field(
                "webhook_secret_configured",
                &(!self.webhook_secret.is_empty()),
            )
            .field("auto_purchase", &self.auto_purchase)
            .field("auto_delete_forbidden", &self.auto_delete_forbidden)
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
            .field(
                "webhook_secret_configured",
                &self.webhook_secret.as_ref().is_some_and(|v| !v.is_empty()),
            )
            .field("auto_purchase", &self.auto_purchase)
            .field("auto_delete_forbidden", &self.auto_delete_forbidden)
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
    /// 校验并规范化一份连接配置。`existing` 提供「留空 secret 不覆盖」的旧值。
    ///
    /// 缺失的 webhook token 不在这里生成——由 `SupplierEntryRuntime::normalize_update`
    /// 统一补，避免持久化读取路径意外「凭空」得到一个 token。
    pub fn normalize_standalone(
        update: SupplierConfigUpdate,
        existing: Option<&Self>,
    ) -> anyhow::Result<Self> {
        Self::normalize(existing, update, false)
    }

    fn normalize(
        existing: Option<&Self>,
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

        let base_url = normalize_http_origin(&update.base_url, "baseUrl")?;
        let public_base_url = normalize_http_origin(&update.public_base_url, "publicBaseUrl")?;
        let api_region = validate_api_region(&update.api_region)?.to_string();
        let api_key = normalize_secret(
            update
                .api_key
                .as_deref()
                .unwrap_or_else(|| existing.map_or("", |value| value.api_key.as_str())),
            "apiKey",
        )?;
        let mut webhook_token = normalize_webhook_token(
            update
                .webhook_token
                .as_deref()
                .unwrap_or_else(|| existing.map_or("", |value| value.webhook_token.as_str())),
        )?;
        if generate_missing_webhook_token && webhook_token.is_empty() {
            webhook_token = generate_webhook_token();
        }
        // 签名密钥由供货商生成，格式不由我们定，只做长度上限校验。
        let webhook_secret = normalize_secret(
            update
                .webhook_secret
                .as_deref()
                .unwrap_or_else(|| existing.map_or("", |value| value.webhook_secret.as_str())),
            "webhookSecret",
        )?;

        let runtime = Self {
            base_url,
            api_key,
            public_base_url,
            webhook_token,
            webhook_secret,
            auto_purchase: update.auto_purchase,
            auto_delete_forbidden: update.auto_delete_forbidden,
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
            webhook_secret: value.webhook_secret.clone(),
            auto_purchase: value.auto_purchase,
            auto_delete_forbidden: value.auto_delete_forbidden,
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
            webhook_secret_configured: !value.webhook_secret.is_empty(),
            auto_purchase: value.auto_purchase,
            auto_delete_forbidden: value.auto_delete_forbidden,
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

/// 供货商 id：小写字母/数字/`-`/`_`，用于 URL 路径与事件表外键，创建后不可改。
pub fn normalize_supplier_id(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("供货商 id 不能为空");
    }
    if value.chars().count() > MAX_SUPPLIER_ID_CHARS {
        anyhow::bail!("供货商 id 最多允许 {MAX_SUPPLIER_ID_CHARS} 个字符");
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        anyhow::bail!("供货商 id 只能包含字母、数字、- 和 _");
    }
    Ok(value.to_ascii_lowercase())
}

/// 读取整份供货商列表。空列表且历史单供货商已配置时，迁移出 `default` 一项。
///
/// 返回 `(列表, 是否发生迁移)`；迁移标记由调用方决定要不要落盘。
pub fn load_suppliers(config: &Config) -> anyhow::Result<(Vec<SupplierEntryRuntime>, bool)> {
    if config.key_suppliers.is_empty() {
        if !KeySupplierEntryConfig::legacy_is_configured(&config.key_supplier) {
            return Ok((Vec::new(), false));
        }
        let migrated = KeySupplierEntryConfig::from_legacy(config.key_supplier.clone());
        return Ok((vec![SupplierEntryRuntime::from_persisted(&migrated)?], true));
    }

    let mut seen = HashSet::new();
    let mut entries = Vec::with_capacity(config.key_suppliers.len());
    for entry in &config.key_suppliers {
        let runtime = SupplierEntryRuntime::from_persisted(entry)?;
        if !seen.insert(runtime.id.clone()) {
            anyhow::bail!("供货商 id 重复: {}", runtime.id);
        }
        entries.push(runtime);
    }
    Ok((entries, false))
}

/// 把内存里的供货商列表写回 `Config`（调用方负责 `save()`）。
///
/// 同时把第一个 `kiro-rs` 供货商镜像回历史 `keySupplier` 字段：镜像本身不参与读取
/// （启动时只在 `keySuppliers` 为空才看它），存在的意义是回滚到旧版本后主供货商仍可用。
pub fn store_suppliers(config: &mut Config, entries: &[SupplierEntryRuntime]) {
    config.key_suppliers = entries.iter().map(KeySupplierEntryConfig::from).collect();
    if let Some(primary) = entries
        .iter()
        .find(|entry| entry.enabled && entry.kind == SupplierKind::KiroRs)
        .or_else(|| {
            entries
                .iter()
                .find(|entry| entry.kind == SupplierKind::KiroRs)
        })
    {
        config.key_supplier = KeySupplierConfig::from(&primary.settings);
    }
}

impl SupplierEntryRuntime {
    /// 校验一条新增/修改请求。`id` 已定则沿用（改），否则从入参取（新增）。
    ///
    /// `existing` 是同一条的旧值，用于「留空 secret 不覆盖」语义。
    pub fn normalize_update(
        id: Option<&str>,
        update: SupplierEntryUpdate,
        existing: Option<&SupplierEntryRuntime>,
    ) -> anyhow::Result<Self> {
        let id = match id {
            Some(id) => normalize_supplier_id(id)?,
            None => normalize_supplier_id(
                update
                    .id
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("新增供货商必须提供 id"))?,
            )?,
        };
        let name = normalize_text(&update.name, "name", MAX_SUPPLIER_NAME_CHARS)?;
        let mut settings = SupplierRuntimeConfig::normalize_standalone(
            update.settings,
            existing.map(|entry| &entry.settings),
        )?;
        if settings.webhook_token.is_empty() {
            settings.webhook_token = generate_webhook_token();
        }
        Ok(Self {
            id,
            name,
            kind: update.kind,
            enabled: update.enabled,
            settings,
        })
    }
}

pub fn is_valid_webhook_token(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn normalize_persisted(value: &KeySupplierConfig) -> anyhow::Result<SupplierRuntimeConfig> {
    let update = SupplierConfigUpdate {
        base_url: value.base_url.clone(),
        api_key: Some(value.api_key.clone()),
        public_base_url: value.public_base_url.clone(),
        webhook_token: Some(value.webhook_token.clone()),
        webhook_secret: Some(value.webhook_secret.clone()),
        auto_purchase: value.auto_purchase,
        auto_delete_forbidden: value.auto_delete_forbidden,
        min_purchase: u64::from(value.min_purchase),
        max_purchase: u64::from(value.max_purchase),
        api_region: value.api_region.clone(),
        rpm_limit: u64::from(value.rpm_limit),
        priority: u64::from(value.priority),
        groups: value.groups.clone(),
        source_channel: value.source_channel.clone(),
        nickname_prefix: value.nickname_prefix.clone(),
    };
    SupplierRuntimeConfig::normalize(None, update, false)
}

fn normalize_http_origin(value: &str, field: &str) -> anyhow::Result<String> {
    let value = normalize_text(value, field, MAX_URL_CHARS)?;
    if value.is_empty() {
        return Ok(value);
    }
    let parsed = reqwest::Url::parse(&value).with_context(|| format!("{field} 不是有效 URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        anyhow::bail!("{field} 必须为空或使用 http(s) URL");
    }
    if (parsed.path() != "" && parsed.path() != "/")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        anyhow::bail!("{field} must contain only an http(s) origin");
    }
    Ok(value.trim_end_matches('/').to_string())
}

fn normalize_secret(value: &str, field: &str) -> anyhow::Result<String> {
    normalize_text(value, field, MAX_SECRET_CHARS)
}

fn normalize_webhook_token(value: &str) -> anyhow::Result<String> {
    let token = normalize_secret(value, "webhookToken")?;
    if !token.is_empty() && !is_valid_webhook_token(&token) {
        anyhow::bail!("webhookToken must be empty or 64 hexadecimal characters");
    }
    Ok(token)
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
            base_url: " https://supplier.example/ ".to_string(),
            api_key: Some(" supplier-secret ".to_string()),
            public_base_url: " https://public.example/ ".to_string(),
            webhook_token: None,
            webhook_secret: None,
            auto_purchase: true,
            auto_delete_forbidden: true,
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

        assert!(SupplierRuntimeConfig::normalize_standalone(update, None).is_err());
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
            assert!(SupplierRuntimeConfig::normalize_standalone(update, None).is_err());
        }
    }

    #[test]
    fn rejects_urls_that_are_not_origins() {
        for value in [
            "https://supplier.example/api",
            "https://user:pass@supplier.example",
            "https://supplier.example/?query=1",
            "https://supplier.example/#fragment",
        ] {
            let mut update = valid_update();
            update.base_url = value.to_string();
            assert!(
                SupplierRuntimeConfig::normalize_standalone(update, None).is_err(),
                "{value}"
            );

            let mut update = valid_update();
            update.public_base_url = value.to_string();
            assert!(
                SupplierRuntimeConfig::normalize_standalone(update, None).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn normalization_trims_values_and_deduplicates_groups() {
        let runtime = SupplierRuntimeConfig::normalize_standalone(valid_update(), None).unwrap();

        assert_eq!(runtime.base_url, "https://supplier.example");
        assert_eq!(runtime.public_base_url, "https://public.example");
        assert_eq!(runtime.api_region, "us-east-1");
        assert_eq!(runtime.groups, vec!["production", "backup"]);
        assert!(runtime.auto_delete_forbidden);
        assert_eq!(runtime.api_key, "supplier-secret");
        assert_eq!(runtime.source_channel, "Webhook 自动采购");
    }

    #[test]
    fn entry_normalization_generates_a_missing_webhook_token() {
        let mut update = valid_update();
        update.webhook_token = None;
        let entry = SupplierEntryRuntime::normalize_update(
            None,
            SupplierEntryUpdate {
                id: Some("kiroapp".to_owned()),
                name: " kiroapp.cc ".to_owned(),
                kind: SupplierKind::KiroApp,
                enabled: true,
                settings: update,
            },
            None,
        )
        .unwrap();

        assert_eq!(entry.id, "kiroapp");
        assert_eq!(entry.name, "kiroapp.cc");
        assert_eq!(entry.kind, SupplierKind::KiroApp);
        // 没有 token 就收不到回调，所以这里必须自动补一个。
        assert_eq!(entry.settings.webhook_token.len(), 64);
        assert!(
            entry
                .settings
                .webhook_token
                .chars()
                .all(|ch| ch.is_ascii_hexdigit())
        );
    }

    #[test]
    fn supplier_ids_are_lowercased_and_restricted_to_url_safe_values() {
        assert_eq!(normalize_supplier_id("  KiroApp  ").unwrap(), "kiroapp");
        assert_eq!(normalize_supplier_id("vendor_1-x").unwrap(), "vendor_1-x");
        for invalid in ["", "   ", "has space", "slash/es", "dots.", "中文", &"a".repeat(65)] {
            assert!(normalize_supplier_id(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn webhook_token_must_be_empty_or_64_hex_when_loading_and_updating() {
        for token in ["webhook-token".to_owned(), "a".repeat(63), "g".repeat(64)] {
            let mut update = valid_update();
            update.webhook_token = Some(token.clone());
            assert!(SupplierRuntimeConfig::normalize_standalone(update, None).is_err());

            let mut config = Config::default();
            config.key_supplier.webhook_token = token;
            // 持久化里的坏 token 也要在读取时就被拒绝，而不是运行时才炸。
            assert!(load_suppliers(&config).is_err() || config.key_supplier.base_url.is_empty());
            config.key_supplier.base_url = "https://supplier.example".to_string();
            assert!(load_suppliers(&config).is_err());
        }

        let mut update = valid_update();
        let valid = "a".repeat(64);
        update.webhook_token = Some(valid.clone());
        assert_eq!(
            SupplierRuntimeConfig::normalize_standalone(update, None)
                .unwrap()
                .webhook_token,
            valid
        );
        // 读取路径不会凭空造 token。
        assert!(
            SupplierRuntimeConfig::normalize_standalone(
                SupplierConfigUpdate {
                    webhook_token: Some(String::new()),
                    webhook_secret: None,
                    ..valid_update()
                },
                None
            )
            .unwrap()
            .webhook_token
            .is_empty()
        );
    }

    #[test]
    fn view_never_serializes_sensitive_values() {
        let runtime = SupplierRuntimeConfig::normalize_standalone(
            SupplierConfigUpdate {
                webhook_token: Some("a".repeat(64)),
                webhook_secret: None,
                ..valid_update()
            },
            None,
        )
        .unwrap();
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
        let (entries, migrated) = load_suppliers(&Config::default()).unwrap();

        // 空配置不该凭空长出一家供货商，更不该长出 token。
        assert!(entries.is_empty());
        assert!(!migrated);

        let mut config = Config::default();
        config.key_supplier.base_url = "https://supplier.example".to_string();
        let (entries, migrated) = load_suppliers(&config).unwrap();
        assert!(migrated);
        assert!(entries[0].settings.webhook_token.is_empty());
        assert!(!SupplierConfigView::from(&entries[0].settings).webhook_token_configured);
    }

    #[test]
    fn debug_does_not_expose_supplier_runtime_secrets() {
        let mut runtime = SupplierRuntimeConfig::normalize_standalone(valid_update(), None).unwrap();
        runtime.api_key = "runtime-api-key-canary".to_string();
        runtime.webhook_token = "runtime-webhook-token-canary".to_string();

        let debug = format!("{:?}", runtime);

        assert!(!debug.contains("runtime-api-key-canary"));
        assert!(!debug.contains("runtime-webhook-token-canary"));

        // 包一层供货商条目后也不能漏。
        let entry = SupplierEntryRuntime {
            id: "kiroapp".to_owned(),
            name: "kiroapp.cc".to_owned(),
            kind: SupplierKind::KiroApp,
            enabled: true,
            settings: runtime,
        };
        let debug = format!("{entry:?}");
        assert!(!debug.contains("runtime-api-key-canary"));
        assert!(!debug.contains("runtime-webhook-token-canary"));

        // 对外视图同样只报「是否已配置」。
        let encoded = serde_json::to_string(&SupplierEntryView::from(&entry)).unwrap();
        assert!(!encoded.contains("runtime-api-key-canary"));
        assert!(!encoded.contains("runtime-webhook-token-canary"));
        assert!(encoded.contains("\"apiKeyConfigured\":true"));
        assert!(encoded.contains("\"kind\":\"kiro-app\""));
        assert!(encoded.contains("\"supportsWebhookRegistration\":false"));
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
