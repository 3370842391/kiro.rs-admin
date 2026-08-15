use std::collections::HashSet;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::kiro::region::validate_api_region;
use crate::model::config::{
    Config, KeySupplierCommonConfig, KeySupplierConfig, KeySupplierEntryConfig,
    KeySupplierPoolConfig, PurchaseRegionMode, SupplierImportOverrides, SupplierKind,
    SupplierRegion,
};

use super::capabilities::SupplierCapabilities;

const MAX_URL_CHARS: usize = 2_048;
const MAX_SECRET_CHARS: usize = 4_096;
const MAX_GROUPS: usize = 64;
const MAX_GROUP_NAME_CHARS: usize = 64;
const MAX_SOURCE_CHANNEL_CHARS: usize = 128;
const MAX_NICKNAME_PREFIX_CHARS: usize = 128;
const MAX_PURCHASE: u64 = 10_000;
/// 补货水位上限。比号池现实规模留足余量，同时挡住手滑输入的天文数字
/// ——水位配得比池子还大等于「永远都买」，那正是这道闸要防的事。
const MAX_TARGET_USABLE: u64 = 10_000;
/// 额度水位上限。上游满额是 10000（KIRO POWER），留一位余量应对更高档位。
/// 配得比满额还大等于「所有号都算不可用」，也就是每次到货都买。
const MAX_LOW_QUOTA_THRESHOLD: u64 = 100_000;
const MAX_RPM_LIMIT: u64 = 100_000;
/// 每账号最大并发上限。与 RPM 同量级：RPM 限速率，这个限瞬时并发。
const MAX_CONCURRENCY: u64 = 100_000;
const MAX_PRIORITY: u64 = u32::MAX as u64;
const MAX_SUPPLIER_ID_CHARS: usize = 64;
const MAX_SUPPLIER_NAME_CHARS: usize = 128;
/// 单实例能挂的供货商上限。够用又不至于让 webhook token 反查退化成长列表扫描。
pub const MAX_SUPPLIERS: usize = 32;

/// 公共预设与单家覆盖合并后的、已经校验过的凭据导入模板。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSupplierImportPreset {
    pub source_channel: String,
    pub nickname_label: String,
    pub rpm_limit: u32,
    pub max_concurrency: u32,
    pub priority: u32,
    pub groups: Vec<String>,
    pub auto_delete_forbidden: bool,
}

impl Default for ResolvedSupplierImportPreset {
    fn default() -> Self {
        Self {
            source_channel: "Webhook 自动采购".to_owned(),
            nickname_label: String::new(),
            rpm_limit: 10,
            max_concurrency: 0,
            priority: 0,
            groups: Vec::new(),
            auto_delete_forbidden: false,
        }
    }
}

impl ResolvedSupplierImportPreset {
    pub fn from_persisted(value: &KeySupplierCommonConfig) -> anyhow::Result<Self> {
        validate_number_range(u64::from(value.rpm_limit), "rpmLimit", 0, MAX_RPM_LIMIT)?;
        validate_number_range(
            u64::from(value.max_concurrency),
            "maxConcurrency",
            0,
            MAX_CONCURRENCY,
        )?;
        validate_number_range(u64::from(value.priority), "priority", 0, MAX_PRIORITY)?;
        Ok(Self {
            source_channel: normalize_text(
                &value.source_channel,
                "sourceChannel",
                MAX_SOURCE_CHANNEL_CHARS,
            )?,
            nickname_label: normalize_text(
                &value.nickname_label,
                "nicknameLabel",
                MAX_NICKNAME_PREFIX_CHARS,
            )?,
            rpm_limit: value.rpm_limit,
            max_concurrency: value.max_concurrency,
            priority: value.priority,
            groups: normalize_groups(value.groups.clone())?,
            auto_delete_forbidden: value.auto_delete_forbidden,
        })
    }

    pub fn resolve(&self, overrides: &SupplierImportOverrides) -> anyhow::Result<Self> {
        let rpm_limit = overrides.rpm_limit.unwrap_or(self.rpm_limit);
        let max_concurrency = overrides.max_concurrency.unwrap_or(self.max_concurrency);
        let priority = overrides.priority.unwrap_or(self.priority);
        validate_number_range(u64::from(rpm_limit), "rpmLimit", 0, MAX_RPM_LIMIT)?;
        validate_number_range(u64::from(max_concurrency), "maxConcurrency", 0, MAX_CONCURRENCY)?;
        validate_number_range(u64::from(priority), "priority", 0, MAX_PRIORITY)?;
        Ok(Self {
            source_channel: normalize_text(
                overrides
                    .source_channel
                    .as_deref()
                    .unwrap_or(&self.source_channel),
                "sourceChannel",
                MAX_SOURCE_CHANNEL_CHARS,
            )?,
            nickname_label: normalize_text(
                overrides
                    .nickname_label
                    .as_deref()
                    .unwrap_or(&self.nickname_label),
                "nicknameLabel",
                MAX_NICKNAME_PREFIX_CHARS,
            )?,
            rpm_limit,
            max_concurrency,
            priority,
            groups: normalize_groups(
                overrides
                    .groups
                    .clone()
                    .unwrap_or_else(|| self.groups.clone()),
            )?,
            auto_delete_forbidden: overrides
                .auto_delete_forbidden
                .unwrap_or(self.auto_delete_forbidden),
        })
    }

    fn materialize(&self, settings: &mut KeySupplierConfig) {
        settings.source_channel = self.source_channel.clone();
        settings.nickname_prefix = self.nickname_label.clone();
        settings.rpm_limit = self.rpm_limit;
        settings.max_concurrency = self.max_concurrency;
        settings.priority = self.priority;
        settings.groups = self.groups.clone();
        settings.auto_delete_forbidden = self.auto_delete_forbidden;
    }

    fn materialize_update(&self, settings: &mut SupplierConfigUpdate) {
        settings.source_channel = self.source_channel.clone();
        settings.nickname_prefix = self.nickname_label.clone();
        settings.rpm_limit = u64::from(self.rpm_limit);
        settings.max_concurrency = u64::from(self.max_concurrency);
        settings.priority = u64::from(self.priority);
        settings.groups = self.groups.clone();
        settings.auto_delete_forbidden = self.auto_delete_forbidden;
    }

    pub fn materialize_runtime(&self, settings: &mut SupplierRuntimeConfig) {
        settings.source_channel = self.source_channel.clone();
        settings.nickname_prefix = self.nickname_label.clone();
        settings.rpm_limit = self.rpm_limit;
        settings.max_concurrency = self.max_concurrency;
        settings.priority = self.priority;
        settings.groups = self.groups.clone();
        settings.auto_delete_forbidden = self.auto_delete_forbidden;
    }
}

impl From<&ResolvedSupplierImportPreset> for KeySupplierCommonConfig {
    fn from(value: &ResolvedSupplierImportPreset) -> Self {
        Self {
            source_channel: value.source_channel.clone(),
            nickname_label: value.nickname_label.clone(),
            rpm_limit: value.rpm_limit,
            max_concurrency: value.max_concurrency,
            priority: value.priority,
            groups: value.groups.clone(),
            auto_delete_forbidden: value.auto_delete_forbidden,
        }
    }
}

impl SupplierImportOverrides {
    pub(crate) fn from_legacy(settings: &KeySupplierConfig) -> Self {
        Self {
            source_channel: Some(settings.source_channel.clone()),
            nickname_label: Some(settings.nickname_prefix.clone()),
            rpm_limit: Some(settings.rpm_limit),
            max_concurrency: Some(settings.max_concurrency),
            priority: Some(settings.priority),
            groups: Some(settings.groups.clone()),
            auto_delete_forbidden: Some(settings.auto_delete_forbidden),
        }
    }

    fn from_legacy_against(
        settings: &KeySupplierConfig,
        common: &ResolvedSupplierImportPreset,
    ) -> Self {
        Self {
            source_channel: (settings.source_channel != common.source_channel)
                .then(|| settings.source_channel.clone()),
            nickname_label: (settings.nickname_prefix != common.nickname_label)
                .then(|| settings.nickname_prefix.clone()),
            rpm_limit: (settings.rpm_limit != common.rpm_limit).then_some(settings.rpm_limit),
            max_concurrency: (settings.max_concurrency != common.max_concurrency)
                .then_some(settings.max_concurrency),
            priority: (settings.priority != common.priority).then_some(settings.priority),
            groups: (settings.groups != common.groups).then(|| settings.groups.clone()),
            auto_delete_forbidden: (settings.auto_delete_forbidden != common.auto_delete_forbidden)
                .then_some(settings.auto_delete_forbidden),
        }
    }

    fn from_legacy_update(settings: &SupplierConfigUpdate) -> anyhow::Result<Self> {
        validate_number_range(settings.rpm_limit, "rpmLimit", 0, MAX_RPM_LIMIT)?;
        validate_number_range(settings.max_concurrency, "maxConcurrency", 0, MAX_CONCURRENCY)?;
        validate_number_range(settings.priority, "priority", 0, MAX_PRIORITY)?;
        Ok(Self {
            source_channel: Some(settings.source_channel.clone()),
            nickname_label: Some(settings.nickname_prefix.clone()),
            rpm_limit: Some(settings.rpm_limit as u32),
            max_concurrency: Some(settings.max_concurrency as u32),
            priority: Some(settings.priority as u32),
            groups: Some(settings.groups.clone()),
            auto_delete_forbidden: Some(settings.auto_delete_forbidden),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplierCommonConfigView {
    pub source_channel: String,
    pub nickname_label: String,
    pub rpm_limit: u32,
    pub max_concurrency: u32,
    pub priority: u32,
    pub groups: Vec<String>,
    pub auto_delete_forbidden: bool,
}

impl From<&ResolvedSupplierImportPreset> for SupplierCommonConfigView {
    fn from(value: &ResolvedSupplierImportPreset) -> Self {
        Self {
            source_channel: value.source_channel.clone(),
            nickname_label: value.nickname_label.clone(),
            rpm_limit: value.rpm_limit,
            max_concurrency: value.max_concurrency,
            priority: value.priority,
            groups: value.groups.clone(),
            auto_delete_forbidden: value.auto_delete_forbidden,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplierCommonConfigUpdate {
    #[serde(default)]
    pub source_channel: String,
    #[serde(default)]
    pub nickname_label: String,
    #[serde(default)]
    pub rpm_limit: u64,
    #[serde(default)]
    pub max_concurrency: u64,
    #[serde(default)]
    pub priority: u64,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub auto_delete_forbidden: bool,
}

impl ResolvedSupplierImportPreset {
    pub fn normalize_update(update: SupplierCommonConfigUpdate) -> anyhow::Result<Self> {
        validate_number_range(update.rpm_limit, "rpmLimit", 0, MAX_RPM_LIMIT)?;
        validate_number_range(update.max_concurrency, "maxConcurrency", 0, MAX_CONCURRENCY)?;
        validate_number_range(update.priority, "priority", 0, MAX_PRIORITY)?;
        Self::from_persisted(&KeySupplierCommonConfig {
            source_channel: update.source_channel,
            nickname_label: update.nickname_label,
            rpm_limit: update.rpm_limit as u32,
            max_concurrency: update.max_concurrency as u32,
            priority: update.priority as u32,
            groups: update.groups,
            auto_delete_forbidden: update.auto_delete_forbidden,
        })
    }
}

/// 一家供货商的运行期配置：身份（id/name/kind/enabled）+ 连接与导入预设。
#[derive(Clone, PartialEq)]
pub struct SupplierEntryRuntime {
    pub id: String,
    pub name: String,
    pub kind: SupplierKind,
    pub enabled: bool,
    /// 仅保存单家显式差异；`settings` 中的导入字段始终是公共值合并后的结果。
    pub import_overrides: SupplierImportOverrides,
    pub settings: SupplierRuntimeConfig,
}

impl std::fmt::Debug for SupplierEntryRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupplierEntryRuntime")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("enabled", &self.enabled)
            .field("import_overrides", &self.import_overrides)
            .field("settings", &self.settings)
            .finish()
    }
}

impl SupplierEntryRuntime {
    /// 从持久化条目读取，校验但不生成缺失的 webhook token。
    pub fn from_persisted(entry: &KeySupplierEntryConfig) -> anyhow::Result<Self> {
        Self::from_persisted_with_common(entry, &ResolvedSupplierImportPreset::default())
    }

    pub fn from_persisted_with_common(
        entry: &KeySupplierEntryConfig,
        common: &ResolvedSupplierImportPreset,
    ) -> anyhow::Result<Self> {
        let import_overrides = entry
            .import_overrides
            .clone()
            .unwrap_or_else(|| SupplierImportOverrides::from_legacy(&entry.settings));
        let resolved_import = common.resolve(&import_overrides)?;
        let mut settings = entry.settings.clone();
        resolved_import.materialize(&mut settings);
        Ok(Self {
            id: normalize_supplier_id(&entry.id)?,
            name: normalize_text(&entry.name, "name", MAX_SUPPLIER_NAME_CHARS)?,
            kind: entry.kind,
            enabled: entry.enabled,
            import_overrides,
            settings: normalize_persisted(entry.kind, &settings)?,
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
            import_overrides: Some(value.import_overrides.clone()),
            settings: KeySupplierConfig::from(&value.settings),
        }
    }
}

/// 供货商列表项的对外视图，secret 只报「是否已配置」。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplierEntryView {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub enabled: bool,
    pub supports_webhook_registration: bool,
    pub capabilities: SupplierCapabilities,
    pub import_overrides: SupplierImportOverrides,
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
            capabilities: SupplierCapabilities::for_kind(value.kind),
            import_overrides: value.import_overrides.clone(),
            settings: SupplierConfigView::from(&value.settings),
        }
    }
}

/// 新增/修改一家供货商的入参。`id` 仅新增时使用，修改时以路径参数为准。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
    /// 缺失表示旧管理端请求：把旧扁平导入字段视为显式覆盖，避免升级时静默改行为。
    #[serde(default)]
    pub import_overrides: Option<SupplierImportOverrides>,
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

// 不派生 `Eq`：`max_unit_price` 是 f64。金额本来就不该参与等价判定。
#[derive(Clone, PartialEq)]
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
    pub purchase_region_mode: PurchaseRegionMode,
    pub purchase_region: Option<SupplierRegion>,
    pub credential_api_region_fallback: String,
    pub rpm_limit: u32,
    pub max_concurrency: u32,
    pub priority: u32,
    pub groups: Vec<String>,
    pub source_channel: String,
    pub nickname_prefix: String,
    /// 逐家水位闸开关。全局号池启用时它整个让位。
    pub restock_only_when_exhausted: bool,
    /// 该供货商名下要常备的可用号数（**目标存量**，不是低水位）。
    /// 到货时按 `target_usable - 当前可用` 的缺口补齐。0 = 失效保护，不买。
    pub target_usable: u32,
    /// 剩余额度 <= 这个数就不算「可用」。0 = 不看额度。
    pub low_quota_threshold: u32,
    /// 单价上限。0 = 不限。单位是**这家自己的计价单位**，不与别家可比。
    pub max_unit_price: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
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
    pub purchase_region_mode: PurchaseRegionMode,
    pub purchase_region: Option<SupplierRegion>,
    pub credential_api_region_fallback: String,
    pub rpm_limit: u32,
    pub max_concurrency: u32,
    pub priority: u32,
    pub groups: Vec<String>,
    pub source_channel: String,
    pub nickname_prefix: String,
    pub restock_only_when_exhausted: bool,
    pub target_usable: u32,
    pub low_quota_threshold: u32,
    pub max_unit_price: f64,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
    #[serde(default)]
    pub purchase_region_mode: Option<PurchaseRegionMode>,
    #[serde(default)]
    pub purchase_region: Option<SupplierRegion>,
    #[serde(default)]
    pub credential_api_region_fallback: Option<String>,
    pub rpm_limit: u64,
    #[serde(default)]
    pub max_concurrency: u64,
    pub priority: u64,
    #[serde(default)]
    pub groups: Vec<String>,
    pub source_channel: String,
    pub nickname_prefix: String,
    /// 旧前端不发这两个字段，`default` 保持历史行为（每条到货都买）。
    #[serde(default)]
    pub restock_only_when_exhausted: bool,
    /// 目标存量。alias 收下旧前端发的 `restockUsableThreshold`——否则会静默变成 0，
    /// 而 0 是「不买」，用户只会看到采购全停而不知道是字段名换了。
    #[serde(default, alias = "restockUsableThreshold")]
    pub target_usable: u64,
    /// 单价上限，0 = 不限。旧前端不发，`default` 即不限（保持历史行为）。
    #[serde(default)]
    pub max_unit_price: f64,
    #[serde(default)]
    pub low_quota_threshold: u64,
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
            .field("purchase_region_mode", &self.purchase_region_mode)
            .field("purchase_region", &self.purchase_region)
            .field(
                "credential_api_region_fallback",
                &self.credential_api_region_fallback,
            )
            .field("rpm_limit", &self.rpm_limit)
            .field("max_concurrency", &self.max_concurrency)
            .field("priority", &self.priority)
            .field("groups", &self.groups)
            .field("source_channel", &self.source_channel)
            .field("nickname_prefix", &self.nickname_prefix)
            .field(
                "restock_only_when_exhausted",
                &self.restock_only_when_exhausted,
            )
            .field("target_usable", &self.target_usable)
            .field("low_quota_threshold", &self.low_quota_threshold)
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
            .field("purchase_region_mode", &self.purchase_region_mode)
            .field("purchase_region", &self.purchase_region)
            .field(
                "credential_api_region_fallback",
                &self.credential_api_region_fallback,
            )
            .field("rpm_limit", &self.rpm_limit)
            .field("max_concurrency", &self.max_concurrency)
            .field("priority", &self.priority)
            .field("groups", &self.groups)
            .field("source_channel", &self.source_channel)
            .field("nickname_prefix", &self.nickname_prefix)
            .field(
                "restock_only_when_exhausted",
                &self.restock_only_when_exhausted,
            )
            .field("target_usable", &self.target_usable)
            .field("low_quota_threshold", &self.low_quota_threshold)
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
        Self::normalize(SupplierKind::KiroRs, existing, update, false)
    }

    fn normalize(
        kind: SupplierKind,
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
        validate_number_range(update.max_concurrency, "maxConcurrency", 0, MAX_CONCURRENCY)?;
        validate_number_range(update.priority, "priority", 0, MAX_PRIORITY)?;
        validate_number_range(update.target_usable, "targetUsable", 0, MAX_TARGET_USABLE)?;
        // 单价上限必须是有限的非负数。NaN 参与任何比较都是 false，会让这道闸静默失效；
        // 负数则等于「永不采购」但看起来像配了个价。两者都当配置错误挡在门口。
        if !update.max_unit_price.is_finite() || update.max_unit_price < 0.0 {
            anyhow::bail!("maxUnitPrice 必须是 0 或正数（0 = 不限价）");
        }
        validate_number_range(
            update.low_quota_threshold,
            "lowQuotaThreshold",
            0,
            MAX_LOW_QUOTA_THRESHOLD,
        )?;

        let base_url = normalize_http_origin(&update.base_url, "baseUrl")?;
        let public_base_url = normalize_http_origin(&update.public_base_url, "publicBaseUrl")?;
        let fallback_input = update
            .credential_api_region_fallback
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&update.api_region);
        let credential_api_region_fallback = validate_api_region(fallback_input)?.to_string();
        let api_region = credential_api_region_fallback.clone();
        let purchase_region_was_defaulted = update.purchase_region_mode.is_none();
        let purchase_region_mode = update
            .purchase_region_mode
            .unwrap_or_else(|| default_purchase_region_mode(kind));
        let purchase_region = update.purchase_region.or_else(|| {
            (purchase_region_was_defaulted
                && kind == SupplierKind::KiroCeo
                && purchase_region_mode == PurchaseRegionMode::Fixed)
                .then_some(SupplierRegion::Us)
        });
        let capabilities = SupplierCapabilities::for_kind(kind);
        if !capabilities.supports_region_mode(purchase_region_mode) {
            anyhow::bail!(
                "purchaseRegionMode={:?} 不受供货商协议 {} 支持",
                purchase_region_mode,
                kind
            );
        }
        if purchase_region_mode == PurchaseRegionMode::Fixed && purchase_region.is_none() {
            anyhow::bail!("purchaseRegionMode=fixed 时必须指定 purchaseRegion");
        }
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
            purchase_region_mode,
            purchase_region,
            credential_api_region_fallback,
            rpm_limit: update.rpm_limit as u32,
            max_concurrency: update.max_concurrency as u32,
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
            restock_only_when_exhausted: update.restock_only_when_exhausted,
            target_usable: update.target_usable as u32,
            low_quota_threshold: update.low_quota_threshold as u32,
            max_unit_price: update.max_unit_price,
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
            purchase_region_mode: value.purchase_region_mode,
            purchase_region: value.purchase_region,
            credential_api_region_fallback: value.credential_api_region_fallback.clone(),
            rpm_limit: value.rpm_limit,
            max_concurrency: value.max_concurrency,
            priority: value.priority,
            groups: value.groups.clone(),
            source_channel: value.source_channel.clone(),
            nickname_prefix: value.nickname_prefix.clone(),
            restock_only_when_exhausted: value.restock_only_when_exhausted,
            target_usable: value.target_usable,
            low_quota_threshold: value.low_quota_threshold,
            max_unit_price: value.max_unit_price,
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
            purchase_region_mode: value.purchase_region_mode,
            purchase_region: value.purchase_region,
            credential_api_region_fallback: value.credential_api_region_fallback.clone(),
            rpm_limit: value.rpm_limit,
            max_concurrency: value.max_concurrency,
            priority: value.priority,
            groups: value.groups.clone(),
            source_channel: value.source_channel.clone(),
            nickname_prefix: value.nickname_prefix.clone(),
            restock_only_when_exhausted: value.restock_only_when_exhausted,
            target_usable: value.target_usable,
            low_quota_threshold: value.low_quota_threshold,
            max_unit_price: value.max_unit_price,
        }
    }
}

/// 全局号池的目标存量上限。与 `MAX_PURCHASE` 同量级：比号池现实规模留足余量，
/// 同时挡住手滑输入的天文数字。
pub const MAX_POOL_TARGET: u64 = 10_000;

/// 全局号池的运行期配置。校验过的值才进得来。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PoolRuntimeConfig {
    pub enabled: bool,
    pub target_count: u32,
    pub low_quota_threshold: u32,
}

/// 对外视图。没有 secret，因此与入参字段完全一致，不需要「留空不覆盖」那套处理。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolConfigView {
    pub enabled: bool,
    pub target_count: u32,
    pub low_quota_threshold: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolConfigUpdate {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub target_count: u64,
    #[serde(default)]
    pub low_quota_threshold: u64,
}

impl PoolRuntimeConfig {
    /// 校验并规范化一份号池配置。
    ///
    /// `enabled` 为假时**跳过数值校验**：关闭状态下的脏数据不该阻塞保存或启动，
    /// 而它本来就不参与任何判定。开启时才要求 `targetCount` 落在合法区间——
    /// 这也是「想启用就必须显式填一个数量」的强制点。
    pub fn normalize(update: PoolConfigUpdate) -> anyhow::Result<Self> {
        validate_number_range(
            update.low_quota_threshold,
            "poolLowQuotaThreshold",
            0,
            MAX_LOW_QUOTA_THRESHOLD,
        )?;
        if update.enabled {
            validate_number_range(update.target_count, "poolTargetCount", 1, MAX_POOL_TARGET)?;
        } else {
            validate_number_range(update.target_count, "poolTargetCount", 0, MAX_POOL_TARGET)?;
        }
        Ok(Self {
            enabled: update.enabled,
            target_count: update.target_count as u32,
            low_quota_threshold: update.low_quota_threshold as u32,
        })
    }

    /// 从持久化结构读取。校验失败返回 `Err`，由调用方决定失效方向。
    ///
    /// 调用方（启动装配）不应把校验失败当成「关闭该功能」——那等于退回不受限的
    /// 逐家采购模式偷偷花钱。正确做法见 `poisoned()`。
    pub fn from_persisted(config: &KeySupplierPoolConfig) -> anyhow::Result<Self> {
        Self::normalize(PoolConfigUpdate {
            enabled: config.enabled,
            target_count: u64::from(config.target_count),
            low_quota_threshold: u64::from(config.low_quota_threshold),
        })
    }

    /// 校验失败时装配的「中毒」配置：启用但目标存量为 0。
    ///
    /// 这与直觉相反——直觉是校验失败就关掉这个功能。但关掉意味着退回逐家独立采购、
    /// 不受任何限制地花钱，而用户配这个功能的意图明显是要限制采购。配错时最坏的
    /// 结果应该是不买，不是无限制买。目标存量 0 会让每次触发都命中
    /// 「目标存量不可用」跳过。
    pub fn poisoned() -> Self {
        Self {
            enabled: true,
            target_count: 0,
            low_quota_threshold: 0,
        }
    }
}

impl From<&PoolRuntimeConfig> for PoolConfigView {
    fn from(value: &PoolRuntimeConfig) -> Self {
        Self {
            enabled: value.enabled,
            target_count: value.target_count,
            low_quota_threshold: value.low_quota_threshold,
        }
    }
}

impl From<&PoolRuntimeConfig> for KeySupplierPoolConfig {
    fn from(value: &PoolRuntimeConfig) -> Self {
        Self {
            enabled: value.enabled,
            target_count: value.target_count,
            low_quota_threshold: value.low_quota_threshold,
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
pub fn load_suppliers_with_common(
    config: &Config,
) -> anyhow::Result<(
    Vec<SupplierEntryRuntime>,
    ResolvedSupplierImportPreset,
    bool,
)> {
    let configured_common =
        ResolvedSupplierImportPreset::from_persisted(&config.key_supplier_common)?;
    if config.key_suppliers.is_empty() {
        if !KeySupplierEntryConfig::legacy_is_configured(&config.key_supplier) {
            return Ok((Vec::new(), configured_common, false));
        }
        let common = if config.key_supplier_common == KeySupplierCommonConfig::default() {
            ResolvedSupplierImportPreset {
                source_channel: config.key_supplier.source_channel.clone(),
                nickname_label: config.key_supplier.nickname_prefix.clone(),
                rpm_limit: config.key_supplier.rpm_limit,
                max_concurrency: config.key_supplier.max_concurrency,
                priority: config.key_supplier.priority,
                groups: config.key_supplier.groups.clone(),
                auto_delete_forbidden: config.key_supplier.auto_delete_forbidden,
            }
        } else {
            configured_common
        };
        let mut migrated = KeySupplierEntryConfig::from_legacy(config.key_supplier.clone());
        migrated.import_overrides = Some(SupplierImportOverrides::from_legacy_against(
            &migrated.settings,
            &common,
        ));
        return Ok((
            vec![SupplierEntryRuntime::from_persisted_with_common(
                &migrated, &common,
            )?],
            common,
            true,
        ));
    }

    let all_entries_are_legacy = config
        .key_suppliers
        .iter()
        .all(|entry| entry.import_overrides.is_none());
    let common = if all_entries_are_legacy
        && config.key_supplier_common == KeySupplierCommonConfig::default()
    {
        shared_legacy_common(&config.key_suppliers, &configured_common)
    } else {
        configured_common
    };

    let mut seen = HashSet::new();
    let mut entries = Vec::with_capacity(config.key_suppliers.len());
    let mut migrated = false;
    for entry in &config.key_suppliers {
        migrated |= entry.import_overrides.is_none();
        let migrated_entry;
        let entry = if entry.import_overrides.is_none() {
            migrated_entry = KeySupplierEntryConfig {
                import_overrides: Some(SupplierImportOverrides::from_legacy_against(
                    &entry.settings,
                    &common,
                )),
                ..entry.clone()
            };
            &migrated_entry
        } else {
            entry
        };
        let runtime = SupplierEntryRuntime::from_persisted_with_common(entry, &common)?;
        if !seen.insert(runtime.id.clone()) {
            anyhow::bail!("供货商 id 重复: {}", runtime.id);
        }
        entries.push(runtime);
    }
    Ok((entries, common, migrated))
}

pub fn load_suppliers(config: &Config) -> anyhow::Result<(Vec<SupplierEntryRuntime>, bool)> {
    let (entries, _, migrated) = load_suppliers_with_common(config)?;
    Ok((entries, migrated))
}

fn shared_legacy_common(
    entries: &[KeySupplierEntryConfig],
    fallback: &ResolvedSupplierImportPreset,
) -> ResolvedSupplierImportPreset {
    let Some(first) = entries.first() else {
        return fallback.clone();
    };
    let all = |matches: &dyn Fn(&KeySupplierConfig) -> bool| {
        entries.iter().all(|entry| matches(&entry.settings))
    };
    ResolvedSupplierImportPreset {
        source_channel: all(&|settings| settings.source_channel == first.settings.source_channel)
            .then(|| first.settings.source_channel.clone())
            .unwrap_or_else(|| fallback.source_channel.clone()),
        nickname_label: all(&|settings| settings.nickname_prefix == first.settings.nickname_prefix)
            .then(|| first.settings.nickname_prefix.clone())
            .unwrap_or_else(|| fallback.nickname_label.clone()),
        rpm_limit: all(&|settings| settings.rpm_limit == first.settings.rpm_limit)
            .then_some(first.settings.rpm_limit)
            .unwrap_or(fallback.rpm_limit),
        max_concurrency: all(&|settings| {
            settings.max_concurrency == first.settings.max_concurrency
        })
        .then_some(first.settings.max_concurrency)
        .unwrap_or(fallback.max_concurrency),
        priority: all(&|settings| settings.priority == first.settings.priority)
            .then_some(first.settings.priority)
            .unwrap_or(fallback.priority),
        groups: all(&|settings| settings.groups == first.settings.groups)
            .then(|| first.settings.groups.clone())
            .unwrap_or_else(|| fallback.groups.clone()),
        auto_delete_forbidden: all(&|settings| {
            settings.auto_delete_forbidden == first.settings.auto_delete_forbidden
        })
        .then_some(first.settings.auto_delete_forbidden)
        .unwrap_or(fallback.auto_delete_forbidden),
    }
}

/// 把内存里的供货商列表写回 `Config`（调用方负责 `save()`）。
///
/// 同时把第一个 `kiro-rs` 供货商镜像回历史 `keySupplier` 字段：镜像本身不参与读取
/// （启动时只在 `keySuppliers` 为空才看它），存在的意义是回滚到旧版本后主供货商仍可用。
///
/// **没有 `kiro-rs` 条目时必须把镜像清空。** 否则删掉最后一家供货商后 `keySuppliers`
/// 变成空数组，而残留的镜像仍然「已配置」——下次启动 `load_suppliers` 会把它当历史配置
/// 迁移回来，复活一家带着原 `autoPurchase` 的供货商，然后开始花钱。
pub fn store_suppliers(config: &mut Config, entries: &[SupplierEntryRuntime]) {
    config.key_suppliers = entries.iter().map(KeySupplierEntryConfig::from).collect();
    match entries
        .iter()
        .find(|entry| entry.enabled && entry.kind == SupplierKind::KiroRs)
        .or_else(|| {
            entries
                .iter()
                .find(|entry| entry.kind == SupplierKind::KiroRs)
        }) {
        Some(primary) => config.key_supplier = KeySupplierConfig::from(&primary.settings),
        None => config.key_supplier = KeySupplierConfig::default(),
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
        Self::normalize_update_with_common(
            id,
            update,
            existing,
            &ResolvedSupplierImportPreset::default(),
        )
    }

    pub fn normalize_update_with_common(
        id: Option<&str>,
        mut update: SupplierEntryUpdate,
        existing: Option<&SupplierEntryRuntime>,
        common: &ResolvedSupplierImportPreset,
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
        let import_overrides = match update.import_overrides.take() {
            Some(overrides) => overrides,
            None => SupplierImportOverrides::from_legacy_update(&update.settings)?,
        };
        let resolved_import = common.resolve(&import_overrides)?;
        resolved_import.materialize_update(&mut update.settings);
        let mut settings = SupplierRuntimeConfig::normalize(
            update.kind,
            existing.map(|entry| &entry.settings),
            update.settings,
            false,
        )?;
        if settings.webhook_token.is_empty() {
            settings.webhook_token = generate_webhook_token();
        }
        Ok(Self {
            id,
            name,
            kind: update.kind,
            enabled: update.enabled,
            import_overrides,
            settings,
        })
    }
}

pub fn is_valid_webhook_token(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn default_purchase_region_mode(kind: SupplierKind) -> PurchaseRegionMode {
    match kind {
        SupplierKind::KiroCeo => PurchaseRegionMode::Fixed,
        SupplierKind::KiroAppIo => PurchaseRegionMode::Batch,
        // Drop 默认给 BestAvailable：不指定区时先打对方默认区（美区），
        // 明确判定缺货再自动改打欧区。这正是「美区常被抢空、欧区还有货」
        // 那个场景需要的默认行为——老配置升级后不用手工改也能生效。
        SupplierKind::KiroDrop => PurchaseRegionMode::BestAvailable,
        SupplierKind::KiroRs | SupplierKind::KiroApp => PurchaseRegionMode::Omit,
    }
}

fn normalize_persisted(
    kind: SupplierKind,
    value: &KeySupplierConfig,
) -> anyhow::Result<SupplierRuntimeConfig> {
    let capabilities = SupplierCapabilities::for_kind(kind);
    let persisted_mode = if capabilities.supports_region_mode(value.purchase_region_mode) {
        Some(value.purchase_region_mode)
    } else {
        None
    };
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
        purchase_region_mode: persisted_mode,
        purchase_region: value.purchase_region,
        credential_api_region_fallback: if value.credential_api_region_fallback.trim().is_empty() {
            None
        } else {
            Some(value.credential_api_region_fallback.clone())
        },
        rpm_limit: u64::from(value.rpm_limit),
        max_concurrency: u64::from(value.max_concurrency),
        priority: u64::from(value.priority),
        groups: value.groups.clone(),
        source_channel: value.source_channel.clone(),
        nickname_prefix: value.nickname_prefix.clone(),
        restock_only_when_exhausted: value.restock_only_when_exhausted,
        target_usable: u64::from(value.target_usable),
        low_quota_threshold: u64::from(value.low_quota_threshold),
        max_unit_price: value.max_unit_price,
    };
    SupplierRuntimeConfig::normalize(kind, None, update, false)
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
            purchase_region_mode: None,
            purchase_region: None,
            credential_api_region_fallback: None,
            rpm_limit: 100,
            max_concurrency: 4,
            priority: 10,
            groups: vec![
                " production ".to_string(),
                "production".to_string(),
                " backup ".to_string(),
            ],
            source_channel: " Webhook 自动采购 ".to_string(),
            nickname_prefix: " 自动采购 ".to_string(),
            restock_only_when_exhausted: false,
            target_usable: 0,
            low_quota_threshold: 0,
            max_unit_price: 0.0,
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
    fn purchase_region_mode_is_validated_against_supplier_capabilities() {
        let mut fixed_us = valid_update();
        fixed_us.purchase_region_mode = Some(PurchaseRegionMode::Fixed);
        fixed_us.purchase_region = Some(SupplierRegion::Us);
        let ceo = SupplierEntryRuntime::normalize_update(
            None,
            SupplierEntryUpdate {
                id: Some("ceo".to_owned()),
                name: "CEO".to_owned(),
                kind: SupplierKind::KiroCeo,
                enabled: true,
                import_overrides: None,
                settings: fixed_us,
            },
            None,
        )
        .unwrap();
        assert_eq!(ceo.settings.purchase_region_mode, PurchaseRegionMode::Fixed);
        assert_eq!(ceo.settings.purchase_region, Some(SupplierRegion::Us));

        let mut missing_region = valid_update();
        missing_region.purchase_region_mode = Some(PurchaseRegionMode::Fixed);
        assert!(
            SupplierEntryRuntime::normalize_update(
                None,
                SupplierEntryUpdate {
                    id: Some("ceo-invalid".to_owned()),
                    name: "CEO".to_owned(),
                    kind: SupplierKind::KiroCeo,
                    enabled: true,
                    import_overrides: None,
                    settings: missing_region,
                },
                None,
            )
            .is_err()
        );

        // 换用 kiro-app 当反例：它的协议里压根没有区域字段（仅 Omit）。
        // 原先这里用 Kiro Drop，但 Drop 的购买接口是接受 region 的，
        // 已改为支持 Fixed / Webhook / BestAvailable，不再是有效反例。
        let mut unsupported = valid_update();
        unsupported.purchase_region_mode = Some(PurchaseRegionMode::Fixed);
        unsupported.purchase_region = Some(SupplierRegion::Us);
        assert!(
            SupplierEntryRuntime::normalize_update(
                None,
                SupplierEntryUpdate {
                    id: Some("app".to_owned()),
                    name: "App".to_owned(),
                    kind: SupplierKind::KiroApp,
                    enabled: true,
                    import_overrides: None,
                    settings: unsupported,
                },
                None,
            )
            .is_err()
        );

        // Drop 现在应当**接受** Fixed + 指定区（运维想固定只买某个区时用）。
        let mut drop_fixed = valid_update();
        drop_fixed.purchase_region_mode = Some(PurchaseRegionMode::Fixed);
        drop_fixed.purchase_region = Some(SupplierRegion::Eu);
        assert!(
            SupplierEntryRuntime::normalize_update(
                None,
                SupplierEntryUpdate {
                    id: Some("drop".to_owned()),
                    name: "Drop".to_owned(),
                    kind: SupplierKind::KiroDrop,
                    enabled: true,
                    import_overrides: None,
                    settings: drop_fixed,
                },
                None,
            )
            .is_ok(),
            "Drop 支持固定区采购，配置不该被拒"
        );
    }

    #[test]
    fn legacy_ceo_defaults_to_fixed_us_and_keeps_credential_region_fallback() {
        let persisted = KeySupplierEntryConfig {
            id: "ceo".to_owned(),
            name: "CEO".to_owned(),
            kind: SupplierKind::KiroCeo,
            enabled: true,
            import_overrides: None,
            settings: KeySupplierConfig {
                api_region: "eu-central-1".to_owned(),
                ..Default::default()
            },
        };

        let runtime = SupplierEntryRuntime::from_persisted(&persisted).unwrap();
        assert_eq!(
            runtime.settings.purchase_region_mode,
            PurchaseRegionMode::Fixed
        );
        assert_eq!(runtime.settings.purchase_region, Some(SupplierRegion::Us));
        assert_eq!(
            runtime.settings.credential_api_region_fallback,
            "eu-central-1"
        );
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
                import_overrides: None,
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
        for invalid in [
            "",
            "   ",
            "has space",
            "slash/es",
            "dots.",
            "中文",
            &"a".repeat(65),
        ] {
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

    fn pool_update(enabled: bool, target: u64, low_quota: u64) -> PoolConfigUpdate {
        PoolConfigUpdate {
            enabled,
            target_count: target,
            low_quota_threshold: low_quota,
        }
    }

    #[test]
    fn enabling_the_pool_requires_an_explicit_target_count() {
        // 想启用就必须显式填数量。默认 0 是「未配置」哨兵，不是某个业务默认值——
        // 放过 0 等于让人以为开了限制、实际上每次都跳过（或更糟，被当默认值去买）。
        assert!(PoolRuntimeConfig::normalize(pool_update(true, 0, 0)).is_err());
        assert!(PoolRuntimeConfig::normalize(pool_update(true, 1, 0)).is_ok());
        assert!(PoolRuntimeConfig::normalize(pool_update(true, MAX_POOL_TARGET, 0)).is_ok());
        assert!(PoolRuntimeConfig::normalize(pool_update(true, MAX_POOL_TARGET + 1, 0)).is_err());
    }

    #[test]
    fn disabled_pool_config_skips_numeric_validation() {
        // 关闭状态下的脏数据不该阻塞保存或启动——它本来就不参与任何判定。
        let disabled = PoolRuntimeConfig::normalize(pool_update(false, 0, 0)).unwrap();
        assert!(!disabled.enabled);
        assert_eq!(disabled.target_count, 0);

        // 但越界值仍然拒绝：那是手滑而不是「未配置」。
        assert!(PoolRuntimeConfig::normalize(pool_update(false, MAX_POOL_TARGET + 1, 0)).is_err());
    }

    #[test]
    fn pool_low_quota_threshold_is_range_checked() {
        assert!(PoolRuntimeConfig::normalize(pool_update(true, 3, 0)).is_ok());
        assert!(
            PoolRuntimeConfig::normalize(pool_update(true, 3, MAX_LOW_QUOTA_THRESHOLD)).is_ok()
        );
        assert!(
            PoolRuntimeConfig::normalize(pool_update(true, 3, MAX_LOW_QUOTA_THRESHOLD + 1))
                .is_err()
        );
    }

    #[test]
    fn poisoned_pool_config_fails_closed_instead_of_falling_back_to_unlimited_buying() {
        // 校验失败时不能「关掉这个功能」——那等于退回不受限的逐家采购继续花钱。
        // 中毒配置是启用但目标存量 0，使每次触发都跳过。
        let poisoned = PoolRuntimeConfig::poisoned();
        assert!(poisoned.enabled, "关掉就退回不受限采购了");
        assert_eq!(
            poisoned.target_count, 0,
            "0 会让每次触发都命中「目标存量不可用」"
        );
    }

    #[test]
    fn pool_config_round_trips_through_json_and_missing_block_means_disabled() {
        let runtime = PoolRuntimeConfig::normalize(pool_update(true, 7, 500)).unwrap();
        let persisted = KeySupplierPoolConfig::from(&runtime);

        let json = serde_json::to_string(&persisted).unwrap();
        let parsed: KeySupplierPoolConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, persisted);
        assert_eq!(PoolRuntimeConfig::from_persisted(&parsed).unwrap(), runtime);

        // camelCase 线格式写死：字段名一变，线上配置就读不回来。
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["enabled"], true);
        assert_eq!(value["targetCount"], 7);
        assert_eq!(value["lowQuotaThreshold"], 500);

        // 缺整块 = 未启用，使老 config.json 升级后行为不变。
        let empty: KeySupplierPoolConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, KeySupplierPoolConfig::default());
        assert!(!empty.enabled);
        assert!(!PoolRuntimeConfig::from_persisted(&empty).unwrap().enabled);
    }

    #[test]
    fn deleting_the_last_supplier_clears_the_legacy_mirror_so_it_cannot_resurrect() {
        // 复活路径：删光供货商 → `keySuppliers` 变空 → 启动时 `load_suppliers` 看到
        // legacy 镜像「已配置」就把它迁移回来，于是一家带着 autoPurchase 的供货商
        // 自己回来了，然后开始花钱。镜像必须跟着一起清掉。
        let mut config = Config::default();
        config.key_supplier.base_url = "https://legacy.example".to_string();
        config.key_supplier.api_key = "legacy-key".to_string();
        config.key_supplier.webhook_token = "a".repeat(64);
        config.key_supplier.auto_purchase = true;

        let (entries, migrated) = load_suppliers(&config).unwrap();
        assert!(migrated);
        assert_eq!(entries.len(), 1);

        // 迁移落盘：此时镜像和列表都在。
        store_suppliers(&mut config, &entries);
        assert!(!config.key_suppliers.is_empty());
        assert!(KeySupplierEntryConfig::legacy_is_configured(
            &config.key_supplier
        ));

        // 删掉唯一一家。
        store_suppliers(&mut config, &[]);
        assert!(config.key_suppliers.is_empty());
        assert!(
            !KeySupplierEntryConfig::legacy_is_configured(&config.key_supplier),
            "镜像没清空，重启会把删掉的供货商迁移回来"
        );

        // 模拟重启：不该再长出任何供货商。
        let (after_restart, migrated_again) = load_suppliers(&config).unwrap();
        assert!(after_restart.is_empty());
        assert!(!migrated_again);
    }

    #[test]
    fn deleting_the_last_kiro_rs_supplier_clears_the_mirror_but_keeps_the_others() {
        // 只剩非 kiro-rs 供货商时镜像也要清（镜像只服务 kiro-rs 的版本回滚），
        // 但列表本身不能被动到。
        let mut config = Config::default();
        config.key_supplier.base_url = "https://legacy.example".to_string();
        config.key_supplier.api_key = "legacy-key".to_string();

        let mut io = SupplierEntryRuntime {
            id: "io".to_string(),
            name: "kiroapp.io".to_string(),
            kind: SupplierKind::KiroAppIo,
            enabled: true,
            import_overrides: SupplierImportOverrides::default(),
            settings: SupplierRuntimeConfig::normalize_standalone(valid_update(), None).unwrap(),
        };
        io.settings.api_key = "km_secret".to_string();

        store_suppliers(&mut config, std::slice::from_ref(&io));

        assert_eq!(config.key_suppliers.len(), 1);
        assert_eq!(config.key_suppliers[0].id, "io");
        assert!(
            !KeySupplierEntryConfig::legacy_is_configured(&config.key_supplier),
            "没有 kiro-rs 条目时镜像必须清空"
        );

        // 列表非空，启动时根本不看镜像，读回来还是那一家。
        let (entries, migrated) = load_suppliers(&config).unwrap();
        assert!(!migrated);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, SupplierKind::KiroAppIo);
    }

    #[test]
    fn common_import_preset_is_shared_and_explicit_supplier_overrides_win() {
        let json = serde_json::json!({
            "keySupplierCommon": {
                "sourceChannel": "统一采购",
                "nicknameLabel": "生产",
                "rpmLimit": 23,
                "priority": 7,
                "groups": ["common-a", "common-b"],
                "autoDeleteForbidden": true
            },
            "keySuppliers": [
                {
                    "id": "ceo",
                    "name": "ceo",
                    "kind": "kiro-ceo",
                    "enabled": true,
                    "baseUrl": "https://ceo.example",
                    "apiKey": "ceo-secret",
                    "publicBaseUrl": "https://admin.example",
                    "webhookToken": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "minPurchase": 1,
                    "maxPurchase": 10,
                    "apiRegion": "us-east-1",
                    "purchaseRegionMode": "fixed",
                    "purchaseRegion": "us",
                    "importOverrides": {}
                },
                {
                    "id": "drop",
                    "name": "drop",
                    "kind": "kiro-drop",
                    "enabled": true,
                    "baseUrl": "https://drop.example",
                    "apiKey": "drop-secret",
                    "publicBaseUrl": "https://admin.example",
                    "webhookToken": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "minPurchase": 1,
                    "maxPurchase": 10,
                    "apiRegion": "us-east-1",
                    "importOverrides": {
                        "sourceChannel": "Drop 专用",
                        "nicknameLabel": "备用",
                        "rpmLimit": 5,
                        "groups": ["drop-only"]
                    }
                }
            ]
        });
        let config: Config = serde_json::from_value(json).unwrap();

        let (entries, migrated) = load_suppliers(&config).unwrap();

        assert!(!migrated);
        let ceo = entries.iter().find(|entry| entry.id == "ceo").unwrap();
        assert_eq!(ceo.settings.source_channel, "统一采购");
        assert_eq!(ceo.settings.nickname_prefix, "生产");
        assert_eq!(ceo.settings.rpm_limit, 23);
        assert_eq!(ceo.settings.priority, 7);
        assert_eq!(ceo.settings.groups, vec!["common-a", "common-b"]);
        assert!(ceo.settings.auto_delete_forbidden);

        let drop = entries.iter().find(|entry| entry.id == "drop").unwrap();
        assert_eq!(drop.settings.source_channel, "Drop 专用");
        assert_eq!(drop.settings.nickname_prefix, "备用");
        assert_eq!(drop.settings.rpm_limit, 5);
        assert_eq!(drop.settings.priority, 7);
        assert_eq!(drop.settings.groups, vec!["drop-only"]);
        assert!(drop.settings.auto_delete_forbidden);
    }

    #[test]
    fn legacy_flat_entries_promote_identical_import_values_to_common() {
        let json = serde_json::json!({
            "keySuppliers": [
                {
                    "id": "drop-a",
                    "name": "Drop A",
                    "kind": "kiro-drop",
                    "baseUrl": "https://drop-a.example",
                    "apiKey": "secret-a",
                    "sourceChannel": "统一采购",
                    "nicknamePrefix": "生产",
                    "rpmLimit": 23,
                    "priority": 7,
                    "groups": ["common"],
                    "autoDeleteForbidden": true
                },
                {
                    "id": "drop-b",
                    "name": "Drop B",
                    "kind": "kiro-drop",
                    "baseUrl": "https://drop-b.example",
                    "apiKey": "secret-b",
                    "sourceChannel": "统一采购",
                    "nicknamePrefix": "备用",
                    "rpmLimit": 23,
                    "priority": 7,
                    "groups": ["common"],
                    "autoDeleteForbidden": true
                }
            ]
        });
        let config: Config = serde_json::from_value(json).unwrap();

        let (entries, common, migrated) = load_suppliers_with_common(&config).unwrap();

        assert!(migrated);
        assert_eq!(common.source_channel, "统一采购");
        assert_eq!(common.nickname_label, "");
        assert_eq!(common.rpm_limit, 23);
        assert_eq!(common.priority, 7);
        assert_eq!(common.groups, vec!["common"]);
        assert!(common.auto_delete_forbidden);
        assert!(
            entries
                .iter()
                .all(|entry| entry.import_overrides.source_channel.is_none())
        );
        assert!(
            entries
                .iter()
                .all(|entry| entry.import_overrides.rpm_limit.is_none())
        );
        assert_eq!(
            entries[0].import_overrides.nickname_label.as_deref(),
            Some("生产")
        );
        assert_eq!(
            entries[1].import_overrides.nickname_label.as_deref(),
            Some("备用")
        );
    }

    #[test]
    fn storing_new_import_config_materializes_legacy_flat_fields_for_rollback() {
        let json = serde_json::json!({
            "keySupplierCommon": {
                "sourceChannel": "统一采购",
                "nicknameLabel": "生产",
                "rpmLimit": 23,
                "priority": 7,
                "groups": ["common"],
                "autoDeleteForbidden": true
            },
            "keySuppliers": [{
                "id": "ceo",
                "name": "ceo",
                "kind": "kiro-ceo",
                "enabled": true,
                "baseUrl": "https://ceo.example",
                "apiKey": "secret",
                "publicBaseUrl": "https://admin.example",
                "webhookToken": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "minPurchase": 1,
                "maxPurchase": 10,
                "apiRegion": "us-east-1",
                "purchaseRegionMode": "fixed",
                "purchaseRegion": "us",
                "importOverrides": { "priority": 9 }
            }]
        });
        let mut config: Config = serde_json::from_value(json).unwrap();
        let (entries, _) = load_suppliers(&config).unwrap();

        store_suppliers(&mut config, &entries);
        let encoded = serde_json::to_value(&config).unwrap();
        let ceo = &encoded["keySuppliers"][0];

        assert_eq!(ceo["sourceChannel"], "统一采购");
        assert_eq!(ceo["nicknamePrefix"], "生产");
        assert_eq!(ceo["rpmLimit"], 23);
        assert_eq!(ceo["priority"], 9);
        assert_eq!(ceo["groups"], serde_json::json!(["common"]));
        assert_eq!(ceo["autoDeleteForbidden"], true);
        assert_eq!(ceo["importOverrides"]["priority"], 9);
    }

    #[test]
    fn debug_does_not_expose_supplier_runtime_secrets() {
        let mut runtime =
            SupplierRuntimeConfig::normalize_standalone(valid_update(), None).unwrap();
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
            import_overrides: SupplierImportOverrides::default(),
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
