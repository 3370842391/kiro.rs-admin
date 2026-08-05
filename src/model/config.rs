use anyhow::Context;
use atomicwrites::{AllowOverwrite, AtomicFile};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

/// 工具兼容模式。
///
/// - `ClaudeCode`（默认）：把 Claude Code 内置工具（Write/Edit/Bash/Read/Glob/Grep/LS/WebSearch）
///   的工具名与入参双向适配为 Kiro 内置工具（fs_write/str_replace/... ），并替换为 Kiro 内置 schema。
/// - `Raw`：保留旧行为，直接透传客户端工具名/schema，用于排障。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCompatibilityMode {
    #[default]
    ClaudeCode,
    Raw,
}

/// 普通 429 的重试策略模式。
///
/// `Failover` 是本项目默认策略：普通 429 先用同一凭据切换 q/runtime 独立限流桶，
/// 备用端点仍失败时再在本次请求内换凭据，且不给凭据施加跨请求冷却。其它模式来自
/// Kiro-RS-Tool，用于按需切换为更激进或更保守的普通 429 冷却与重试节奏。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum RetryMode {
    #[default]
    Failover,
    Turbo,
    Fast,
    Balanced,
    Steady,
    Polite,
    Custom,
}

/// 上游端点路由预设。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointMode {
    #[default]
    Best,
    Manual,
}

impl std::fmt::Display for RetryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Failover => "failover",
            Self::Turbo => "turbo",
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Steady => "steady",
            Self::Polite => "polite",
            Self::Custom => "custom",
        };
        f.write_str(value)
    }
}

impl std::str::FromStr for RetryMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "failover" | "current" | "default" => Ok(Self::Failover),
            "turbo" => Ok(Self::Turbo),
            "fast" => Ok(Self::Fast),
            "balanced" => Ok(Self::Balanced),
            "steady" => Ok(Self::Steady),
            "polite" => Ok(Self::Polite),
            "custom" => Ok(Self::Custom),
            _ => anyhow::bail!("无效的重试模式: {}", value),
        }
    }
}

// 不派生 `Eq`：`max_unit_price` 是 f64。金额本来就不该参与等价判定。
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct KeySupplierConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub public_base_url: String,
    #[serde(default)]
    pub webhook_token: String,
    /// Webhook 签名密钥（kiroapp.cc 的 `X-Kiro-Signature` 用）。
    ///
    /// 留空表示不验签（历史号商协议不签名）。配上以后，未带正确
    /// `hex(HMAC-SHA256(secret, 原始请求体))` 的推送会被拒。
    #[serde(default)]
    pub webhook_secret: String,
    #[serde(default)]
    pub auto_purchase: bool,
    #[serde(default)]
    pub auto_delete_forbidden: bool,
    #[serde(default = "default_supplier_purchase")]
    pub min_purchase: u32,
    #[serde(default = "default_supplier_purchase")]
    pub max_purchase: u32,
    #[serde(default = "default_region")]
    pub api_region: String,
    /// 采购请求的区域选择策略。旧配置缺失时由协议类型在运行时补默认值。
    #[serde(default, skip_serializing_if = "purchase_region_mode_is_omit")]
    pub purchase_region_mode: PurchaseRegionMode,
    /// fixed 模式下要采购的区域。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purchase_region: Option<SupplierRegion>,
    /// 无法从采购响应、Webhook 或请求确定区域时，写入凭据的 API 区域兜底。
    /// 空值兼容旧配置，运行时回退读取 `apiRegion`。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub credential_api_region_fallback: String,
    #[serde(default = "default_supplier_rpm_limit")]
    pub rpm_limit: u32,
    #[serde(default)]
    pub priority: u32,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default = "default_supplier_source_channel")]
    pub source_channel: String,
    #[serde(default = "default_supplier_nickname_prefix")]
    pub nickname_prefix: String,
    /// 逐家水位闸的开关。开启后该供货商的到货通知按 `targetUsable` 的缺口补货。
    ///
    /// 供货商不断推到货通知时，不加这道闸就是每次到货都掏钱。
    ///
    /// 关闭（默认）保持历史行为：每条到货通知都尝试采购。手动采购不受此开关影响。
    /// 全局号池启用时这道闸整个让位——两套水位并存会交叉出第三种行为。
    #[serde(default)]
    pub restock_only_when_exhausted: bool,
    /// **目标存量**：该供货商名下要常备多少个可用号。
    ///
    /// 语义与全局号池的 `targetCount` 一致：到货通知来了就按 `目标 - 当前可用` 的缺口
    /// 补齐，补满就不再买。所以「每家常备 1 个」填 1，三家各填 1 就是全局 3 个。
    ///
    /// 0 = 配了开关没填数量，按失效保护不买（宁可少买）。
    /// 仅在 `restockOnlyWhenExhausted` 为真时生效。
    ///
    /// 旧字段名 `restockUsableThreshold` 是**低水位**语义（可用数 <= 它才买），
    /// 那套语义下填 1 会在买到 1 个后仍然满足 `1 <= 1` 而继续买，同一家连推三次就
    /// 买三次。alias 只为让老配置能读进来，读进来后按新语义解释。
    #[serde(default, alias = "restockUsableThreshold")]
    pub target_usable: u32,
    /// 单价上限：对方现在的单价高于这个数就不自动采购。0 = 不限。
    ///
    /// 单位是**这家自己的计价单位**（Drop 报 USD，kiroapp 系报积分，kiro.ceo 报分区价），
    /// 各家不通用，所以只和同一家的报价比较，绝不参与跨家算术。
    ///
    /// 配了上限但这家在下单前报不出价（kiro-rs 的 `/api/my/stock` 只有 `max`）时按
    /// 「宁可少买」跳过：把「不知道价」当成免费会让这道闸在最需要它的时候失效。
    #[serde(default)]
    pub max_unit_price: f64,
    /// 额度水位：剩余额度 <= 这个数就不算「可用」。0 = 不看额度，只认封号与 402。
    ///
    /// 为什么需要它：号没被封、也没触发 402，但剩余额度只有几百，对流量来说已经接近
    /// 废号。只等 402 意味着必须先把号跑干才补货，中间那段是服务空窗。
    ///
    /// 单位与上游 `usageLimit` 一致（KIRO POWER 满额 10000）。跨订阅档位混用时注意
    /// 这是绝对值，不是百分比。
    #[serde(default)]
    pub low_quota_threshold: u32,
}

impl std::fmt::Debug for KeySupplierConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeySupplierConfig")
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

impl Default for KeySupplierConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            public_base_url: String::new(),
            webhook_token: String::new(),
            webhook_secret: String::new(),
            auto_purchase: false,
            auto_delete_forbidden: false,
            min_purchase: default_supplier_purchase(),
            max_purchase: default_supplier_purchase(),
            api_region: default_region(),
            purchase_region_mode: PurchaseRegionMode::Omit,
            purchase_region: None,
            credential_api_region_fallback: String::new(),
            rpm_limit: default_supplier_rpm_limit(),
            priority: 0,
            groups: Vec::new(),
            source_channel: default_supplier_source_channel(),
            nickname_prefix: default_supplier_nickname_prefix(),
            restock_only_when_exhausted: false,
            target_usable: 0,
            low_quota_threshold: 0,
            // 0 = 不限价，保持历史行为。
            max_unit_price: 0.0,
        }
    }
}

/// 全局号池采购配置。全实例一份，与 `keySuppliers` 并列在 `config.json` 顶层。
///
/// 语义是**目标存量**而不是「每次买几个」：所有自动采购来的可用凭据合计不得超过
/// `targetCount`。任一供货商推来到货通知时，按「目标存量 - 当前全局可用数」算出缺口，
/// 只向推送方那一家下单补齐。缺口为 0 就不买。
///
/// 为什么必须是存量：供货商之间不设优先级、谁先推来谁先买，若 `targetCount` 是每次
/// 触发的上限，三家各推一次就买三倍，与逐家把 `maxPurchase` 设成该值完全等价，等于
/// 没有这个功能。存量口径下三家抢的是同一个缺口。
///
/// 启用后接管补货判定：各家自己的 `restockOnlyWhenExhausted` /
/// `restockUsableThreshold` / `lowQuotaThreshold` 不再参与，避免两套水位交叉出
/// 第三种行为。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct KeySupplierPoolConfig {
    /// 关闭（默认）时本特性完全不生效，逐家独立采购行为逐字节不变。
    #[serde(default)]
    pub enabled: bool,

    /// 目标存量：所有自动采购来的可用凭据合计上限。
    ///
    /// 默认 0 是「未配置」的哨兵值而非某个业务默认：有人手工把 `enabled` 改成 true
    /// 却忘了填数量时，结果必须是不买，而不是按某个猜出来的默认值开始花钱。
    #[serde(default)]
    pub target_count: u32,

    /// 剩余额度 <= 此值的号不算可用。0 = 不看额度，只认封号与 402。
    ///
    /// 与各家的同名字段语义一致（绝对值，单位对齐上游 `usageLimit`），但启用号池后
    /// 只认这一份，各家自己配的那个不参与判定。
    #[serde(default)]
    pub low_quota_threshold: u32,
}

/// 供货商协议类型。
///
/// - `KiroRs`：历史号商协议。`X-API-Key` 认证，`/api/my/*`，采购带 `client_order_id`（幂等），
///   支持远程注册 webhook。
/// - `KiroApp`：kiroapp.cc 协议。`Authorization: Bearer` 认证，`/openapi/*`，采购是
///   `POST /openapi/claim`，**没有幂等键**（因此绝不重试），回调地址在对方面板手填。
/// - `KiroAppIo`：kiroapp.io 协议。`Authorization: Bearer km_…` 认证，`/api/me/*`，采购是
///   `POST /api/me/purchase`，**带 `client_order_id` 幂等**（因此可安全重试）；阶梯定价，
///   实际扣费只认响应里的 `total_debit`；回调地址在对方面板手填。
/// - `KiroDrop`：Kiro Drop 协议。`X-API-Key: usr-…` 认证，路径与 `kiro-rs` 大体相同
///   （`/api/my/profile`、`POST /api/my/purchase`、`PUT /api/my/webhook`），带
///   `client_order_id` 幂等，支持远程注册 webhook。但有四处硬差异，**不能复用
///   `kiro-rs` 的实现**：
///   1. 没有 `/api/my/stock`，库存在 `GET /api/status` 的 `keys_stock`
///   2. 金额字段是**字符串**（`"884.400000"`），不是 JSON 数字
///   3. 到货推送里**没有** `new_keys` 字段
///   4. 推送的 `purchase_order_id` 不是 32 位十六进制（形如 `batch_xxx`）
/// - `KiroCeo`：kiro.ceo 协议。`X-API-Key` 认证，`/api/my/*`，带 32 位十六进制
///   `client_order_id` 幂等，支持远程注册/测试 webhook，**推送格式与 `kiro-rs`
///   逐字段一致**（只多一个 `zone`）。但采购与概览有三处硬差异：
///   1. **没有 `/api/status`**（也没有 `/api/my/status`）。这个站点是 SPA，未命中的
///      路径会落到前端兜底路由并返回 `200` + HTML，所以按 `kiro-rs` 接会在概览的
///      JSON 反序列化上炸掉，报出来只是一句「请求失败」，完全看不出是缺接口。
///   2. 采购响应的 `keys` 是**纯字符串数组**（`["kiro-xxx", …]`），不是
///      `[{"key": …}]`。按 `kiro-rs` 的 `KeyWire` 解析必然失败——而积分**已经扣了**，
///      等于钱花了 key 丢了。账号密码另放在 `details` 数组里。
///   3. key 前缀是 `kiro-` 而不是 `ksk_`。`kiro-rs` 走的是严格前缀校验，会把这些
///      已付费的 key 全部判为无效。
///   计费单位是**积分**而不是「还能提几个号」；`quota`/`remaining`/`used_quota`
///   字段名没变，只是数字含义变了，所以概览可以照读。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
// 三家供货商都确实是 Kiro 号商，`Kiro` 前缀是事实而非冗余；去掉反而看不出卖的是什么。
#[allow(clippy::enum_variant_names)]
pub enum SupplierKind {
    #[default]
    KiroRs,
    KiroApp,
    /// 显式 rename：避免依赖 serde 对连续大写的 kebab 化规则，且与 `as_str` 保持一致。
    #[serde(rename = "kiroapp-io")]
    KiroAppIo,
    /// Kiro Drop。基本照 `kiro-rs` 抄的，但有四处硬差异，见枚举文档注释。
    KiroDrop,
    /// kiro.ceo。推送格式与 `kiro-rs` 一致，但采购与概览有三处硬差异，
    /// 见枚举文档注释。
    KiroCeo,
}

/// 供货商协议使用的标准采购区域。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum SupplierRegion {
    Us,
    Eu,
}

impl SupplierRegion {
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Us => "us",
            Self::Eu => "eu",
        }
    }

    pub const fn as_api_region(self) -> &'static str {
        match self {
            Self::Us => "us-east-1",
            Self::Eu => "eu-central-1",
        }
    }
}

/// 所有供货商共享的凭据导入预设。连接、采购与区域协议仍由单家配置负责。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KeySupplierCommonConfig {
    #[serde(default = "default_supplier_source_channel")]
    pub source_channel: String,
    /// Nickname 中位于供货商名之后的可选标签。供货商名由服务端强制保留。
    #[serde(default)]
    pub nickname_label: String,
    #[serde(default = "default_supplier_rpm_limit")]
    pub rpm_limit: u32,
    #[serde(default)]
    pub priority: u32,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub auto_delete_forbidden: bool,
}

impl Default for KeySupplierCommonConfig {
    fn default() -> Self {
        Self {
            source_channel: default_supplier_source_channel(),
            nickname_label: String::new(),
            rpm_limit: default_supplier_rpm_limit(),
            priority: 0,
            groups: Vec::new(),
            auto_delete_forbidden: false,
        }
    }
}

/// 单家供货商对公共导入预设的显式覆盖。`None` 表示继承公共值。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SupplierImportOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_delete_forbidden: Option<bool>,
}

impl std::str::FromStr for SupplierRegion {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "us" | "us-east-1" => Ok(Self::Us),
            "eu" | "eu-central-1" => Ok(Self::Eu),
            _ => anyhow::bail!("无效的供应商区域: {value}"),
        }
    }
}

impl std::fmt::Display for SupplierRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// 采购请求怎样选择或省略区域。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "camelCase")]
pub enum PurchaseRegionMode {
    #[default]
    Omit,
    Fixed,
    Webhook,
    BestAvailable,
    Batch,
}

fn purchase_region_mode_is_omit(mode: &PurchaseRegionMode) -> bool {
    *mode == PurchaseRegionMode::Omit
}

impl SupplierKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KiroRs => "kiro-rs",
            Self::KiroApp => "kiro-app",
            Self::KiroAppIo => "kiroapp-io",
            Self::KiroDrop => "kiro-drop",
            Self::KiroCeo => "kiro-ceo",
        }
    }

    /// 该协议是否能远程注册/测试 webhook。`kiro-rs`、`kiro-drop`、`kiro-ceo` 都提供
    /// `PUT /api/my/webhook` 与 `POST /api/my/webhook/test`；两家 kiroapp 都没有
    /// 注册接口，只能在对方面板手填回调地址。
    ///
    /// kiro.ceo 的文档没列 `webhook/test`，但那个端点确实存在（未带密钥探测返回
    /// 401 而不是落到 SPA 兜底的 200 HTML）。
    pub fn supports_webhook_registration(self) -> bool {
        matches!(self, Self::KiroRs | Self::KiroDrop | Self::KiroCeo)
    }

    /// 采购是否带幂等键。带幂等键才允许在网络抖动/5xx 后重试同一单。
    ///
    /// `kiro-app` 的 `/openapi/claim` 没有幂等键，重试等于重复扣积分。
    ///
    /// HTTP 重试策略仍然在客户端按 `kind` 直接分支决定（那里还要区分具体端点）；
    /// 这个开关用在 409 的语义判定上：只有带幂等键的协议，409 才等于「原单已成交」。
    pub fn purchase_is_idempotent(self) -> bool {
        matches!(
            self,
            Self::KiroRs | Self::KiroAppIo | Self::KiroDrop | Self::KiroCeo
        )
    }

    /// 409 是否意味着「原单已经成交」（钱扣了、货出了、我们没拿到）。
    ///
    /// 对大多数家是的：它们的 409 只有一个含义——同一 `client_order_id` 换了参数。
    ///
    /// **kiro.ceo 与 Kiro Drop 都不是**，它们的 409 是「状态冲突」的统称：
    ///
    /// - kiro.ceo：库存不足、已达最大持有库存上限、幂等键撞了别的订单
    /// - Kiro Drop：余额不足、库存不足、订单号冲突、价格超过 `max_total_cny`
    ///
    /// 这些里面只有「订单号冲突」扣了钱，其余几种一分没动。按「已成交」去报会告诉运维
    /// 「积分已扣，去订单历史补取 key」——一条不存在的订单，纯误导。
    ///
    /// Drop 早期文档把这些分开成 403（余额不足）/ 404（库存不足）/ 409（订单号冲突），
    /// 后来全并进了 409，所以这个判断跟着改。
    pub fn conflict_means_order_settled(self) -> bool {
        !matches!(self, Self::KiroCeo | Self::KiroDrop)
    }
}

impl std::fmt::Display for SupplierKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SupplierKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "kiro-rs" | "kirors" | "default" => Ok(Self::KiroRs),
            "kiro-app" | "kiroapp" => Ok(Self::KiroApp),
            "kiroapp-io" | "kiroappio" | "kiro-app-io" => Ok(Self::KiroAppIo),
            "kiro-drop" | "kirodrop" | "drop" => Ok(Self::KiroDrop),
            "kiro-ceo" | "kiroceo" | "kiro.ceo" | "ceo" => Ok(Self::KiroCeo),
            other => anyhow::bail!("无效的供货商协议类型: {other}"),
        }
    }
}

/// 单个供货商的完整配置。`settings` 复用历史单供货商结构，避免两套字段。
///
/// 不派生 `Eq`：`settings.max_unit_price` 是 f64。
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct KeySupplierEntryConfig {
    /// 稳定标识。用于路由、事件表 `supplier_id` 与前端选择，创建后不可改。
    pub id: String,
    /// 展示名。
    #[serde(default)]
    pub name: String,
    /// 协议类型。
    #[serde(default)]
    pub kind: SupplierKind,
    /// 关闭后不参与自动采购，webhook 仍然落库（标记 skipped）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `None` 仅表示来自 v0.9.45 或更早的旧配置；加载时会把旧扁平字段迁移成显式覆盖。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_overrides: Option<SupplierImportOverrides>,
    #[serde(flatten)]
    pub settings: KeySupplierConfig,
}

impl std::fmt::Debug for KeySupplierEntryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeySupplierEntryConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("enabled", &self.enabled)
            .field("import_overrides", &self.import_overrides)
            .field("settings", &self.settings)
            .finish()
    }
}

impl KeySupplierEntryConfig {
    /// 把历史单供货商配置迁移成一条多供货商条目。
    pub fn from_legacy(settings: KeySupplierConfig) -> Self {
        Self {
            id: "default".to_string(),
            name: "默认供货商".to_string(),
            kind: SupplierKind::KiroRs,
            enabled: true,
            import_overrides: None,
            settings,
        }
    }

    /// 历史单供货商配置是否值得迁移（没配 baseUrl/apiKey 的空壳不迁）。
    pub fn legacy_is_configured(settings: &KeySupplierConfig) -> bool {
        !settings.base_url.trim().is_empty() || !settings.api_key.trim().is_empty()
    }
}

/// 普通 429 的可配置重试策略。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RetryPolicy {
    /// 普通 429 后的跨请求冷却时间；0 表示不进入跨请求冷却。
    pub rate_limit_cooldown_ms: u64,
    /// 每个凭据的请求重试预算。非默认策略会按账号数放大，并受全局上限保护。
    pub max_request_retries: usize,
    /// 指数退避基础时长。
    pub base_backoff_ms: u64,
    /// 指数退避最大时长。
    pub max_backoff_ms: u64,
    /// 普通 429 后是否优先切换其它凭据。
    pub credential_switch_on_429: bool,
    /// 是否尊重上游 Retry-After 头。
    pub respect_retry_after: bool,
}

impl RetryPolicy {
    pub fn preset(mode: RetryMode) -> Self {
        match mode {
            RetryMode::Failover => Self {
                rate_limit_cooldown_ms: 0,
                max_request_retries: 3,
                base_backoff_ms: 1_000,
                max_backoff_ms: 8_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Turbo => Self {
                rate_limit_cooldown_ms: 1_000,
                max_request_retries: 12,
                base_backoff_ms: 100,
                max_backoff_ms: 1_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Fast => Self {
                rate_limit_cooldown_ms: 3_000,
                max_request_retries: 9,
                base_backoff_ms: 200,
                max_backoff_ms: 2_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Balanced => Self {
                rate_limit_cooldown_ms: 10_000,
                max_request_retries: 9,
                base_backoff_ms: 500,
                max_backoff_ms: 5_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Steady => Self {
                rate_limit_cooldown_ms: 30_000,
                max_request_retries: 6,
                base_backoff_ms: 1_000,
                max_backoff_ms: 10_000,
                credential_switch_on_429: true,
                respect_retry_after: true,
            },
            RetryMode::Polite => Self {
                rate_limit_cooldown_ms: 60_000,
                max_request_retries: 4,
                base_backoff_ms: 2_000,
                max_backoff_ms: 30_000,
                credential_switch_on_429: false,
                respect_retry_after: true,
            },
            RetryMode::Custom => Self::preset(RetryMode::Fast),
        }
    }

    pub fn effective(mode: RetryMode, custom: Option<&RetryPolicy>) -> anyhow::Result<Self> {
        let policy = if mode == RetryMode::Custom {
            custom
                .cloned()
                .unwrap_or_else(|| Self::preset(RetryMode::Fast))
        } else {
            Self::preset(mode)
        };

        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.rate_limit_cooldown_ms > 120_000 {
            anyhow::bail!("rateLimitCooldownMs 必须在 0..=120000 之间");
        }
        if !(1..=30).contains(&self.max_request_retries) {
            anyhow::bail!("maxRequestRetries 必须在 1..=30 之间");
        }
        if !(50..=30_000).contains(&self.base_backoff_ms) {
            anyhow::bail!("baseBackoffMs 必须在 50..=30000 之间");
        }
        if self.max_backoff_ms < self.base_backoff_ms || self.max_backoff_ms > 120_000 {
            anyhow::bail!("maxBackoffMs 必须在 baseBackoffMs..=120000 之间");
        }
        Ok(())
    }
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 上一次成功更新前正在运行的版本号，用于在前端展示「回退到 vX.Y.Z」按钮。
    /// 实际回退动作通过 `<exe>.backup` 文件完成，无需访问网络。
    #[serde(default)]
    pub update_previous_version: Option<String>,

    /// GitHub Personal Access Token（可选）。设置后 GitHub Releases 接口会带上
    /// `Authorization: Bearer <token>`，把限流从匿名 60/h 提到认证 5000/h。
    /// 仅需 `public_repo` 读取权限即可。
    #[serde(default)]
    pub github_token: Option<String>,

    /// 上一次成功完成在线更新的时间（RFC3339）。前端用于显示「上次更新于 …」。
    #[serde(default)]
    pub update_last_applied_at: Option<String>,

    /// 是否启用无人值守自动更新。开启后服务会在每天的 `update_auto_apply_time`
    /// 时刻检查 GitHub Releases，发现新版本即自动下载二进制并替换重启。
    #[serde(default)]
    pub update_auto_apply: bool,

    /// 自动更新的每日触发时间（本地时区，`HH:MM` 24 小时制）。
    /// 默认 03:00 凌晨执行，对在线服务影响最小。
    #[serde(default = "default_update_auto_apply_time")]
    pub update_auto_apply_time: String,

    /// 负载均衡模式（"priority" / "balanced" / "least_conn"，默认 "least_conn" 最少负载）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 代理均衡模式（"sticky" / "round_robin" / "least_load"）
    #[serde(default = "default_proxy_balancing_mode")]
    pub proxy_balancing_mode: String,

    /// 账号级 429 风控触发时是否对当前凭据进入冷却并故障转移（默认 true）。
    ///
    /// 关闭后：429 + suspicious activity 仍按普通瞬态错误重试，不切换凭据。
    /// 开启后：识别到 suspicious activity 字符串时，把当前凭据冷却 `account_throttle_cooldown_secs` 秒，
    /// 立即切换到下一个可用凭据。
    #[serde(default = "default_account_throttle_failover")]
    pub account_throttle_failover: bool,

    /// 账号级风控冷却时长（秒，默认 1800 = 30 分钟）。
    #[serde(default = "default_account_throttle_cooldown_secs")]
    pub account_throttle_cooldown_secs: u64,

    /// 普通 429 重试策略模式。默认 `failover` 保持当前项目行为。
    #[serde(default = "default_retry_mode")]
    pub retry_mode: RetryMode,

    /// `retry_mode = custom` 时使用的普通 429 自定义策略。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_policy: Option<RetryPolicy>,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 客户端请求 thinking 但 Kiro 未返回 reasoning 时，是否按严格协议返回错误。
    /// 默认 false：保留上游已经产生的正文或工具调用，不伪造 thinking。
    #[serde(default)]
    pub strict_thinking_validation: bool,

    /// 是否把无 system/tools/thinking/历史/多模态的单轮 `ping` 作为本地健康检查，
    /// 直接返回 `pong`。默认开启，可关闭以恢复完全上游行为。
    #[serde(default = "default_true")]
    pub local_ping_response: bool,

    /// 是否兼容 system 非空、唯一 user 文本为空且无工具/多模态内容的请求。
    #[serde(default)]
    pub empty_user_message_compat: bool,

    /// 是否启用严格模型资料探针的本地确定性回复。
    #[serde(default = "default_true")]
    pub model_profile_exact_answers_enabled: bool,

    /// 工具兼容模式。默认 `claude-code`：把 Claude Code 内置工具名/入参双向适配为
    /// Kiro 内置工具；`raw` 保留旧行为、直接透传客户端工具 schema，用于排障。
    #[serde(default = "default_tool_compatibility_mode")]
    pub tool_compatibility_mode: ToolCompatibilityMode,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 端点路由模式：best（默认最好模式）或 manual（手动端点链）。
    #[serde(default)]
    pub endpoint_mode: EndpointMode,

    /// 是否启用请求链路追踪（写 traces.db）。默认 true。
    ///
    /// 关闭后：不再写入 trace 记录、不走 TraceSink，但 `GET /api/admin/traces`
    /// 仍可查询历史已存记录。适合隐私敏感或磁盘紧张的场景。
    #[serde(default = "default_trace_enabled")]
    pub trace_enabled: bool,

    /// 是否采集自动压缩诊断。独立于 trace 开关；关闭后请求入口立即短路。
    #[serde(default = "default_true")]
    pub auto_compact_diagnostics_enabled: bool,

    /// 请求链路追踪记录保留天数（默认 7）。后台任务每天清理超期记录。
    #[serde(default = "default_trace_retention_days")]
    pub trace_retention_days: u32,

    /// 请求用量日志（usage_log.*.jsonl + 聚合桶）保留天数（默认 31）。
    #[serde(default = "default_usage_log_retention_days")]
    pub usage_log_retention_days: u32,

    /// NewAPI 利润统计连接地址（仅管理员统计使用）。
    #[serde(default)]
    pub profit_newapi_base: Option<String>,

    /// NewAPI 利润统计访问令牌（仅服务端使用，不回显到管理端）。
    #[serde(default)]
    pub profit_newapi_token: Option<String>,

    /// NewAPI 管理员用户 ID。
    #[serde(default)]
    pub profit_newapi_user: Option<String>,

    /// Kiro 每个 Credit 的采购成本，默认 ¥45 / 2000。
    #[serde(default = "default_profit_credit_price")]
    pub profit_credit_price: f64,

    /// NewAPI quota 转换为 ¥1 所需的额度单位。
    #[serde(default = "default_profit_quota_per_unit")]
    pub profit_quota_per_unit: f64,

    /// Kiro API Key 供应商与 Webhook 自动采购配置。
    ///
    /// 单供货商的历史字段。多供货商改造后仅作为升级来源：启动时若 `key_suppliers`
    /// 为空且这里已配置，会迁移成 `id=default` 的一项。保留字段本身是为了回滚兼容。
    #[serde(default)]
    pub key_supplier: KeySupplierConfig,

    /// 多供货商 Key 采购配置。每项一家供货商，自带协议类型、凭据与导入预设。
    #[serde(default)]
    pub key_suppliers: Vec<KeySupplierEntryConfig>,

    /// 供货商公共凭据导入预设。单家只在 `importOverrides` 中声明差异。
    #[serde(default)]
    pub key_supplier_common: KeySupplierCommonConfig,

    /// 全局号池采购配置。控制「所有采购来的号合计养几个」，跨供货商共享一个缺口。
    ///
    /// 缺失时整块取默认值（`enabled = false`），使老 `config.json` 升级后行为不变。
    #[serde(default)]
    pub key_supplier_pool: KeySupplierPoolConfig,

    /// 是否记录失败、中断和可选恢复请求的完整脱敏诊断快照。
    #[serde(default = "default_true")]
    pub error_snapshot_enabled: bool,

    /// 判死凭据是否在保留期结束后自动删除。
    ///
    /// 关闭后死号只禁用、永久留在池子里（配合管理端的「含已禁用」筛选决定是否显示）。
    /// 与凭据级 `deleteOnForbidden` 是 AND 关系：这是全局总闸，那个是逐账号白名单
    /// （手工添加的账号通常是唯一一份，即使总闸打开也不参与自动删除）。
    #[serde(default = "default_true")]
    pub dead_credential_auto_delete: bool,

    /// 判死凭据的保留时长（小时）。
    ///
    /// 403 命中封禁标记后凭据先被禁用并记录 `died_at`，供运营查看存活时长与死因；
    /// 超过本时长后由后台清理删除（仅限带 `deleteOnForbidden` 的凭据，手工添加的
    /// 只禁用不自动删）。用小时而非天：线上封号速率约每小时 5 个，按天保留会在
    /// 凭据列表里积压上百条死号。
    #[serde(default = "default_dead_credential_retention_hours")]
    pub dead_credential_retention_hours: u32,

    /// 普通错误快照保留天数。critical 与手动 pin 不参加自动过期清理。
    #[serde(default = "default_error_snapshot_retention_days")]
    pub error_snapshot_retention_days: u32,

    /// 错误快照总存储软上限（GiB）。
    #[serde(default = "default_error_snapshot_max_storage_gb")]
    pub error_snapshot_max_storage_gb: u64,

    /// 是否保存经过重试后恢复成功的请求现场。
    #[serde(default)]
    pub error_snapshot_capture_recovered: bool,

    /// 是否保存脱敏后的请求/响应正文；关闭后仅记录结构化元数据。
    #[serde(default = "default_true")]
    pub error_snapshot_capture_bodies: bool,

    /// 磁盘至少保留的空闲空间（GiB）；低于此值时新快照降级为元数据。
    #[serde(default = "default_error_snapshot_min_free_disk_gb")]
    pub error_snapshot_min_free_disk_gb: u64,

    /// 流式空闲超时（秒，默认 120）。
    ///
    /// 上游返回 200 后，若连续 `stream_idle_timeout_secs` 秒没有收到任何字节
    /// （首字节前挂死或中途停流），中转层主动收尾结束该流，避免空烧到绝对超时。
    /// 复现 Kiro-Go 的 idle watchdog（120s）。`0` = 关闭空闲超时（仅靠绝对超时兜底）。
    /// 运行时可在管理面板调整。
    #[serde(default = "default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,

    /// 流式请求是否在 Kiro 上游响应前立即提交 SSE，并用注释心跳保活。
    /// false 保留真实上游 HTTP 状态；true 时提交后的上游错误改走 SSE error。
    #[serde(default)]
    pub early_stream_handshake: bool,

    /// 下发 `model_context_window_exceeded` 的上下文占比阈值（百分比）。
    ///
    /// 客户端（Claude Code 等）收到这个 stop_reason 才会压缩历史。原实现硬编码
    /// 100%，属**事后通知**：那时压缩自己也没余量了——compact 请求同样带全量历史、
    /// 同样撞上游字节上限，形成死锁（线上实测 240 分钟内发了 5 次信号，会话仍死在
    /// 400）。降到 85% 是为了给压缩本身留出 15% 窗口。
    ///
    /// 取 85 的依据：社区报告的 Claude Code auto-compact 触发点在 83.5%–92%，
    /// 取下沿确保我们的信号先到。做成配置项而非常量，是为了线上可灰度、可回滚：
    /// 触发太频繁往 88–90 调，仍撞 400 往 80 调。
    ///
    /// 注意：这条路径**不改 usage 上报**，因此对下游计费零影响。
    #[serde(default = "default_context_window_signal_threshold_pct")]
    pub context_window_signal_threshold_pct: f64,

    /// 纯文本响应因 `max_tokens` 截断时是否自动向 Kiro 发起续写。
    /// 默认关闭；开启后可能增加上游调用次数、总耗时和计费。
    #[serde(default)]
    pub auto_continue_enabled: bool,

    /// 单条响应最多自动续写的轮数。仅在 `auto_continue_enabled` 开启时生效。
    #[serde(default = "default_auto_continue_max")]
    pub auto_continue_max: u32,

    /// 对“已输出少量正文后过早 EOF”的流启用自动恢复。默认关闭，避免误判正常短答。
    #[serde(default)]
    pub partial_stream_recovery_enabled: bool,

    /// 首个语义输出后多快结束才视为可疑半截流（毫秒）。
    #[serde(default = "default_partial_stream_recovery_window_ms")]
    pub partial_stream_recovery_window_ms: u64,

    /// 身份归一化：把 Kiro 网关注入的品牌自述（"I'm Kiro" / "made by AWS"）改写回
    /// Claude / Anthropic。默认 true（开启）——底层本就是真实 Claude 模型，用户已授权
    /// 还原对外身份一致性以通过验真检测。设 false 保留上游原始品牌措辞。见 anthropic::identity。
    #[serde(default = "default_true")]
    pub identity_normalization: bool,

    /// 429 降级桶链运行时覆盖。键 = 主端点名（`ide` / `cli`），值 = 该主端点 429 时
    /// 依次尝试的备用桶名（有序）。缺省（None）时回退各 endpoint 的静态 `fallback_chain()`，
    /// 保证老部署零行为变化。空数组 = 该主端点不降级。运行时可在管理面板编辑。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_chains: Option<HashMap<String, Vec<String>>>,

    /// 单个客户端请求内「备用桶尝试」的总次数硬上限（跨 attempt 累计）。
    /// 防止「链长 × attempt 数」把单请求放大成上百次上游调用。默认 6；`0` = 不限。
    /// 运行时可在管理面板调整。
    #[serde(default = "default_max_bucket_attempts_per_request")]
    pub max_bucket_attempts_per_request: usize,

    /// 是否启用 rs 的模拟 prompt cache 计量。
    #[serde(default = "default_cache_metering_enabled")]
    pub cache_metering_enabled: bool,

    /// 未显式提供 cache_control TTL 时使用的默认缓存窗口（秒）。
    #[serde(default = "default_cache_default_ttl_secs")]
    pub cache_default_ttl_secs: u64,

    /// 请求没有 cache_control 时，是否自动对稳定前缀进行模拟缓存。
    #[serde(default = "default_cache_auto_without_control")]
    pub cache_auto_without_control: bool,

    /// 是否仅登记每个请求最近的可复用前缀；关闭时恢复旧的全历史前缀算法。
    #[serde(default = "default_cache_rolling_prefix_enabled")]
    pub cache_rolling_prefix_enabled: bool,

    /// 滚动模式下每个请求最多参与查询和登记的最近前缀数量。
    #[serde(default = "default_cache_rolling_prefix_limit")]
    pub cache_rolling_prefix_limit: usize,

    /// 模拟缓存最多保留的前缀条目数。
    #[serde(default = "default_cache_capacity")]
    pub cache_capacity: usize,

    /// 过期清理和缓存状态落盘周期（秒）。
    #[serde(default = "default_cache_flush_interval_secs")]
    pub cache_flush_interval_secs: u64,

    /// 缓存命中率整形——下界（百分比 0..=100）。
    ///
    /// 上游不下发真实缓存 token，中转层自行模拟；本旋钮把最终呈现（newapi 计费用量）的
    /// 命中率 `cache_read/(input+cache_read)` **钳制**进 `[min, max]` 区间：低于 min 提到
    /// min，高于 max 压到 max，区间内保留真实模拟值。整形只在 `input↔cache_read` 之间挪，
    /// **保持 `input+creation+read` 总量不变**（计费总额不漂），creation 不动。
    /// `min == 0 && max == 0` = 关闭整形（默认，零行为变化）。运行时可在管理面板调整。
    #[serde(default)]
    pub cache_hit_rate_min_pct: u32,

    /// 缓存命中率整形——上界（百分比 0..=100）。见 [`Self::cache_hit_rate_min_pct`]。
    /// `min == 0 && max == 0` = 关闭。仅 max>0 时也生效（下界视为 0）。
    #[serde(default)]
    pub cache_hit_rate_max_pct: u32,

    /// 是否启用 Kiro 出站图片总预算治理。只压缩历史图片，不修改当前轮图片。
    #[serde(default = "default_true")]
    pub image_budget_enabled: bool,

    /// 所有历史与当前轮图片的 base64 总预算字节数。
    #[serde(default = "default_image_total_budget")]
    pub image_total_base64_budget_bytes: usize,

    /// 图片 base64 本地硬上限；普通体和激进体都超过时才拒绝。
    #[serde(default = "default_image_hard_limit")]
    pub image_hard_base64_limit_bytes: usize,

    /// 普通预检压缩历史图片时的最大边长。
    #[serde(default = "default_image_history_dimension")]
    pub image_history_max_dimension: u32,

    /// 普通预检压缩历史图片时的 JPEG 质量。
    #[serde(default = "default_image_history_quality")]
    pub image_history_jpeg_quality: u8,

    /// 上游请求体长度拒绝后，一次降级重试使用的历史图片最大边长。
    #[serde(default = "default_image_retry_dimension")]
    pub image_retry_history_max_dimension: u32,

    /// 上游请求体长度拒绝后，一次降级重试使用的历史图片 JPEG 质量。
    #[serde(default = "default_image_retry_quality")]
    pub image_retry_history_jpeg_quality: u8,

    /// 图片长边硬上限（像素），历史图与当前轮图都封顶，与字节预算解耦。
    /// 对齐上游多图请求的像素约束（2000）。
    #[serde(default = "default_image_hard_max_dimension")]
    pub image_hard_max_dimension: u32,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_supplier_purchase() -> u32 {
    1
}

fn default_supplier_rpm_limit() -> u32 {
    10
}

fn default_supplier_source_channel() -> String {
    "Webhook 自动采购".to_string()
}

fn default_supplier_nickname_prefix() -> String {
    "自动采购".to_string()
}

fn default_kiro_version() -> String {
    "2.3.0".to_string()
}

fn default_system_version() -> String {
    "macos".to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    // 默认最少负载：把请求优先分给当前在途请求数最少的凭据，避免高优先级凭据被打爆。
    "least_conn".to_string()
}

fn default_proxy_balancing_mode() -> String {
    "sticky".to_string()
}

fn default_account_throttle_failover() -> bool {
    true
}

fn default_account_throttle_cooldown_secs() -> u64 {
    30 * 60
}

fn default_retry_mode() -> RetryMode {
    RetryMode::Failover
}

fn default_update_auto_apply_time() -> String {
    "03:00".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_tool_compatibility_mode() -> ToolCompatibilityMode {
    ToolCompatibilityMode::ClaudeCode
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

fn default_trace_enabled() -> bool {
    true
}

fn default_trace_retention_days() -> u32 {
    7
}

fn default_error_snapshot_retention_days() -> u32 {
    7
}

fn default_dead_credential_retention_hours() -> u32 {
    24
}

fn default_error_snapshot_max_storage_gb() -> u64 {
    5
}

fn default_error_snapshot_min_free_disk_gb() -> u64 {
    10
}

fn default_stream_idle_timeout_secs() -> u64 {
    120
}

/// 见 [`Config::context_window_signal_threshold_pct`]。85% 留 15% 窗口给压缩本身。
fn default_context_window_signal_threshold_pct() -> f64 {
    85.0
}

fn default_auto_continue_max() -> u32 {
    3
}

fn default_partial_stream_recovery_window_ms() -> u64 {
    750
}

fn default_max_bucket_attempts_per_request() -> usize {
    6
}

fn default_cache_metering_enabled() -> bool {
    true
}

fn default_cache_default_ttl_secs() -> u64 {
    30 * 60
}

fn default_cache_auto_without_control() -> bool {
    true
}

fn default_cache_rolling_prefix_enabled() -> bool {
    true
}

fn default_cache_rolling_prefix_limit() -> usize {
    8
}

fn default_cache_capacity() -> usize {
    4096
}

fn default_cache_flush_interval_secs() -> u64 {
    60
}

fn default_image_total_budget() -> usize {
    819_200
}

fn default_image_hard_limit() -> usize {
    8 * 1024 * 1024
}

fn default_image_history_dimension() -> u32 {
    1_280
}

fn default_image_history_quality() -> u8 {
    72
}

fn default_image_retry_dimension() -> u32 {
    960
}

fn default_image_retry_quality() -> u8 {
    60
}

fn default_image_hard_max_dimension() -> u32 {
    2_000
}

fn default_usage_log_retention_days() -> u32 {
    31
}

fn default_profit_credit_price() -> f64 {
    45.0 / 2000.0
}

fn default_profit_quota_per_unit() -> f64 {
    500_000.0
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            update_previous_version: None,
            github_token: None,
            update_last_applied_at: None,
            update_auto_apply: false,
            update_auto_apply_time: default_update_auto_apply_time(),
            load_balancing_mode: default_load_balancing_mode(),
            proxy_balancing_mode: default_proxy_balancing_mode(),
            account_throttle_failover: default_account_throttle_failover(),
            account_throttle_cooldown_secs: default_account_throttle_cooldown_secs(),
            retry_mode: default_retry_mode(),
            retry_policy: None,
            extract_thinking: default_extract_thinking(),
            strict_thinking_validation: false,
            local_ping_response: default_true(),
            empty_user_message_compat: false,
            model_profile_exact_answers_enabled: default_true(),
            tool_compatibility_mode: default_tool_compatibility_mode(),
            default_endpoint: default_endpoint(),
            endpoint_mode: EndpointMode::default(),
            trace_enabled: default_trace_enabled(),
            auto_compact_diagnostics_enabled: default_true(),
            trace_retention_days: default_trace_retention_days(),
            usage_log_retention_days: default_usage_log_retention_days(),
            profit_newapi_base: None,
            profit_newapi_token: None,
            profit_newapi_user: None,
            profit_credit_price: default_profit_credit_price(),
            profit_quota_per_unit: default_profit_quota_per_unit(),
            key_supplier: KeySupplierConfig::default(),
            key_suppliers: Vec::new(),
            key_supplier_common: KeySupplierCommonConfig::default(),
            key_supplier_pool: KeySupplierPoolConfig::default(),
            error_snapshot_enabled: true,
            dead_credential_auto_delete: true,
            dead_credential_retention_hours: default_dead_credential_retention_hours(),
            error_snapshot_retention_days: default_error_snapshot_retention_days(),
            error_snapshot_max_storage_gb: default_error_snapshot_max_storage_gb(),
            error_snapshot_capture_recovered: false,
            error_snapshot_capture_bodies: true,
            error_snapshot_min_free_disk_gb: default_error_snapshot_min_free_disk_gb(),
            stream_idle_timeout_secs: default_stream_idle_timeout_secs(),
            early_stream_handshake: false,
            context_window_signal_threshold_pct: default_context_window_signal_threshold_pct(),
            auto_continue_enabled: false,
            auto_continue_max: default_auto_continue_max(),
            partial_stream_recovery_enabled: false,
            partial_stream_recovery_window_ms: default_partial_stream_recovery_window_ms(),
            identity_normalization: true,
            endpoint_chains: None,
            max_bucket_attempts_per_request: default_max_bucket_attempts_per_request(),
            cache_metering_enabled: default_cache_metering_enabled(),
            cache_default_ttl_secs: default_cache_default_ttl_secs(),
            cache_auto_without_control: default_cache_auto_without_control(),
            cache_rolling_prefix_enabled: default_cache_rolling_prefix_enabled(),
            cache_rolling_prefix_limit: default_cache_rolling_prefix_limit(),
            cache_capacity: default_cache_capacity(),
            cache_flush_interval_secs: default_cache_flush_interval_secs(),
            cache_hit_rate_min_pct: 0,
            cache_hit_rate_max_pct: 0,
            image_budget_enabled: true,
            image_total_base64_budget_bytes: default_image_total_budget(),
            image_hard_base64_limit_bytes: default_image_hard_limit(),
            image_history_max_dimension: default_image_history_dimension(),
            image_history_jpeg_quality: default_image_history_quality(),
            image_retry_history_max_dimension: default_image_retry_dimension(),
            image_retry_history_jpeg_quality: default_image_retry_quality(),
            image_hard_max_dimension: default_image_hard_max_dimension(),
            endpoints: HashMap::new(),
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());

        // 用户手工把字符串字段清空（如 `"updateAutoApplyTime": ""`）时，serde 默认值不会
        // 介入；这里把"看起来像空"的关键字段回退到默认值，避免后续业务用到
        // 空字符串导致难以诊断的错误。
        if config.update_auto_apply_time.trim().is_empty() {
            config.update_auto_apply_time = default_update_auto_apply_time();
        }

        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        AtomicFile::new(path, AllowOverwrite)
            .write(|file| file.write_all(content.as_bytes()))
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_supplier_defaults_keep_legacy_config_compatible() {
        let config: Config = serde_json::from_str("{}").unwrap();

        assert!(config.key_supplier.base_url.is_empty());
        assert!(config.key_supplier.api_key.is_empty());
        assert!(config.key_supplier.public_base_url.is_empty());
        assert!(config.key_supplier.webhook_token.is_empty());
        assert!(!config.key_supplier.auto_purchase);
        assert!(!config.key_supplier.auto_delete_forbidden);
        assert_eq!(config.key_supplier.min_purchase, 1);
        assert_eq!(config.key_supplier.max_purchase, 1);
        assert_eq!(config.key_supplier.api_region, "us-east-1");
        assert_eq!(config.key_supplier.rpm_limit, 10);
        assert_eq!(config.key_supplier.priority, 0);
        assert!(config.key_supplier.groups.is_empty());
        assert_eq!(config.key_supplier.source_channel, "Webhook 自动采购");
        assert_eq!(config.key_supplier.nickname_prefix, "自动采购");
    }

    #[test]
    fn supplier_kind_wire_names_round_trip_and_are_all_distinct() {
        // `kind` 落在 config.json 里，序列化名一变，线上配置就读不回来了。
        for (kind, wire) in [
            (SupplierKind::KiroRs, "kiro-rs"),
            (SupplierKind::KiroApp, "kiro-app"),
            (SupplierKind::KiroAppIo, "kiroapp-io"),
            (SupplierKind::KiroDrop, "kiro-drop"),
            (SupplierKind::KiroCeo, "kiro-ceo"),
        ] {
            assert_eq!(kind.as_str(), wire);
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(wire));
            assert_eq!(
                serde_json::from_value::<SupplierKind>(serde_json::json!(wire)).unwrap(),
                kind
            );
            assert_eq!(wire.parse::<SupplierKind>().unwrap(), kind);
            assert_eq!(kind.to_string(), wire);
        }

        // 两家 kiroapp 绝不能互相解析成对方——那会让采购走错协议和错误的重试策略。
        assert_eq!(
            "kiroappio".parse::<SupplierKind>().unwrap(),
            SupplierKind::KiroAppIo
        );
        assert_eq!(
            "kiro-app-io".parse::<SupplierKind>().unwrap(),
            SupplierKind::KiroAppIo
        );
        assert_eq!(
            "kiroapp".parse::<SupplierKind>().unwrap(),
            SupplierKind::KiroApp
        );
        assert!("kiro-io".parse::<SupplierKind>().is_err());

        // kiro-drop 的别名不能撞到别家：drop 协议的金额是字符串，接错会 Decode 失败。
        assert_eq!(
            "kirodrop".parse::<SupplierKind>().unwrap(),
            SupplierKind::KiroDrop
        );
        assert_eq!(
            "drop".parse::<SupplierKind>().unwrap(),
            SupplierKind::KiroDrop
        );

        // kiro-ceo 的别名同样不能撞到别家：它的采购响应形状与 kiro-rs 不同，
        // 接错就是钱扣了 key 解析不出来。
        for alias in ["kiroceo", "kiro.ceo", "ceo"] {
            assert_eq!(
                alias.parse::<SupplierKind>().unwrap(),
                SupplierKind::KiroCeo
            );
        }

        // 幂等能力决定能不能重试；接错会导致重复扣费。
        assert!(SupplierKind::KiroRs.purchase_is_idempotent());
        assert!(SupplierKind::KiroAppIo.purchase_is_idempotent());
        assert!(SupplierKind::KiroDrop.purchase_is_idempotent());
        assert!(SupplierKind::KiroCeo.purchase_is_idempotent());
        assert!(!SupplierKind::KiroApp.purchase_is_idempotent());
    }

    #[test]
    fn key_supplier_config_round_trips_in_camel_case() {
        let input = serde_json::json!({
            "keySupplier": {
                "baseUrl": "https://supplier.example",
                "apiKey": "secret",
                "publicBaseUrl": "https://public.example",
                "webhookToken": "token",
                "webhookSecret": "hook-secret",
                "autoPurchase": true,
                "autoDeleteForbidden": true,
                "minPurchase": 2,
                "maxPurchase": 4,
                "apiRegion": "eu-central-1",
                "rpmLimit": 20,
                "priority": 3,
                "groups": ["paid"],
                "sourceChannel": "supplier",
                "nicknamePrefix": "auto",
                "restockOnlyWhenExhausted": true,
                "targetUsable": 2,
                "lowQuotaThreshold": 500,
                "maxUnitPrice": 30.0
            }
        });

        let config: Config = serde_json::from_value(input.clone()).unwrap();
        let encoded = serde_json::to_value(config).unwrap();
        assert_eq!(encoded["keySupplier"], input["keySupplier"]);
        assert!(encoded.get("key_supplier").is_none());
    }

    #[test]
    fn legacy_restock_usable_threshold_still_loads_as_the_target_count() {
        // 字段改名前叫 `restockUsableThreshold`，语义是低水位。线上配置里存的是旧名，
        // 读不进来就会静默变成 0，而 0 是「不买」——用户只会看到采购全停，
        // 完全看不出是字段名换了。
        let config: Config = serde_json::from_value(serde_json::json!({
            "keySupplier": { "restockOnlyWhenExhausted": true, "restockUsableThreshold": 2 }
        }))
        .unwrap();

        assert_eq!(config.key_supplier.target_usable, 2);
        assert!(config.key_supplier.restock_only_when_exhausted);
        // 写回时统一用新名，不再产出旧名。
        let encoded = serde_json::to_value(&config).unwrap();
        assert_eq!(encoded["keySupplier"]["targetUsable"], 2);
        assert!(
            encoded["keySupplier"]
                .get("restockUsableThreshold")
                .is_none()
        );
    }

    #[test]
    fn save_persists_updated_json_value() {
        let path = std::env::temp_dir().join(format!(
            "kiro-config-save-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut config = Config::load(&path).unwrap();
        config.host = "127.0.0.2".to_string();
        config.save().unwrap();

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["host"], "127.0.0.2");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn debug_does_not_expose_key_supplier_secrets() {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "keySupplier": {
                "apiKey": "config-api-key-canary",
                "webhookToken": "config-webhook-token-canary"
            }
        }))
        .unwrap();
        config.key_supplier.api_key = "config-api-key-canary".to_string();
        config.key_supplier.webhook_token = "config-webhook-token-canary".to_string();

        let debug = format!("{:?}", config);

        assert!(!debug.contains("config-api-key-canary"));
        assert!(!debug.contains("config-webhook-token-canary"));
    }

    #[test]
    fn error_snapshot_defaults_are_safe_and_round_trip_in_camel_case() {
        let defaulted: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(defaulted.error_snapshot_enabled);
        assert_eq!(defaulted.error_snapshot_retention_days, 7);
        assert_eq!(defaulted.error_snapshot_max_storage_gb, 5);
        assert!(!defaulted.error_snapshot_capture_recovered);
        assert!(defaulted.error_snapshot_capture_bodies);
        assert_eq!(defaulted.error_snapshot_min_free_disk_gb, 10);

        let custom: Config = serde_json::from_value(serde_json::json!({
            "errorSnapshotEnabled": false,
            "errorSnapshotRetentionDays": 30,
            "errorSnapshotMaxStorageGb": 64,
            "errorSnapshotCaptureRecovered": false,
            "errorSnapshotCaptureBodies": false,
            "errorSnapshotMinFreeDiskGb": 32
        }))
        .unwrap();
        let encoded = serde_json::to_value(custom).unwrap();
        assert_eq!(encoded["errorSnapshotEnabled"], false);
        assert_eq!(encoded["errorSnapshotRetentionDays"], 30);
        assert_eq!(encoded["errorSnapshotMaxStorageGb"], 64);
        assert_eq!(encoded["errorSnapshotCaptureRecovered"], false);
        assert_eq!(encoded["errorSnapshotCaptureBodies"], false);
        assert_eq!(encoded["errorSnapshotMinFreeDiskGb"], 32);
    }

    #[test]
    fn profit_config_defaults_to_45_over_2000_and_round_trips() {
        let defaulted: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!((defaulted.profit_credit_price - 0.0225).abs() < 1e-12);
        assert_eq!(defaulted.profit_quota_per_unit, 500_000.0);
        assert!(defaulted.profit_newapi_token.is_none());

        let value = serde_json::to_value(&defaulted).unwrap();
        assert_eq!(value["profitCreditPrice"], 0.0225);
        assert_eq!(value["profitQuotaPerUnit"], 500_000.0);
    }

    #[test]
    fn local_ping_response_defaults_on_and_round_trips_in_camel_case() {
        let defaulted: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(defaulted.local_ping_response);

        let disabled: Config = serde_json::from_value(serde_json::json!({
            "localPingResponse": false
        }))
        .unwrap();
        assert!(!disabled.local_ping_response);
        let encoded = serde_json::to_value(disabled).unwrap();
        assert_eq!(encoded["localPingResponse"], false);
    }

    #[test]
    fn model_profile_exact_answers_default_on_and_round_trip_in_camel_case() {
        let defaulted: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(defaulted.model_profile_exact_answers_enabled);

        let disabled: Config = serde_json::from_value(serde_json::json!({
            "modelProfileExactAnswersEnabled": false
        }))
        .unwrap();
        assert!(!disabled.model_profile_exact_answers_enabled);
        let encoded = serde_json::to_value(disabled).unwrap();
        assert_eq!(encoded["modelProfileExactAnswersEnabled"], false);
    }

    #[test]
    fn cache_policy_defaults_to_thirty_minutes() {
        let config: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(config.cache_metering_enabled);
        assert_eq!(config.cache_default_ttl_secs, 1800);
        assert!(config.cache_auto_without_control);
        assert_eq!(config.cache_capacity, 4096);
        assert_eq!(config.cache_flush_interval_secs, 60);
    }

    #[test]
    fn cache_policy_fields_round_trip_in_camel_case() {
        let value = serde_json::json!({
            "cacheMeteringEnabled": false,
            "cacheDefaultTtlSecs": 300,
            "cacheAutoWithoutControl": false,
            "cacheCapacity": 8192,
            "cacheFlushIntervalSecs": 30
        });
        let config: Config = serde_json::from_value(value).unwrap();
        let encoded = serde_json::to_value(config).unwrap();
        assert_eq!(encoded["cacheMeteringEnabled"], false);
        assert_eq!(encoded["cacheDefaultTtlSecs"], 300);
        assert_eq!(encoded["cacheAutoWithoutControl"], false);
        assert_eq!(encoded["cacheCapacity"], 8192);
        assert_eq!(encoded["cacheFlushIntervalSecs"], 30);
    }

    #[test]
    fn cache_rolling_policy_defaults_are_enabled_and_bounded() {
        let config: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(config.cache_rolling_prefix_enabled);
        assert_eq!(config.cache_rolling_prefix_limit, 8);
    }

    #[test]
    fn cache_rolling_policy_round_trips_camel_case() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "cacheRollingPrefixEnabled": false,
            "cacheRollingPrefixLimit": 16
        }))
        .unwrap();
        assert!(!config.cache_rolling_prefix_enabled);
        assert_eq!(config.cache_rolling_prefix_limit, 16);
        let encoded = serde_json::to_value(config).unwrap();
        assert_eq!(encoded["cacheRollingPrefixEnabled"], false);
        assert_eq!(encoded["cacheRollingPrefixLimit"], 16);
    }

    #[test]
    fn image_budget_defaults_and_round_trips_in_camel_case() {
        let defaulted: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(defaulted.image_budget_enabled);
        assert_eq!(defaulted.image_total_base64_budget_bytes, 819_200);
        assert_eq!(defaulted.image_hard_base64_limit_bytes, 8 * 1024 * 1024);
        assert_eq!(defaulted.image_history_max_dimension, 1_280);
        assert_eq!(defaulted.image_history_jpeg_quality, 72);
        assert_eq!(defaulted.image_retry_history_max_dimension, 960);
        assert_eq!(defaulted.image_retry_history_jpeg_quality, 60);

        let encoded = serde_json::to_value(defaulted).unwrap();
        assert_eq!(encoded["imageBudgetEnabled"], true);
        assert_eq!(encoded["imageTotalBase64BudgetBytes"], 819_200);
        assert_eq!(encoded["imageHardBase64LimitBytes"], 8 * 1024 * 1024);
        assert_eq!(encoded["imageHistoryMaxDimension"], 1_280);
        assert_eq!(encoded["imageHistoryJpegQuality"], 72);
        assert_eq!(encoded["imageRetryHistoryMaxDimension"], 960);
        assert_eq!(encoded["imageRetryHistoryJpegQuality"], 60);
    }

    #[test]
    fn early_stream_handshake_defaults_off() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(!config.early_stream_handshake);
    }

    #[test]
    fn early_stream_handshake_accepts_camel_case_json() {
        let config: Config = serde_json::from_str(r#"{"earlyStreamHandshake":true}"#).unwrap();
        assert!(config.early_stream_handshake);
    }

    /// 阈值默认 85 而非 100：100 是「事后通知」，压缩那时已经没有余量。
    /// 这条测试同时防止有人把默认值改回 100 而不留痕迹。
    #[test]
    fn context_window_signal_threshold_defaults_to_85_pct() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.context_window_signal_threshold_pct, 85.0);
    }

    #[test]
    fn context_window_signal_threshold_accepts_camel_case_json() {
        let config: Config =
            serde_json::from_str(r#"{"contextWindowSignalThresholdPct":90.5}"#).unwrap();
        assert_eq!(config.context_window_signal_threshold_pct, 90.5);
    }

    #[test]
    fn auto_continue_defaults_off_with_three_round_limit() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(!config.auto_continue_enabled);
        assert_eq!(config.auto_continue_max, 3);
        assert!(!config.partial_stream_recovery_enabled);
        assert_eq!(config.partial_stream_recovery_window_ms, 750);
    }

    #[test]
    fn auto_continue_accepts_camel_case_json() {
        let config: Config =
            serde_json::from_str(r#"{"autoContinueEnabled":true,"autoContinueMax":2}"#).unwrap();
        assert!(config.auto_continue_enabled);
        assert_eq!(config.auto_continue_max, 2);
    }

    #[test]
    fn strict_thinking_validation_defaults_to_false() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(!config.strict_thinking_validation);
    }

    #[test]
    fn strict_thinking_validation_can_be_enabled() {
        let config: Config = serde_json::from_str(r#"{"strictThinkingValidation":true}"#).unwrap();
        assert!(config.strict_thinking_validation);
    }

    #[test]
    fn auto_compact_diagnostics_defaults_on_and_round_trips_in_camel_case() {
        let defaulted: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(defaulted.auto_compact_diagnostics_enabled);

        let disabled: Config = serde_json::from_value(serde_json::json!({
            "autoCompactDiagnosticsEnabled": false
        }))
        .unwrap();
        assert!(!disabled.auto_compact_diagnostics_enabled);
        let serialized = serde_json::to_value(disabled).unwrap();
        assert_eq!(serialized["autoCompactDiagnosticsEnabled"], false);
        assert!(serialized.get("auto_compact_diagnostics_enabled").is_none());
    }

    #[test]
    fn empty_user_message_compat_defaults_off_and_round_trips_in_camel_case() {
        let defaulted: Config = serde_json::from_str("{}").unwrap();
        assert!(!defaulted.empty_user_message_compat);

        let enabled: Config = serde_json::from_str(r#"{"emptyUserMessageCompat":true}"#).unwrap();
        assert!(enabled.empty_user_message_compat);
        let encoded = serde_json::to_value(enabled).unwrap();
        assert_eq!(encoded["emptyUserMessageCompat"], true);
    }

    #[test]
    fn endpoint_mode_defaults_to_best_and_round_trips() {
        let defaulted: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(defaulted.endpoint_mode, EndpointMode::Best);

        let manual: Config = serde_json::from_str(r#"{"endpointMode":"manual"}"#).unwrap();
        assert_eq!(manual.endpoint_mode, EndpointMode::Manual);
        let encoded = serde_json::to_value(manual).unwrap();
        assert_eq!(encoded["endpointMode"], "manual");
    }

    #[test]
    fn endpoint_mode_rejects_unknown_values() {
        let error = serde_json::from_str::<Config>(r#"{"endpointMode":"turbo"}"#).unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }
}
