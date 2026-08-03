use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use serde_json::Value;

use crate::admin::key_supplier::capabilities::RegionSource;
use crate::admin::key_supplier::client::{PurchaseContext, SupplierClient, SupplierSnapshot};
use crate::admin::key_supplier::config::{
    MAX_SUPPLIERS, PoolConfigUpdate, PoolConfigView, PoolRuntimeConfig,
    ResolvedSupplierImportPreset, SupplierCommonConfigUpdate, SupplierCommonConfigView,
    SupplierConfigUpdate, SupplierConfigView, SupplierEntryRuntime, SupplierEntryUpdate,
    SupplierEntryView, SupplierRuntimeConfig, is_valid_webhook_token, normalize_supplier_id,
    store_suppliers,
};
use crate::admin::key_supplier::pool::{
    PoolDecision, PoolSkipReason, deficit, select_pool_purchase_count,
};
use crate::admin::key_supplier::store::{
    IncomingSupplierEvent, InsertOutcome, LEGACY_SUPPLIER_ID, ProcessSummary, StoredSupplierEvent,
    SupplierDecisionOutcome, SupplierDecisionQuote, SupplierDecisionRegion, SupplierDecisionResult,
    SupplierDecisionSnapshot, SupplierDecisionSupplier, SupplierDecisionTarget,
    SupplierDecisionTrigger, SupplierEventStore,
};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::region::API_KEY_AUTH_REGION;
use crate::kiro::token_manager::{MultiTokenManager, PoolHealth, SupplierCredentialHealth};
use crate::model::config::{
    Config, KeySupplierPoolConfig, PurchaseRegionMode, SupplierKind, SupplierRegion,
};

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
        /// 供货商侧的开号批次号（`kiroapp-io` 的 `order_id`）。带上它采购就只拉
        /// 这一车产出的 key。对方没给就是 `None`，退化成从公共池子取。
        supplier_batch_id: Option<String>,
        event_region: Option<SupplierRegion>,
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
    /// 不触发采购的通知类事件（例如 `key_revoked_abuse`），以及**任何无法确定
    /// 是到货信号的事件**。落库留痕，但绝不下单。
    ///
    /// 存在的意义是 fail-safe：宽容解析器过去把「不是 test 的一切」都当到货，
    /// 于是 `all_keys_dead` 这类事件会顶着 `max_purchase` 去买一车。
    Notice {
        event_id: String,
        event_type: String,
        message: String,
        quantity: u32,
    },
}

impl IncomingWebhook {
    /// 解析 webhook。按供货商协议分派：`kiro-rs` 走原有严格校验，
    /// `kiro-app` 的推送体格式未文档化，走宽容解析。
    pub fn parse(kind: SupplierKind, body: &[u8]) -> Result<Self, SupplierServiceError> {
        match kind {
            SupplierKind::KiroRs => Self::parse_kiro_rs(body),
            SupplierKind::KiroApp => Self::parse_kiro_app(body),
            SupplierKind::KiroAppIo => Self::parse_kiro_app_io(body),
            // Kiro Drop 与 kiro.ceo 是同一族（`usr-` 令牌 + `/api/my/*`），推送格式相同，
            // 只是各自缺的字段不同，所以共用一个宽容解析。
            // Drop 的推送消息里就写着「自动购买时请把 purchase_order_id 原样传给
            // /api/my/purchase」，文档里那个字段也已经从 `batch_xxx` 变成 32 位 ID。
            //
            // 我一度改成自己派生，理由是「回传对方的 id 导致 500」——**那个判断是错的**。
            // 反证：2026-08-02 01:53 那条到货推送用的就是原样回传，成功导入 1 个；
            // 而失败的几条也是同一个写法。500 的真正来源是对方上游不稳（新文档为此
            // 专门加了 `503 上游 Store 暂时不可用`），与订单号无关。
            SupplierKind::KiroDrop => Self::parse_my_api_family(body, "Kiro Drop 通知", true),
            // kiro.ceo 文档明确要求把推送里的订单号原样作为 `client_order_id` 传回。
            SupplierKind::KiroCeo => Self::parse_my_api_family(body, "kiro.ceo 通知", true),
        }
    }

    /// kiroapp.io 的推送：字段名都是文档化的，所以按名取而不猜。
    ///
    /// - `event`：`new_keys_available` | `all_keys_dead` | `key_revoked_abuse` | `test`
    /// - `event_id`：去重用，每次推送不同（同一事件重试时不变）
    /// - `order_id`：开号批次 id，回传给采购接口就只拉这一车
    /// - `client_order_id`：**对方替我们派生好的幂等键**（批次+收件人确定性派生，
    ///   重推/重启后同值），直接用它，不必自己造
    ///
    /// 只有 `new_keys_available` 触发采购。其它一律 `Notice`——包括不认识的
    /// event 名，宁可漏买也不错买。
    fn parse_kiro_app_io(body: &[u8]) -> Result<Self, SupplierServiceError> {
        let value: Value =
            serde_json::from_slice(body).map_err(|_| SupplierServiceError::InvalidJson)?;
        let object = value
            .as_object()
            .ok_or(SupplierServiceError::InvalidPayload)?;

        let event_name = object
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let message = object
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("kiroapp.io 通知")
            .to_string();
        // event_id 是对方文档明确要求的去重键。缺了就退化成 body 指纹，
        // 保证同一事件重复推送仍映射到同一行。
        let event_id = optional_id(object, &["event_id", "eventId", "id"])
            .unwrap_or_else(|| body_fingerprint(body));

        if event_name.eq_ignore_ascii_case("test") {
            return Ok(Self::Test { event_id, message });
        }

        if !event_name.eq_ignore_ascii_case("new_keys_available") {
            // all_keys_dead / key_revoked_abuse / 未来新增的任何事件都落这里。
            let quantity =
                optional_quantity(object, &["new_keys", "newKeys", "count"]).unwrap_or(0);
            return Ok(Self::Notice {
                event_id,
                event_type: if event_name.is_empty() {
                    "notice".to_owned()
                } else {
                    normalize_event_type(&event_name)
                },
                message,
                quantity,
            });
        }

        let new_keys = optional_quantity(object, &["new_keys", "newKeys", "count"]).unwrap_or(0);
        let supplier_batch_id = optional_id(object, &["order_id", "orderId"]);
        let event_region = optional_region(object, &["region", "zone"]);
        // 优先用对方派生的幂等键：拉取超时后原样重发即命中幂等重放，不会二次扣费。
        // 它必须是 32 hex（采购接口的硬要求），不合格就从 event_id 自己派生。
        let purchase_order_id = optional_id(object, &["client_order_id", "clientOrderId"])
            .filter(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .unwrap_or_else(|| derive_order_id(&event_id));

        Ok(Self::NewKeysAvailable {
            event_id,
            purchase_order_id,
            supplier_batch_id,
            event_region,
            message,
            new_keys,
        })
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
                // kiro-rs 没有批次概念。
                supplier_batch_id: None,
                event_region: None,
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

    /// `/api/my/*` 号商协议族（Kiro Drop、kiro.ceo）的推送。事件名与字段名都和
    /// `kiro-rs` 一致，但 `kiro-rs` 那套「全都必填、id 必须 32 hex」的校验在这一族
    /// 上会把事件整条打回 400，于是采购永远不发生，而日志上只留一句「格式不对」。
    ///
    /// 因此三处放宽（每一处都对应线上实际踩到的形状）：
    ///
    /// 1. **`event_id` 不要求 32 hex**。文档只写「32 位 ID」，没说是 hex；测试/模拟
    ///    推送的 id 往往是 `evt_…` 或带横线的 UUID。它只是去重键，取不到就退化成
    ///    body 指纹，同一车重复推仍映射到同一行。
    /// 2. **`message` 可缺**。它只是给人看的描述，缺了不影响采购决策。
    /// 3. **数量字段可缺**。Kiro Drop 的到货推送根本没有 `new_keys`；实际下单量由
    ///    号池缺口或 `maxPurchase` 夹逼决定，本来就不依赖推送里的数字。
    ///
    /// `purchase_order_id` 能否当幂等键**由 `reuse_pushed_order_id` 决定，不看形状**。
    /// kiro.ceo 文档要求「原样作为 client_order_id 传回」，所以它是 `true`；Kiro Drop
    /// 文档从没这么说——那个字段是它自己的批次号，回传过去会让它的采购接口 500。
    /// 不能当键时就从 `event_id` 派生一个（确定性，重推/重启后同值，幂等仍然成立），
    /// 原值放进 `supplier_batch_id` 留痕。采购客户端强校验 32 hex，
    /// 直接透传非法值会在下单前就报错。
    ///
    /// 「所有已配置 Webhook 的用户都会收到全部事件推送」——Kiro Drop 文档明说的。
    /// 因此 `all_keys_dead` 可能是别人家的号死了，绝不能当采购信号；白名单机制已经
    /// 挡住了，这里只负责如实解析。不认识的事件名一律拒收，避免将来新增事件被误当
    /// 成到货信号。
    fn parse_my_api_family(
        body: &[u8],
        notice: &str,
        reuse_pushed_order_id: bool,
    ) -> Result<Self, SupplierServiceError> {
        let value: Value =
            serde_json::from_slice(body).map_err(|_| SupplierServiceError::InvalidJson)?;
        let object = value
            .as_object()
            .ok_or(SupplierServiceError::InvalidPayload)?;
        let event = object
            .get("event")
            .and_then(Value::as_str)
            .ok_or(SupplierServiceError::InvalidPayload)?;
        let event_id = optional_id(object, &["event_id", "eventId", "id"])
            .unwrap_or_else(|| body_fingerprint(body));
        let message = object
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(notice)
            .to_string();

        match event {
            "new_keys_available" => {
                let raw_order = optional_id(object, &["purchase_order_id", "purchaseOrderId"]);
                // 只有对方文档说了「把这个值原样传回当幂等键」才能这么用。
                // 光看形状（32 hex）不够：Drop 推的就是 32 hex，但那是**它自己的**
                // 批次号，回传给它的采购接口会让它 500（四次自动采购四次 500，而同一
                // 时刻用我们自己生成的订单号手动买就成功）。
                let usable_as_key = reuse_pushed_order_id
                    && raw_order.as_deref().is_some_and(|id| {
                        id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    });
                Ok(Self::NewKeysAvailable {
                    purchase_order_id: if usable_as_key {
                        raw_order.clone().unwrap_or_default()
                    } else {
                        derive_order_id(&event_id)
                    },
                    // 只有当原值当不了幂等键时才需要单独留痕；能当键时它就是订单号本身。
                    supplier_batch_id: (!usable_as_key).then_some(raw_order).flatten(),
                    event_region: optional_region(object, &["zone", "region"]),
                    event_id,
                    message,
                    new_keys: optional_quantity(object, &["new_keys", "newKeys", "count"])
                        .unwrap_or(0),
                })
            }
            "all_keys_dead" => Ok(Self::AllKeysDead {
                event_id,
                message,
                // 对方带 `dead`，缺了也不算错——这个事件不花钱，只留痕。
                dead: optional_quantity(object, &["dead"]).unwrap_or(0),
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
        let event_id = optional_id(
            object,
            &["event_id", "eventId", "id", "batchId", "batch_id"],
        )
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
            // kiroapp.cc 的 claim 不接受批次定向。
            supplier_batch_id: None,
            event_region: None,
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
                supplier_batch_id,
                event_region,
                message,
                new_keys,
            } => IncomingSupplierEvent {
                supplier_id,
                event_id,
                event_type: "new_keys_available".to_string(),
                purchase_order_id: Some(purchase_order_id),
                supplier_batch_id,
                event_region,
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
                supplier_batch_id: None,
                event_region: None,
                message: Some(message),
                quantity: i64::from(dead),
            },
            Self::Test { event_id, message } => IncomingSupplierEvent {
                supplier_id,
                event_id,
                event_type: "test".to_string(),
                purchase_order_id: None,
                supplier_batch_id: None,
                event_region: None,
                message: Some(message),
                quantity: 0,
            },
            Self::Notice {
                event_id,
                event_type,
                message,
                quantity,
            } => IncomingSupplierEvent {
                supplier_id,
                event_id,
                event_type,
                purchase_order_id: None,
                supplier_batch_id: None,
                event_region: None,
                message: Some(message),
                quantity: i64::from(quantity),
            },
        }
    }
}

/// 把对方的 event 名收敛成可安全入库的 event_type：小写，只留字母数字和下划线，
/// 限长。未知事件名直接进 DB 的 `event_type` 列，所以先做一层清洗。
fn normalize_event_type(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if cleaned.trim_matches('_').is_empty() {
        "notice".to_owned()
    } else {
        cleaned
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

/// 凭据备注名的上限，与 `nickname_prefix` 的校验上限一致。
const MAX_NICKNAME_CHARS: usize = 128;

/// 一次触发生效的水位口径。全局号池与逐家水位归约成同一个形状，好让缺口夹逼
/// 只有一份实现——两套并存必然在 `minPurchase` 这类边界上漂移。
struct RestockGate {
    /// 只用于日志和事件原因，让「是哪道闸拦的」不必去翻配置。
    scope: &'static str,
    /// 目标存量。0 视为「配了开关没填数量」，按失效保护不买。
    target: u32,
    /// 该口径下当前可用的采购凭据数。
    usable: usize,
}

/// 事件级自动重试的等待表，第 n 次尝试失败后取第 n-1 项；用完就转 `failed` 终态。
///
/// 与 `client.rs` 的 `RETRY_BACKOFF` 是两层不同的东西：那层管一次处理内的三连
/// 请求（供货商抖动几秒），这层管「供货商整段时间都在 500」（分钟级）。少了这层，
/// 一条到货通知撞上一次上游故障就永久丢单——`failed` 是终态，`claim_next` 只捡
/// `received`，除了人工点重试没有任何东西会再碰它。
const EVENT_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
];

/// 瞬时上游故障压回队列，等 `EVENT_RETRY_DELAYS` 到点再试；重试额度用尽则失败。
///
/// `replayable` = 这次失败的请求可以原样再发一遍。查库存永远可以；下单只有带幂等键
/// 的协议可以（kiro-app 的 `/openapi/claim` 没有订单号，重发就是再扣一次积分）。
///
/// `purchase_count` 是已经发出去的数量，非 `None` 时钉进事件行。重放必须原样重发：
/// `purchase_order_id` 由 `event_id` 派生，同一订单号换数量会让幂等协议返 409
/// （原单已成交、钱扣了、key 没到手），正是这次重试要避免的结果。
fn defer_or_fail(
    event: &StoredSupplierEvent,
    replayable: bool,
    purchase_count: Option<u32>,
    error: crate::admin::key_supplier::client::SupplierError,
) -> Result<ProcessAction, SupplierServiceError> {
    use crate::admin::key_supplier::client::SupplierError;

    // 只有「等一会儿可能就好了」才值得重试。4xx、解码失败、参数错误重放一万次
    // 也是同一个结果；`OutOfStock` / `InsufficientBalance` / `OrderConflict`
    // 在调用方已经归成 skipped，走不到这里。
    let transient = match &error {
        SupplierError::Http { status, .. } => (500..=599).contains(status),
        SupplierError::Network(_) => true,
        // 429 自带 `retry_after`：这是它第一次真正被用上，之前只解析了没人读。
        SupplierError::RateLimited { .. } => true,
        _ => false,
    };
    // 手动采购不自动重试：人就在旁边等结果，十分钟后突然冒出一笔扣款是惊吓。
    // 他看到错误自己再点一次即可。
    if !transient || !replayable || event.event_type != "new_keys_available" {
        return Err(SupplierServiceError::supplier_api(error));
    }
    // `attempts` 在领取时就 +1 了，所以本次是第 `attempts` 次尝试。
    let slot = usize::try_from(event.attempts.max(1) - 1).unwrap_or(usize::MAX);
    let Some(delay) = EVENT_RETRY_DELAYS.get(slot).copied() else {
        return Err(SupplierServiceError::supplier_api(error));
    };
    // 对方明说了要等多久就听它的，别比它更急。
    let delay = match &error {
        SupplierError::RateLimited {
            retry_after: Some(seconds),
            ..
        } => delay.max(Duration::from_secs(*seconds)),
        _ => delay,
    };
    let retry_after = Utc::now()
        + ChronoDuration::from_std(delay).unwrap_or_else(|_| ChronoDuration::seconds(30));
    tracing::warn!(
        supplier = %event.supplier_id,
        event_id = %event.event_id,
        attempts = event.attempts,
        delay_secs = delay.as_secs(),
        pinned_count = ?purchase_count,
        "供货商瞬时故障，事件压回队列等待自动重试"
    );
    Ok(ProcessAction::Deferred {
        retry_after,
        purchase_count,
        error: SupplierServiceError::supplier_api(error),
    })
}

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

fn optional_region(
    object: &serde_json::Map<String, Value>,
    fields: &[&str],
) -> Option<SupplierRegion> {
    fields.iter().find_map(|field| {
        object
            .get(*field)
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<SupplierRegion>().ok())
    })
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

    /// 某家供货商名下凭据的可用统计，用于补货判定。
    ///
    /// `low_quota_threshold` 是「剩余额度 <= 这个数就算不可用」的水位；0 = 不看额度，
    /// 只认封号与 402。
    fn supplier_health(
        &self,
        supplier_id: &str,
        low_quota_threshold: f64,
    ) -> SupplierCredentialHealth;

    /// 全部自动采购凭据的合计可用统计，用于全局号池水位判定。**不区分供货商。**
    ///
    /// `configured_channels` 是已配置供货商的 `sourceChannel` 集合，调用方必须已剔空。
    ///
    /// 默认实现返回全零，让不关心号池的测试替身不必实现。**生产实现必须覆盖**：
    /// 合计为 0 意味着缺口恒等于目标存量，会持续买到上限。这个失效方向很危险，
    /// 靠 `production_importer_overrides_pool_health` 那条契约测试兜住。
    fn pool_health(
        &self,
        _low_quota_threshold: f64,
        _configured_channels: &HashSet<String>,
    ) -> PoolHealth {
        PoolHealth::default()
    }

    /// 注入余额数据源。默认空实现——只有生产的 token manager 实现需要它。
    fn set_quota_source(&self, _source: Arc<dyn QuotaSource>) {}
}

/// 按凭据 id 查剩余额度。实现方是 `AdminService`（它持有余额缓存，后台每 5 分钟刷）。
///
/// 单独抽出来是因为装配顺序：供货商服务先于 `AdminService` 构造，只能事后注入。
pub trait QuotaSource: Send + Sync {
    /// 返回该凭据的剩余额度；缓存缺失或过期返回 `None`。
    ///
    /// 调用方把 `None` 当「还有额度」处理——缺数据时宁可少买。
    fn remaining_quota(&self, credential_id: u64) -> Option<f64>;
}

#[derive(Clone)]
pub struct TokenManagerCredentialImporter {
    token_manager: Arc<MultiTokenManager>,
    /// 余额数据源。装配顺序所限只能事后注入，未注入时额度水位判定退化为不生效
    /// （只认封号与 402）。
    quota_source: Arc<OnceLock<Arc<dyn QuotaSource>>>,
}

impl TokenManagerCredentialImporter {
    pub fn new(token_manager: Arc<MultiTokenManager>) -> Self {
        Self {
            token_manager,
            quota_source: Arc::new(OnceLock::new()),
        }
    }

    /// 按凭据 id 查剩余额度的闭包。没注入数据源时一律返回 `None`，额度水位判定
    /// 自然不生效——缺数据时宁可少买。
    ///
    /// 抽出来是因为逐家统计与全局统计都要它，两处各写一遍容易在「未注入时返回
    /// 什么」上漂移。
    fn quota_lookup(&self) -> impl Fn(u64) -> Option<f64> {
        let quota_source = self.quota_source.get().cloned();
        move |id: u64| {
            quota_source
                .as_ref()
                .and_then(|source| source.remaining_quota(id))
        }
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

    fn supplier_health(
        &self,
        supplier_id: &str,
        low_quota_threshold: f64,
    ) -> SupplierCredentialHealth {
        self.token_manager.supplier_credential_health(
            supplier_id,
            low_quota_threshold,
            &self.quota_lookup(),
        )
    }

    fn pool_health(
        &self,
        low_quota_threshold: f64,
        configured_channels: &HashSet<String>,
    ) -> PoolHealth {
        self.token_manager.pool_credential_health(
            low_quota_threshold,
            configured_channels,
            &self.quota_lookup(),
        )
    }

    /// 只生效一次，重复注入被忽略。
    fn set_quota_source(&self, source: Arc<dyn QuotaSource>) {
        if self.quota_source.set(source).is_err() {
            tracing::debug!("余额数据源已注入过，忽略重复注入");
        }
    }
}

/// 号池当前状态。只读快照，不含按供货商的拆分——水位是全局的，拆分指导不了
/// 任何决策（下单对象恒为推送方那一家）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolStatus {
    pub enabled: bool,
    pub target_count: u32,
    pub low_quota_threshold: u32,
    /// 当前全局可用的采购凭据数，等于 `health.usable`，单独给一份方便前端直读。
    pub global_usable: usize,
    /// 还差几个。`0` 表示池子已满，本次到货不会买。
    pub deficit: u32,
    /// 四类拆分。`dead` 是判死但尚未被保留期清理的号——占位置但不算可用，
    /// 这是「池里有 10 个号怎么可用数只有 3」的答案。
    pub health: SupplierCredentialHealth,
    /// 靠 `supplier_id` 认出来的凭据数。
    pub by_supplier_id: usize,
    /// 靠备注认出来的凭据数。突然掉到 0 基本就是有人改了某家的 `sourceChannel`。
    pub by_legacy_channel: usize,
    /// 当前参与备注匹配的 `sourceChannel` 集合，已排序。
    pub matched_channels: Vec<String>,
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
    /// 全局号池配置。读多写少，与 `suppliers` 同构；写路径同样由
    /// `config_update_lock` 串行化并与它共用同一把锁。
    pool: parking_lot::RwLock<PoolRuntimeConfig>,
    /// 所有供货商共享的凭据导入预设；单家只保存显式覆盖。
    common_import: parking_lot::RwLock<ResolvedSupplierImportPreset>,
}

impl fmt::Debug for KeySupplierService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeySupplierService")
            .field("suppliers", &*self.suppliers.read())
            .field("pool", &*self.pool.read())
            .field("common_import", &*self.common_import.read())
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
            pool: parking_lot::RwLock::new(PoolRuntimeConfig::default()),
            common_import: parking_lot::RwLock::new(ResolvedSupplierImportPreset::default()),
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
            pool: parking_lot::RwLock::new(PoolRuntimeConfig::default()),
            common_import: parking_lot::RwLock::new(ResolvedSupplierImportPreset::default()),
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

    /// 注入余额数据源，让「额度水位」判定生效。
    ///
    /// 必须在 `AdminService` 构造好之后调用——装配顺序上供货商服务先建，那时余额
    /// 缓存还不存在。**不调用的后果是静默降级**：额度水位永远不触发，补货只认封号
    /// 与 402。
    pub fn set_quota_source(&self, source: Arc<dyn QuotaSource>) {
        if let Some(importer) = self.importer.as_ref() {
            importer.set_quota_source(source);
        }
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

    /// 装配启动时读到的号池配置。
    ///
    /// 校验失败时调用方应传 [`PoolRuntimeConfig::poisoned`] 而不是默认值——
    /// 默认值等于关闭，会退回不受限的逐家采购继续花钱。
    pub fn with_pool_config(self, pool: PoolRuntimeConfig) -> Self {
        *self.pool.write() = pool;
        self
    }

    pub fn with_common_import(self, common: ResolvedSupplierImportPreset) -> Self {
        {
            let mut suppliers = self.suppliers.write();
            for entry in suppliers.iter_mut() {
                let resolved = common
                    .resolve(&entry.import_overrides)
                    .expect("启动时已校验供货商导入覆盖");
                resolved.materialize_runtime(&mut entry.settings);
            }
        }
        *self.common_import.write() = common;
        self
    }

    pub fn common_import_view(&self) -> SupplierCommonConfigView {
        SupplierCommonConfigView::from(&*self.common_import.read())
    }

    pub fn update_common_import(
        &self,
        update: SupplierCommonConfigUpdate,
    ) -> Result<SupplierCommonConfigView, SupplierServiceError> {
        let _guard = self.config_update_lock.lock();
        let common = ResolvedSupplierImportPreset::normalize_update(update)
            .map_err(|_| SupplierServiceError::SupplierConfiguration)?;
        let mut entries = self.suppliers.read().clone();
        for entry in &mut entries {
            let resolved = common
                .resolve(&entry.import_overrides)
                .map_err(|_| SupplierServiceError::SupplierConfiguration)?;
            resolved.materialize_runtime(&mut entry.settings);
        }
        self.persist_common_import(&common, &entries)?;
        *self.common_import.write() = common.clone();
        *self.suppliers.write() = entries;
        Ok(SupplierCommonConfigView::from(&common))
    }

    pub fn pool_config(&self) -> PoolRuntimeConfig {
        self.pool.read().clone()
    }

    pub fn pool_view(&self) -> PoolConfigView {
        PoolConfigView::from(&self.pool_config())
    }

    /// 校验 → 落盘 → 换内存。顺序不能反：落盘失败时内存必须保持旧值，
    /// 否则重启后配置会「回滚」而运行中的实例已经按新值花钱了。
    pub fn update_pool(
        &self,
        update: PoolConfigUpdate,
    ) -> Result<PoolConfigView, SupplierServiceError> {
        let _guard = self.config_update_lock.lock();
        let runtime =
            PoolRuntimeConfig::normalize(update).map_err(|_| SupplierServiceError::PoolConfig)?;
        self.persist_pool(&runtime)?;
        *self.pool.write() = runtime.clone();
        Ok(PoolConfigView::from(&runtime))
    }

    /// 把号池配置写回 `config.json`。
    ///
    /// 与 `persist_suppliers` 共用 `config_update_lock`：两者都是「读整个 config →
    /// 改一块 → 写回」，各自加锁只防得住自己并发，防不住彼此——同时改供货商和号池
    /// 会丢一方。这个缺陷在项目里更大范围地存在（`token_manager` 有约 12 处同样写法），
    /// 本特性不修，但至少不新增一个不受同一把锁保护的写入点。
    fn persist_pool(&self, pool: &PoolRuntimeConfig) -> Result<(), SupplierServiceError> {
        let path = self
            .config_path
            .as_ref()
            .ok_or(SupplierServiceError::ConfigPathUnavailable)?;
        let mut config = Config::load(path).map_err(|_| SupplierServiceError::ConfigPersistence)?;
        config.key_supplier_pool = KeySupplierPoolConfig::from(pool);
        config
            .save()
            .map_err(|_| SupplierServiceError::ConfigPersistence)
    }

    /// 号池当前状态：目标存量、全局可用数、缺口、四类不可用拆分、两类识别计数。
    ///
    /// 纯读，不发起任何采购、不产生写操作。这是管理端回答三个问题的唯一入口：
    /// 「池子里明明有号，怎么可用数这么低」（看四类拆分）、「备注匹配还生效吗」
    /// （看两类识别计数）、「我买的号怎么没算进去」（看 `matchedChannels`）。
    pub fn pool_status(&self) -> Result<PoolStatus, SupplierServiceError> {
        let importer = self
            .importer
            .as_ref()
            .ok_or(SupplierServiceError::ImporterUnavailable)?;
        let pool = self.pool_config();
        let mut channels: Vec<String> = self.configured_source_channels().into_iter().collect();
        // 排序让响应稳定：HashSet 的遍历顺序每次都不同，界面上会看到字段乱跳。
        channels.sort();

        let health = importer.pool_health(
            f64::from(pool.low_quota_threshold),
            &channels.iter().cloned().collect(),
        );
        Ok(PoolStatus {
            enabled: pool.enabled,
            target_count: pool.target_count,
            low_quota_threshold: pool.low_quota_threshold,
            global_usable: health.health.usable,
            deficit: deficit(pool.target_count, health.health.usable),
            health: health.health,
            by_supplier_id: health.by_supplier_id,
            by_legacy_channel: health.by_legacy_channel,
            matched_channels: channels,
        })
    }

    /// 参与旧版采购号备注匹配的 `sourceChannel` 集合，**已去重并剔除空串**。
    ///
    /// 剔空是硬要求而不是防御性编程：空串会命中所有无备注凭据，把全部手工号算进
    /// 水位，缺口被顶成 0、自动采购静默失效——而日志里只有一条「号池已达目标存量」，
    /// 极难定位。这个契约只在这一处保证，`classify_membership` 不再重复兜底。
    fn configured_source_channels(&self) -> HashSet<String> {
        self.suppliers
            .read()
            .iter()
            .map(|entry| entry.settings.source_channel.trim().to_owned())
            .filter(|channel| !channel.is_empty())
            .collect()
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
                import_overrides: None,
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
        let common = self.common_import.read().clone();
        let runtime = SupplierEntryRuntime::normalize_update_with_common(
            id.as_deref(),
            update,
            existing.as_ref(),
            &common,
        )
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

    fn persist_common_import(
        &self,
        common: &ResolvedSupplierImportPreset,
        entries: &[SupplierEntryRuntime],
    ) -> Result<(), SupplierServiceError> {
        let path = self
            .config_path
            .as_ref()
            .ok_or(SupplierServiceError::ConfigPathUnavailable)?;
        let mut config = Config::load(path).map_err(|_| SupplierServiceError::ConfigPersistence)?;
        config.key_supplier_common = crate::model::config::KeySupplierCommonConfig::from(common);
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
        let client = SupplierClient::with_kind(
            &entry.settings.base_url,
            &entry.settings.api_key,
            entry.kind,
        )
        .map_err(|_| SupplierServiceError::SupplierConfiguration)?;
        // 服务层测试里造 5xx 是常事，真按生产退避表等 4 秒会把整套测试拖垮。
        // 退避本身在 `client.rs` 有专门用例断言。
        #[cfg(test)]
        let client = client.without_backoff();
        Ok(client)
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
        let (overview_region, _) = requested_purchase_region(&entry.settings, None);
        let snapshot = self
            .client_for(&entry)?
            .snapshot_for(overview_region)
            .await
            .map_err(SupplierServiceError::supplier_api)?;
        let webhook_registered = match (&snapshot.webhook_url, self.supplier_callback_url(id).ok())
        {
            (Some(registered), Some(callback)) => *registered == callback,
            // kiro-app 读不到对方登记的回调地址，注册状态未知。
            _ => false,
        };
        let credential_health = self
            .importer
            .as_ref()
            .map(|importer| {
                importer.supplier_health(&entry.id, f64::from(entry.settings.low_quota_threshold))
            })
            .unwrap_or_default();
        Ok(SupplierOverview {
            supplier_id: entry.id,
            kind: entry.kind.as_str(),
            snapshot,
            webhook_registered,
            credential_health,
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

        // 解析失败必须留痕。以前这里直接 `?` 返回 400，应用侧一行日志都没有：
        // 只能从 nginx 访问日志看到 `400 72`，再靠猜是哪条校验不满足。
        // 只记字段名、事件名和长度——载荷不含 key 明文，但也不必把值写进日志。
        let webhook = IncomingWebhook::parse(entry.kind, body.as_ref()).inspect_err(|error| {
            let fields = serde_json::from_slice::<Value>(body.as_ref())
                .ok()
                .and_then(|value| {
                    value
                        .as_object()
                        .map(|object| object.keys().cloned().collect::<Vec<_>>().join(","))
                })
                .unwrap_or_else(|| "<not a json object>".to_owned());
            tracing::warn!(
                supplier = %entry.id,
                kind = %entry.kind,
                reason = processing_error_kind(error),
                body_len = body.as_ref().len(),
                fields = %fields,
                "供货商 webhook 载荷被拒收"
            );
        })?;
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
            // 手动采购不定向批次：从对方公共库存里取。
            supplier_batch_id: None,
            event_region: None,
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
        let decision_entry = self.supplier(&event.supplier_id);
        let decision_pool = self.pool_config();
        match self.execute_claimed(&event).await {
            Ok(ProcessAction::Complete(summary)) => {
                let summary = attach_decision_snapshot(
                    summary,
                    &event,
                    decision_entry.as_ref(),
                    &decision_pool,
                    SupplierDecisionOutcome::Succeeded,
                    None,
                );
                let stored_summary = summary.clone();
                self.run_store_operation(move |store| store.complete(event.id, stored_summary))
                    .await?;
                Ok(summary)
            }
            Ok(ProcessAction::Skip) => {
                let summary = attach_decision_snapshot(
                    empty_summary(),
                    &event,
                    decision_entry.as_ref(),
                    &decision_pool,
                    SupplierDecisionOutcome::Skipped,
                    Some("purchase skipped"),
                );
                self.run_store_operation(move |store| {
                    store.skip_with_summary(event.id, Some("purchase skipped"), summary)
                })
                .await?;
                Ok(empty_summary())
            }
            Ok(ProcessAction::SkipWithReason { reason, summary }) => {
                let summary = attach_decision_snapshot(
                    summary,
                    &event,
                    decision_entry.as_ref(),
                    &decision_pool,
                    SupplierDecisionOutcome::Skipped,
                    Some(&reason),
                );
                self.run_store_operation(move |store| {
                    store.skip_with_summary(event.id, Some(&reason), summary)
                })
                .await?;
                Ok(empty_summary())
            }
            Ok(ProcessAction::Failed { summary, error }) => {
                let persistence_error = self.sanitize_for(&event.supplier_id, &error);
                let summary = attach_decision_snapshot(
                    summary,
                    &event,
                    decision_entry.as_ref(),
                    &decision_pool,
                    SupplierDecisionOutcome::Failed,
                    Some(&persistence_error),
                );
                self.run_store_operation(move |store| {
                    store.fail_with_summary(event.id, summary, &persistence_error)
                })
                .await?;
                Err(error)
            }
            Ok(ProcessAction::Deferred {
                retry_after,
                purchase_count,
                error,
            }) => {
                let persistence_error = self.sanitize_for(&event.supplier_id, &error);
                self.run_store_operation(move |store| {
                    store.defer(event.id, retry_after, purchase_count, &persistence_error)
                })
                .await?;
                Err(error)
            }
            Err(error) => {
                let persistence_error = self.sanitize_for(&event.supplier_id, &error);
                let summary = attach_decision_snapshot(
                    ProcessSummary {
                        failed_count: 1,
                        ..Default::default()
                    },
                    &event,
                    decision_entry.as_ref(),
                    &decision_pool,
                    SupplierDecisionOutcome::Failed,
                    Some(&persistence_error),
                );
                self.run_store_operation(move |store| {
                    store.fail_with_summary(event.id, summary, &persistence_error)
                })
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
        // 采购白名单：只有这两种事件会花钱。其它一切——`all_keys_dead`、`test`、
        // `key_revoked_abuse`、以及供货商将来新增的任何事件名——都只留痕不下单。
        //
        // 这里用白名单而不是「排除已知的几种」是刻意的：过去宽容解析器把
        // 「不是 test 的一切」都当到货信号，`all_keys_dead` 这种事件会顶着
        // `max_purchase` 去买一车。默认必须是不买。
        if !matches!(
            event.event_type.as_str(),
            "new_keys_available" | "manual_purchase"
        ) {
            return Ok(ProcessAction::Complete(empty_summary()));
        }

        // 号池闸的水位快照。所有 skipped / failed 出口都带上它——「为什么没买」
        // 正是这三个数要回答的问题，只在成功时记录等于没记录。号池闸未启用时保持
        // 全 `None`，落库时走 `COALESCE` 不会覆盖任何已有值。
        let mut pool_snapshot = ProcessSummary::default();

        // 事件带 supplier_id，处理时按它找回供货商；供货商被删掉就跳过而不是报错。
        let entry = self
            .supplier(&event.supplier_id)
            .ok_or(SupplierServiceError::SupplierNotFound)?;
        let runtime = &entry.settings;
        let (mut requested_region, mut requested_region_source) =
            requested_purchase_region(runtime, event.event_region);
        pool_snapshot.decision_requested_region = requested_region;
        pool_snapshot.decision_requested_region_source = requested_region_source;
        if event.event_type == "new_keys_available" && (!runtime.auto_purchase || !entry.enabled) {
            return Ok(ProcessAction::Skip);
        }
        let importer = self
            .importer
            .as_ref()
            .ok_or(SupplierServiceError::ImporterUnavailable)?;

        // 上一轮已经把这个数量发出去过（瞬时故障压回队列时钉下来的）。必须原样重放：
        // `purchase_order_id` 由 `event_id` 派生，同一订单号换数量会让幂等协议返 409
        // （原单已成交、钱扣了、key 没到手）。所以钉住的数量优先于所有水位判定——
        // 可能因此多买一个，但换来的是把可能已经付过钱的那一单取回来。
        let pinned = event
            .purchase_count
            .and_then(|count| u32::try_from(count).ok())
            .filter(|count| *count > 0);

        // 号池配置快照。整次触发用同一份，避免中途被改导致前后判定不一致。
        let pool = self.pool_config();

        // 水位闸。两种口径互斥，但**语义完全一致**：填的数字都是「目标存量」，任一
        // 供货商推来到货通知时按 `目标 - 当前可用` 的缺口补齐，只向推送方下单。
        //
        //   - 全局号池：可用数 = 所有自动采购来的号合计
        //   - 逐家水位：可用数 = 只算该供货商名下的号
        //
        // 因此归约成同一个 `(target, usable)` 交给 `select_pool_purchase_count`，夹逼实现
        // 只有一份。逐家那道闸以前是「可用数 > 水位就跳过」，而数量另外由推送里的数字
        // 决定——于是同一家连推三次就买三次。归约成缺口后这不可能发生：第一次买完
        // usable 就到 target，后续推送的缺口是 0。
        //
        // 「不可用」= 封号 + 额度耗尽 + 剩余额度跌到水位以下。只认封号是不够的：
        // 号没被封但额度跑光了，对流量来说一样是废的。
        //
        // 只对 webhook 到货生效；手动采购是人明确要买，不该被拦。钉住数量的重放同样
        // 绕过——那一单可能已经付过钱，取回它优先于水位。
        let gate = if event.event_type != "new_keys_available" || pinned.is_some() {
            None
        } else if pool.enabled {
            let channels = self.configured_source_channels();
            let health = importer.pool_health(f64::from(pool.low_quota_threshold), &channels);
            tracing::info!(
                supplier = %entry.id,
                scope = "全局号池",
                target = pool.target_count,
                usable = health.health.usable,
                dead = health.health.dead,
                quota_exhausted = health.health.quota_exhausted,
                low_quota = health.health.low_quota,
                by_supplier_id = health.by_supplier_id,
                by_legacy_channel = health.by_legacy_channel,
                "水位闸读到的存量"
            );
            Some(RestockGate {
                scope: "全局号池",
                target: pool.target_count,
                usable: health.health.usable,
            })
            .inspect(|gate| {
                pool_snapshot.decision_gate_scope = Some(gate.scope.to_owned());
                pool_snapshot.decision_target = Some(gate.target);
                pool_snapshot.decision_health = Some(health.health);
            })
        } else if runtime.restock_only_when_exhausted {
            let health =
                importer.supplier_health(&entry.id, f64::from(runtime.low_quota_threshold));
            tracing::info!(
                supplier = %entry.id,
                scope = "供货商水位",
                target = runtime.target_usable,
                usable = health.usable,
                dead = health.dead,
                quota_exhausted = health.quota_exhausted,
                low_quota = health.low_quota,
                "水位闸读到的存量"
            );
            Some(RestockGate {
                scope: "供货商水位",
                target: runtime.target_usable,
                usable: health.usable,
            })
            .inspect(|gate| {
                pool_snapshot.decision_gate_scope = Some(gate.scope.to_owned());
                pool_snapshot.decision_target = Some(gate.target);
                pool_snapshot.decision_health = Some(health);
            })
        } else {
            None
        };

        let (count, client) = match (event.event_type.as_str(), pinned) {
            // 重放已发出的那一单：跳过库存与水位判定，只求请求体与上次逐字节相同。
            ("new_keys_available", Some(count)) => {
                tracing::warn!(
                    supplier = %entry.id,
                    event_id = %event.event_id,
                    attempts = event.attempts,
                    count,
                    "重放上一轮已发出的采购数量（订单号相同，命中幂等即不会重复扣费）"
                );
                (count, self.client_for(&entry)?)
            }
            ("new_keys_available", None) => {
                let client = self.client_for(&entry)?;

                // 缺口说了算，推送带的数量只留痕不作依据。
                //
                // 缺口必须在查库存**之前**算完：查库存是一次 HTTP 往返，存量已达标时
                // 不该把请求打出去。这也是「缺口为 0 不发任何请求」那条测试要守的。
                if let Some(gate) = &gate {
                    let gap = deficit(gate.target, gate.usable);
                    pool_snapshot.pool_usable = Some(gate.usable as i64);
                    pool_snapshot.pool_deficit = Some(i64::from(gap));

                    if gate.target == 0 || gap == 0 {
                        let reason = if gate.target == 0 {
                            // 失效保护：有人开了开关却没填数量，宁可不买。
                            PoolSkipReason::TargetUnavailable
                        } else {
                            PoolSkipReason::TargetReached
                        };
                        pool_snapshot.pool_requested = Some(0);
                        tracing::info!(
                            supplier = %entry.id,
                            scope = gate.scope,
                            target = gate.target,
                            usable = gate.usable,
                            reason = reason.as_str(),
                            "水位闸拦下本次到货采购"
                        );
                        return Ok(ProcessAction::SkipWithReason {
                            reason: reason.as_str().to_owned(),
                            summary: pool_snapshot,
                        });
                    }
                }

                let event_count = u32::try_from(event.quantity)
                    .map_err(|_| SupplierServiceError::InvalidEvent)?;
                // kiro-app 的库存通知自带 count，官方文档明确建议「直接尝试领取，
                // 不要先查 /openapi/stock」——查询和领取不是一个事务，多一次往返
                // 只会把货让给别人。kiro-rs 没这个说法，保持先查库存夹逼。
                // 下单前问到的单价，`None` = 这家报不出价，或这条路径没去问。
                let mut unit_price = None;
                let available = match entry.kind {
                    // kiroapp-io 同理不先查库存：它的 purchase 本身就处理部分成交
                    // （买得起多少就成交多少），先查一次只是把货让给别人。
                    //
                    // 但配了单价上限就必须问一次——上限的前提是知道价钱。多一次往返是
                    // 用户开这个开关时明确接受的代价。
                    SupplierKind::KiroApp | SupplierKind::KiroAppIo
                        if runtime.max_unit_price <= 0.0 =>
                    {
                        u64::from(runtime.max_purchase)
                    }
                    SupplierKind::KiroApp | SupplierKind::KiroAppIo => {
                        match client.purchase_quote_for(requested_region).await {
                            Ok(quote) => {
                                unit_price = quote.unit_price;
                                if let Some(region) = quote
                                    .zone
                                    .as_deref()
                                    .and_then(|zone| zone.parse::<SupplierRegion>().ok())
                                {
                                    requested_region = Some(region);
                                    requested_region_source = Some(RegionSource::Request);
                                }
                                quote.stock
                            }
                            Err(error) => return defer_or_fail(event, true, None, error),
                        }
                    }
                    // Drop 的推送不带数量，库存必须查（否则只能盲发）。它的库存来自
                    // `/api/status` 的 `keys_stock`，不是 kiro-rs 那个 `/api/my/stock`。
                    // kiro.ceo 的文档也建议先查 `/api/my/stock` 的 `max` 再提货。
                    SupplierKind::KiroRs | SupplierKind::KiroDrop | SupplierKind::KiroCeo => {
                        match client.purchase_quote_for(requested_region).await {
                            Ok(quote) => {
                                unit_price = quote.unit_price;
                                if let Some(region) = quote
                                    .zone
                                    .as_deref()
                                    .and_then(|zone| zone.parse::<SupplierRegion>().ok())
                                {
                                    requested_region = Some(region);
                                    requested_region_source = Some(RegionSource::Request);
                                }
                                quote.stock
                            }
                            // 查库存是 GET，没花钱，重放永远安全。
                            Err(error) => return defer_or_fail(event, true, None, error),
                        }
                    }
                };
                pool_snapshot.decision_vendor_stock = Some(available);
                pool_snapshot.decision_quoted_unit_price = unit_price;
                pool_snapshot.decision_requested_region = requested_region;
                pool_snapshot.decision_requested_region_source = requested_region_source;

                // 单价闸。放在库存夹逼**之后、下单之前**：能拿到价就比，拿不到就不买。
                //
                // 配了上限却问不到单价时跳过而不是放行，方向和其它闸一致——「宁可少买」。
                // 把 `None` 当免费会让这道闸在最需要它的时候（对方不报价）完全失效。
                if runtime.max_unit_price > 0.0 {
                    let over_budget = match unit_price {
                        Some(price) => price > runtime.max_unit_price,
                        None => true,
                    };
                    if over_budget {
                        let reason = if unit_price.is_some() {
                            PoolSkipReason::UnitPriceTooHigh
                        } else {
                            PoolSkipReason::UnitPriceUnknown
                        };
                        pool_snapshot.pool_requested = Some(0);
                        tracing::info!(
                            supplier = %entry.id,
                            unit_price = ?unit_price,
                            max_unit_price = runtime.max_unit_price,
                            reason = reason.as_str(),
                            "单价闸拦下本次到货采购"
                        );
                        return Ok(ProcessAction::SkipWithReason {
                            reason: reason.as_str().to_owned(),
                            summary: pool_snapshot,
                        });
                    }
                }

                match &gate {
                    Some(gate) => {
                        match select_pool_purchase_count(
                            gate.target,
                            gate.usable,
                            available,
                            runtime.max_purchase,
                            runtime.min_purchase,
                        ) {
                            PoolDecision::Purchase(count) => {
                                pool_snapshot.pool_requested = Some(i64::from(count));
                                tracing::info!(
                                    supplier = %entry.id,
                                    scope = gate.scope,
                                    target = gate.target,
                                    usable = gate.usable,
                                    count,
                                    "水位闸放行本次到货采购"
                                );
                                (count, client)
                            }
                            PoolDecision::Skip(reason) => {
                                pool_snapshot.pool_requested = Some(0);
                                tracing::info!(
                                    supplier = %entry.id,
                                    scope = gate.scope,
                                    target = gate.target,
                                    usable = gate.usable,
                                    available,
                                    min_purchase = runtime.min_purchase,
                                    max_purchase = runtime.max_purchase,
                                    reason = reason.as_str(),
                                    "水位闸拦下本次到货采购"
                                );
                                return Ok(ProcessAction::SkipWithReason {
                                    reason: reason.as_str().to_owned(),
                                    summary: pool_snapshot,
                                });
                            }
                        }
                    }
                    None => {
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
                }
            }
            ("manual_purchase", _) => {
                let count = u32::try_from(event.quantity)
                    .map_err(|_| SupplierServiceError::InvalidEvent)?;
                let client = self.client_for(&entry)?;
                (count, client)
            }
            _ => unreachable!("event type was validated before purchase"),
        };
        pool_snapshot.decision_requested_count = Some(count);

        // 发请求前再校验一次「不超过本次缺口」。上面算完到这里之间没有 await，
        // 但这道校验是廉价的最后一关：任何将来插进来的逻辑一旦把数量放大，
        // 这里就会拦住而不是让它变成一笔真实扣款。
        if pool_snapshot
            .pool_deficit
            .is_some_and(|gap| i64::from(count) > gap)
        {
            tracing::error!(
                supplier = %entry.id,
                count,
                deficit = ?pool_snapshot.pool_deficit,
                "采购量超过本次号池缺口，放弃本笔请求"
            );
            pool_snapshot.pool_requested = Some(0);
            return Ok(ProcessAction::SkipWithReason {
                reason: "采购量超过号池缺口，已放弃本笔请求".to_owned(),
                summary: pool_snapshot,
            });
        }

        let order_id = event
            .purchase_order_id
            .as_deref()
            .ok_or(SupplierServiceError::InvalidEvent)?;
        // 只有 batch 模式才用供货商批次号定向拉取。fixed 模式必须忽略事件里的批次，
        // 否则客户端会优先发送 order_id，静默覆盖用户配置的采购区域。
        let purchase = match client
            .purchase_with_context(
                count,
                order_id,
                PurchaseContext {
                    supplier_batch_id: if runtime.purchase_region_mode == PurchaseRegionMode::Batch
                    {
                        event.supplier_batch_id.as_deref()
                    } else {
                        None
                    },
                    requested_region,
                    region_source: requested_region_source,
                },
            )
            .await
        {
            Ok(purchase) => purchase,
            // 被别人抢完了：正常竞争结果，记 skipped 而不是 failed，也不给重试按钮
            // （重试只会再抢一次空气，还可能在真有货时变成额外下单）。
            Err(crate::admin::key_supplier::client::SupplierError::OutOfStock) => {
                return Ok(ProcessAction::SkipWithReason {
                    reason: "库存已被抢完".to_owned(),
                    summary: pool_snapshot,
                });
            }
            // 有货但积分不够：重试也买不到，得先充值。记 skipped 并说明原因，
            // 免得当成对方故障去查日志。
            Err(crate::admin::key_supplier::client::SupplierError::InsufficientBalance(_)) => {
                return Ok(ProcessAction::SkipWithReason {
                    reason: "供货商积分不足，需充值".to_owned(),
                    summary: pool_snapshot,
                });
            }
            // 原单已成交但参数对不上：钱已经扣、key 已经出货，我们没拿到。
            // 记 failed 会让人反复点 retry 而每次都撞同一个 409，付过的钱一直挂在对方账上；
            // 所以记 skipped 并把该做的事写进原因里。这条必须能在日志里被告警抓到。
            Err(crate::admin::key_supplier::client::SupplierError::OrderConflict(detail)) => {
                tracing::error!(
                    supplier = %entry.id,
                    event_id = %event.event_id,
                    order_id = %order_id,
                    detail = %detail,
                    "供货商返回 409：该订单号已成交但参数不一致，积分已扣但未取到 key，需人工核对"
                );
                return Ok(ProcessAction::SkipWithReason {
                    reason: format!(
                        "订单号已成交但参数不一致：积分已扣，需到供货商订单历史核对并补取 key（对方原话：{detail}）"
                    ),
                    summary: pool_snapshot,
                });
            }
            // 409 但含义不止一种（kiro.ceo：库存不足 / 已达最大持有库存上限 / 幂等键撞单）。
            // 前两种一分钱没扣，报成「积分已扣，去订单历史补取 key」会让人去查一条根本
            // 不存在的订单。只有对方原文能分辨，所以原样写进事件记录。
            //
            // 本次订单号是由 event_id 派生/透传的、每条事件各不相同，从没用过的号不可能
            // 撞单，所以实际几乎总是前两种——但代码不去猜，把话摆出来让人判断。
            Err(crate::admin::key_supplier::client::SupplierError::StateConflict(detail)) => {
                tracing::warn!(
                    supplier = %entry.id,
                    event_id = %event.event_id,
                    order_id = %order_id,
                    detail = %detail,
                    "供货商返回 409 状态冲突（库存不足 / 已达持有上限 / 幂等键撞单），未下单"
                );
                return Ok(ProcessAction::SkipWithReason {
                    reason: format!(
                        "供货商状态冲突，未成交：可能是库存不足或已达对方的最大持有库存上限（对方原话：{detail}）"
                    ),
                    summary: pool_snapshot,
                });
            }
            // 瞬时故障（5xx / 网络 / 429）压回队列自动重试，而不是直接进 failed 终态。
            // 重放安全性取决于协议是否有幂等键：kiro-app 的 claim 没有订单号，
            // 5xx 之后我们分不清积分扣没扣，只能停在失败让人工处理。
            Err(error) => {
                return defer_or_fail(
                    event,
                    entry.kind.purchase_is_idempotent(),
                    Some(count),
                    error,
                );
            }
        };
        // 钱的数字先进 summary 再做导入：导入失败走 `Failed { summary }`，
        // 金额必须跟着一起落库，否则预算累计会把这单算成 0 花费。
        let mut summary = ProcessSummary {
            purchased_count: i64::from(purchase.purchased),
            total_debit: purchase
                .points_cost
                .and_then(|cost| i64::try_from(cost).ok()),
            unit_price: purchase.unit_price,
            supplier_order_id: purchase.supplier_order_id.clone(),
            replayed: purchase.replayed,
            // 水位快照跟着成功与「买到了但导入失败」两条路径一起落库，
            // 解释「这次为什么买了这么多」。
            ..pool_snapshot
        };
        summary.decision_actual_region = purchase.actual_region;
        summary.decision_actual_region_source = purchase.region_source;
        let purchase_response_region = (purchase.region_source
            == Some(RegionSource::PurchaseResponse))
        .then_some(purchase.actual_region)
        .flatten();
        let credential_region = resolve_credential_region(
            purchase_response_region,
            event.event_region,
            requested_region,
            runtime,
        );
        let mut import_failed = false;
        for (index, key) in purchase.keys.into_iter().enumerate() {
            let price = key.price();
            let credential = credential_from_supplier_key(
                key.into_inner(),
                &entry.id,
                &entry.name,
                runtime,
                order_id,
                index + 1,
                price,
                &credential_region.api_region,
            );
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
    ///
    /// `summary` 携带解释性字段（号池水位快照、已知的金额）。跳过路径同样要落库：
    /// 「为什么没买」正是那些数字要回答的问题。
    /// `reason` 是 `String` 而不是 `&'static str`：供货商回的原文常常是唯一能说清
    /// 「到底为什么没买」的东西。kiro.ceo 的 409 同时表示库存不足 / 已达最大持有上限 /
    /// 幂等键撞单，只有它的中文原文能分辨，丢掉就只剩一句猜出来的固定文案。
    SkipWithReason {
        reason: String,
        summary: ProcessSummary,
    },
    Failed {
        summary: ProcessSummary,
        error: SupplierServiceError,
    },
    /// 瞬时上游故障：压回队列，`retry_after` 到点自动再试一次。
    ///
    /// 不带 `summary`：一次都没买成、也没导入，事件行里已有的金额与水位快照
    /// 应当原样留着等下一轮，不该被一份空 summary 抹掉。
    Deferred {
        retry_after: chrono::DateTime<Utc>,
        purchase_count: Option<u32>,
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
    ProcessSummary::default()
}

fn decision_snapshot(
    event: &StoredSupplierEvent,
    entry: Option<&SupplierEntryRuntime>,
    pool: &PoolRuntimeConfig,
    summary: &ProcessSummary,
    outcome: SupplierDecisionOutcome,
    reason: Option<&str>,
) -> SupplierDecisionSnapshot {
    let settings = entry.map(|entry| &entry.settings);
    let target = summary.decision_target.or_else(|| {
        summary
            .pool_usable
            .zip(summary.pool_deficit)
            .and_then(|(usable, deficit)| u32::try_from(usable.saturating_add(deficit)).ok())
    });
    SupplierDecisionSnapshot {
        version: 1,
        outcome,
        reason: reason.map(str::to_owned),
        trigger: SupplierDecisionTrigger {
            event_type: event.event_type.clone(),
            quantity: event.quantity,
            attempt: event.attempts,
        },
        supplier: SupplierDecisionSupplier {
            id: event.supplier_id.clone(),
            kind: entry.map(|entry| entry.kind),
            enabled: entry.map(|entry| entry.enabled),
            auto_purchase: settings.map(|settings| settings.auto_purchase),
            min_purchase: settings.map(|settings| settings.min_purchase),
            max_purchase: settings.map(|settings| settings.max_purchase),
        },
        target: SupplierDecisionTarget {
            scope: summary.decision_gate_scope.clone(),
            configured: target,
            credited_at_decision: summary.pool_usable,
            deficit: summary.pool_deficit,
            requested: summary
                .pool_requested
                .or(summary.decision_requested_count.map(i64::from)),
            reached: summary.pool_deficit.map(|deficit| deficit == 0),
            health: summary.decision_health,
            global_pool_enabled: pool.enabled,
        },
        quote: SupplierDecisionQuote {
            vendor_stock: summary.decision_vendor_stock,
            unit_price: summary.decision_quoted_unit_price.or(summary.unit_price),
            max_unit_price: settings.map(|settings| settings.max_unit_price),
        },
        region: SupplierDecisionRegion {
            mode: settings.map(|settings| settings.purchase_region_mode),
            configured_purchase_region: settings.and_then(|settings| settings.purchase_region),
            webhook_region: event.event_region,
            requested_region: summary.decision_requested_region,
            requested_region_source: summary.decision_requested_region_source,
            actual_region: summary.decision_actual_region,
            actual_region_source: summary.decision_actual_region_source,
            credential_api_region_fallback: settings
                .map(|settings| settings.credential_api_region_fallback.clone()),
        },
        result: SupplierDecisionResult {
            purchased: summary.purchased_count,
            imported: summary.imported_count,
            duplicate: summary.duplicate_count,
            failed: summary.failed_count,
            total_debit: summary.total_debit,
            supplier_order_id: summary.supplier_order_id.clone(),
            replayed: summary.replayed,
        },
    }
}

fn attach_decision_snapshot(
    mut summary: ProcessSummary,
    event: &StoredSupplierEvent,
    entry: Option<&SupplierEntryRuntime>,
    pool: &PoolRuntimeConfig,
    outcome: SupplierDecisionOutcome,
    reason: Option<&str>,
) -> ProcessSummary {
    summary.decision_snapshot = Some(decision_snapshot(
        event, entry, pool, &summary, outcome, reason,
    ));
    summary
}

/// 自动采购来的号叫什么名字：`drop-a19fe933-1` = **来源 + 订单号片段 + 序号**。
///
/// 来源默认取供货商名——不配任何东西也能一眼看出这号是谁家的，这是以前缺的。
/// 配了 `nickname_prefix` 就把它当标签附在供货商名后面，绝不能替换供货商名。
///
/// 后缀刻意用订单号片段而不是新掷的随机串。它看起来一样随机（自动采购的订单号由
/// `event_id` 派生、手动是 uuid），但能一眼回查到是哪一单买的。丢掉它等于删掉凭据与
/// 采购事件之间唯一的线索，出了「钱扣了号没到」只能靠时间戳猜。
fn supplier_credential_nickname(
    supplier_name: &str,
    runtime: &SupplierRuntimeConfig,
    order_id: &str,
    index: usize,
) -> String {
    let supplier = supplier_name.trim_matches(['-', '_', ' ', ':']);
    let label = runtime.nickname_prefix.trim_matches(['-', '_', ' ', ':']);
    let source = if label.is_empty() || label.eq_ignore_ascii_case(supplier) {
        supplier.to_owned()
    } else {
        format!("{supplier}-{label}")
    };
    let trace: String = order_id.chars().take(8).collect();
    let suffix = format!("-{trace}-{index}");
    let room = MAX_NICKNAME_CHARS.saturating_sub(suffix.chars().count());
    format!("{}{suffix}", source.chars().take(room).collect::<String>())
}

fn credential_from_supplier_key(
    key: String,
    supplier_id: &str,
    supplier_name: &str,
    runtime: &SupplierRuntimeConfig,
    order_id: &str,
    index: usize,
    purchase_price: Option<f64>,
    api_region: &str,
) -> KiroCredentials {
    let nickname_source = if supplier_name.trim().is_empty() {
        supplier_id
    } else {
        supplier_name
    };
    let nickname = supplier_credential_nickname(nickname_source, runtime, order_id, index);
    KiroCredentials {
        auth_method: Some("api_key".to_owned()),
        kiro_api_key: Some(key),
        auth_region: Some(API_KEY_AUTH_REGION.to_owned()),
        api_region: Some(api_region.to_owned()),
        rpm_limit: runtime.rpm_limit,
        priority: runtime.priority,
        groups: runtime.groups.clone(),
        source_channel: Some(runtime.source_channel.clone()),
        // 机器可判定的归属，用于「这家全死了才补货」的判定。source_channel 是用户
        // 可编辑的备注，两家填一样就无法归属，所以另起一个字段只由代码写。
        supplier_id: Some(supplier_id.to_owned()),
        delete_on_forbidden: runtime.auto_delete_forbidden,
        // 这个号买来花了多少。和 `added_at` / `died_at` 一起就能算「每存活小时成本」——
        // 阶梯定价下必须按 key 记，按单摊会把便宜的和贵的混成一个假均价。
        purchase_price,
        nickname: Some(nickname),
        ..Default::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedCredentialRegion {
    api_region: String,
    source: RegionSource,
}

fn resolve_credential_region(
    purchase_response_region: Option<SupplierRegion>,
    webhook_region: Option<SupplierRegion>,
    requested_region: Option<SupplierRegion>,
    runtime: &SupplierRuntimeConfig,
) -> ResolvedCredentialRegion {
    if let Some(region) = purchase_response_region {
        return ResolvedCredentialRegion {
            api_region: region.as_api_region().to_owned(),
            source: RegionSource::PurchaseResponse,
        };
    }
    if let Some(region) = webhook_region {
        return ResolvedCredentialRegion {
            api_region: region.as_api_region().to_owned(),
            source: RegionSource::Webhook,
        };
    }
    if let Some(region) = requested_region {
        return ResolvedCredentialRegion {
            api_region: region.as_api_region().to_owned(),
            source: RegionSource::Request,
        };
    }
    ResolvedCredentialRegion {
        api_region: runtime.credential_api_region_fallback.clone(),
        source: RegionSource::ConfigFallback,
    }
}

fn requested_purchase_region(
    runtime: &SupplierRuntimeConfig,
    event_region: Option<SupplierRegion>,
) -> (Option<SupplierRegion>, Option<RegionSource>) {
    match runtime.purchase_region_mode {
        PurchaseRegionMode::Omit | PurchaseRegionMode::BestAvailable => (None, None),
        PurchaseRegionMode::Fixed => (
            runtime.purchase_region,
            runtime.purchase_region.map(|_| RegionSource::Request),
        ),
        PurchaseRegionMode::Webhook | PurchaseRegionMode::Batch => match event_region {
            Some(region) => (Some(region), Some(RegionSource::Webhook)),
            None => (
                runtime
                    .credential_api_region_fallback
                    .parse::<SupplierRegion>()
                    .ok(),
                Some(RegionSource::ConfigFallback),
            ),
        },
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
    /// 本地号池里这家供货商名下凭据的存活情况。补货闸就是按 `alive` 判定的，
    /// 所以要能在界面上看见——否则「为什么没买」只能去翻日志。
    pub credential_health: SupplierCredentialHealth,
}

impl fmt::Debug for SupplierOverview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SupplierOverview")
            .field("supplier_id", &self.supplier_id)
            .field("kind", &self.kind)
            .field("snapshot", &self.snapshot)
            .field("webhook_registered", &self.webhook_registered)
            .field("credential_health", &self.credential_health)
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
        import_overrides: crate::model::config::SupplierImportOverrides::from_legacy(
            &crate::model::config::KeySupplierConfig::from(&runtime),
        ),
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
        purchase_region_mode: PurchaseRegionMode::Omit,
        purchase_region: None,
        credential_api_region_fallback: API_KEY_AUTH_REGION.to_owned(),
        rpm_limit: 0,
        priority: 0,
        groups: Vec::new(),
        source_channel: String::new(),
        nickname_prefix: String::new(),
        restock_only_when_exhausted: false,
        target_usable: 0,
        low_quota_threshold: 0,
        max_unit_price: 0.0,
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
    SupplierApi {
        diagnostic: String,
    },
    /// 路径里的供货商 id 不存在（或事件所属供货商已被删除）。
    SupplierNotFound,
    /// 号池配置非法（目标存量或额度水位越界）。
    PoolConfig,
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
            Self::PoolConfig => "key supplier pool configuration is invalid",
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
            Self::PoolConfig => "PoolConfig",
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
    use std::collections::{HashMap, VecDeque};
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
        /// 按供货商 id 预置的可用统计，驱动补货闸的测试。
        health: Mutex<HashMap<String, SupplierCredentialHealth>>,
        /// 记录每次查询用的额度水位，验证配置真的传下来了。
        health_thresholds: Mutex<Vec<f64>>,
        /// 预置的全局号池统计，驱动号池闸的测试。
        pool: Mutex<PoolHealth>,
        /// 每次 `pool_health` 调用时的入参，验证水位与备注集合真的传下来了。
        pool_calls: Mutex<Vec<(f64, Vec<String>)>>,
        /// 每导入一个凭据就让全局可用数 +1。
        ///
        /// 号池闸的正确性依赖「导入后凭据立即对下一次统计可见」，不模拟这一点就
        /// 测不出「两家先后推送、缺口只有 1」这个核心场景。
        pool_grows_on_import: Mutex<bool>,
    }

    impl FakeImporter {
        fn with_outcomes(outcomes: Vec<anyhow::Result<()>>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into()),
                ..Default::default()
            }
        }

        fn set_health(&self, supplier_id: &str, health: SupplierCredentialHealth) {
            self.health
                .lock()
                .unwrap()
                .insert(supplier_id.to_owned(), health);
        }

        fn set_pool_usable(&self, usable: usize) {
            let mut pool = self.pool.lock().unwrap();
            pool.health.usable = usable;
            pool.health.total = usable;
            pool.by_supplier_id = usable;
        }

        /// 让导入后的凭据立刻反映到全局可用数上，模拟生产的同步可见语义。
        fn grow_pool_on_import(&self) {
            *self.pool_grows_on_import.lock().unwrap() = true;
        }

        fn pool_thresholds(&self) -> Vec<f64> {
            self.pool_calls
                .lock()
                .unwrap()
                .iter()
                .map(|(threshold, _)| *threshold)
                .collect()
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
            // 生产的 `add_credential` 是同步写 entries 再落盘，导入一返回凭据就
            // 能被统计看到。号池闸依赖这个语义，替身必须能模拟它。
            if outcome.is_ok() && *self.pool_grows_on_import.lock().unwrap() {
                let mut pool = self.pool.lock().unwrap();
                pool.health.usable += 1;
                pool.health.total += 1;
                pool.by_supplier_id += 1;
            }
            Box::pin(async move { outcome })
        }

        fn supplier_health(
            &self,
            supplier_id: &str,
            low_quota_threshold: f64,
        ) -> SupplierCredentialHealth {
            self.health_thresholds
                .lock()
                .unwrap()
                .push(low_quota_threshold);
            self.health
                .lock()
                .unwrap()
                .get(supplier_id)
                .copied()
                .unwrap_or_default()
        }

        fn pool_health(
            &self,
            low_quota_threshold: f64,
            configured_channels: &HashSet<String>,
        ) -> PoolHealth {
            let mut channels: Vec<String> = configured_channels.iter().cloned().collect();
            channels.sort();
            self.pool_calls
                .lock()
                .unwrap()
                .push((low_quota_threshold, channels));
            *self.pool.lock().unwrap()
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

        fn supplier_health(&self, _supplier_id: &str, _threshold: f64) -> SupplierCredentialHealth {
            SupplierCredentialHealth::default()
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
            purchase_region_mode: PurchaseRegionMode::Omit,
            purchase_region: None,
            credential_api_region_fallback: "us-east-1".to_string(),
            rpm_limit: 0,
            priority: 0,
            groups: Vec::new(),
            source_channel: String::new(),
            nickname_prefix: String::new(),
            restock_only_when_exhausted: false,
            target_usable: 0,
            low_quota_threshold: 0,
            max_unit_price: 0.0,
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
            purchase_region_mode: Some(runtime.purchase_region_mode),
            purchase_region: runtime.purchase_region,
            credential_api_region_fallback: Some(runtime.credential_api_region_fallback.clone()),
            rpm_limit: u64::from(runtime.rpm_limit),
            priority: u64::from(runtime.priority),
            groups: runtime.groups.clone(),
            source_channel: runtime.source_channel.clone(),
            nickname_prefix: runtime.nickname_prefix.clone(),
            restock_only_when_exhausted: runtime.restock_only_when_exhausted,
            target_usable: u64::from(runtime.target_usable),
            low_quota_threshold: u64::from(runtime.low_quota_threshold),
            max_unit_price: runtime.max_unit_price,
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
                supplier_batch_id: None,
                event_region: None,
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
        let decision = item.decision_snapshot.as_ref().unwrap();
        assert_eq!(decision.outcome, SupplierDecisionOutcome::Succeeded);
        assert_eq!(decision.quote.vendor_stock, Some(4));
        assert_eq!(decision.target.requested, Some(3));
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
        assert_eq!(
            credential.nickname.as_deref(),
            Some("默认供货商-supplier-fedcba98-1")
        );
    }

    #[test]
    fn credential_region_evidence_uses_documented_precedence() {
        let mut runtime = runtime(TOKEN);
        runtime.credential_api_region_fallback = "us-east-1".to_owned();

        let resolved = resolve_credential_region(
            Some(SupplierRegion::Eu),
            Some(SupplierRegion::Us),
            Some(SupplierRegion::Us),
            &runtime,
        );
        assert_eq!(resolved.api_region, "eu-central-1");
        assert_eq!(resolved.source, RegionSource::PurchaseResponse);

        let webhook = resolve_credential_region(
            None,
            Some(SupplierRegion::Eu),
            Some(SupplierRegion::Us),
            &runtime,
        );
        assert_eq!(webhook.api_region, "eu-central-1");
        assert_eq!(webhook.source, RegionSource::Webhook);

        let fallback = resolve_credential_region(None, None, None, &runtime);
        assert_eq!(fallback.api_region, "us-east-1");
        assert_eq!(fallback.source, RegionSource::ConfigFallback);
    }

    #[test]
    fn purchased_credentials_are_named_after_the_supplier_when_no_prefix_is_configured() {
        let mut runtime = runtime(TOKEN);
        let order = "a19fe933d889757410eb382be103c38e";

        // 没配前缀：默认用供货商名，不配任何东西也能一眼看出号是谁家的。
        assert_eq!(
            supplier_credential_nickname("drop", &runtime, order, 1),
            "drop-a19fe933-1"
        );
        // 同一单里多个 key 用序号区分，不会撞名。
        assert_eq!(
            supplier_credential_nickname("drop", &runtime, order, 2),
            "drop-a19fe933-2"
        );

        // 配了标签也只能附加，不能替换供货商名；否则凭据备注又会失去归属信息。
        runtime.nickname_prefix = "supplier-".to_owned();
        assert_eq!(
            supplier_credential_nickname("drop", &runtime, order, 1),
            "drop-supplier-a19fe933-1"
        );
        runtime.nickname_prefix = "vip".to_owned();
        assert_eq!(
            supplier_credential_nickname("drop", &runtime, order, 1),
            "drop-vip-a19fe933-1"
        );

        // 名字再长也不能越过备注上限，且必须在字符边界上截断（中文名会踩到）。
        runtime.nickname_prefix = String::new();
        let long = "供".repeat(200);
        let nickname = supplier_credential_nickname(&long, &runtime, order, 1);
        assert_eq!(nickname.chars().count(), MAX_NICKNAME_CHARS);
        assert!(nickname.ends_with("-a19fe933-1"));

        // 订单号短于 8 位也不能 panic（手动采购是 uuid，但别人改了就未必）。
        assert_eq!(
            supplier_credential_nickname("drop", &runtime, "abc", 1),
            "drop-abc-1"
        );
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
        let decision = failed.decision_snapshot.as_ref().unwrap();
        assert_eq!(decision.outcome, SupplierDecisionOutcome::Failed);
        assert_eq!(decision.result.purchased, 3);
        assert_eq!(decision.result.failed, 1);

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
                supplier_batch_id: None,
                event_region: None,
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
    async fn transient_api_failures_are_requeued_and_errors_never_contain_keys() {
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
        // 查库存拿到 502：瞬时故障 + 是个 GET，压回队列等自动重试而不是进 failed 终态。
        // 没走到下单，所以不钉数量——下一轮该按当时的水位重算。
        let deferred = store.list(1, None, None).unwrap().items.remove(0);
        assert_eq!(deferred.status, SupplierEventStatus::Received);
        assert!(deferred.retry_after.is_some());
        assert!(deferred.purchase_count.is_none());
        assert!(!format!("{deferred:?}").contains("ksk_api_failure_canary"));
        store.retry(deferred.id).unwrap();
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
        assert_eq!(overview.snapshot.status.as_ref().unwrap().keys_active, 3);
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
    async fn fixed_us_supplier_overview_uses_the_configured_ceo_zone() {
        let app = Router::new()
            .route(
                "/api/my/profile",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "name": "ceo", "quota": 200, "remaining": 180,
                        "used_quota": 20, "webhook_url": ""
                    }))
                }),
            )
            .route(
                "/api/my/stock",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "max": 20,
                        "zones": [
                            {"zone": "us", "enabled": true, "available": 9,
                             "max": 0, "unit_price": 20},
                            {"zone": "eu", "enabled": true, "available": 11,
                             "max": 0, "unit_price": 1}
                        ]
                    }))
                }),
            );
        let mut ceo = entry("ceo", SupplierKind::KiroCeo, &"f".repeat(64));
        ceo.settings.base_url = server(app).await;
        ceo.settings.purchase_region_mode = PurchaseRegionMode::Fixed;
        ceo.settings.purchase_region = Some(SupplierRegion::Us);
        let service = KeySupplierService::with_suppliers(
            Arc::new(SupplierEventStore::open_in_memory().unwrap()),
            vec![ceo],
        );

        let overview = service.supplier_overview("ceo").await.unwrap();

        assert_eq!(overview.snapshot.stock_available, Some(9));
        assert_eq!(overview.snapshot.key_price, Some(20.0));
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
                supplier_batch_id: None,
                event_region: None,
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
            import_overrides: crate::model::config::SupplierImportOverrides::default(),
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
            Arc::new(KeySupplierService::with_suppliers(store, entries).with_config_path(&path)),
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
    fn updating_common_import_recomputes_inherited_values_and_persists_flat_compatibility() {
        let mut supplier = entry("ceo", SupplierKind::KiroCeo, &"a".repeat(64));
        supplier.import_overrides.priority = Some(99);
        let (service, path) = multi_service(vec![supplier]);

        let view = service
            .update_common_import(
                crate::admin::key_supplier::config::SupplierCommonConfigUpdate {
                    source_channel: "统一采购".to_owned(),
                    nickname_label: "生产".to_owned(),
                    rpm_limit: 23,
                    priority: 7,
                    groups: vec!["common".to_owned()],
                    auto_delete_forbidden: true,
                },
            )
            .unwrap();

        assert_eq!(view.nickname_label, "生产");
        let current = service.supplier("ceo").unwrap();
        assert_eq!(current.settings.source_channel, "统一采购");
        assert_eq!(current.settings.nickname_prefix, "生产");
        assert_eq!(current.settings.rpm_limit, 23);
        assert_eq!(current.settings.priority, 99);
        assert_eq!(current.settings.groups, vec!["common"]);
        assert!(current.settings.auto_delete_forbidden);

        let config = Config::load(&path).unwrap();
        assert_eq!(config.key_supplier_common.nickname_label, "生产");
        assert_eq!(config.key_suppliers[0].settings.nickname_prefix, "生产");
        assert_eq!(config.key_suppliers[0].settings.priority, 99);
        assert_eq!(
            config.key_suppliers[0]
                .import_overrides
                .as_ref()
                .and_then(|value| value.priority),
            Some(99)
        );
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
    fn kiroapp_io_webhook_uses_the_vendor_supplied_idempotency_key_and_batch_id() {
        // 对方替我们派生好了 client_order_id（批次+收件人确定性派生），直接用它：
        // 拉取超时后原样重发即命中幂等重放，不会二次扣费。
        // 用 str + as_bytes：字节串字面量不接受非 ASCII，而这里要带中文 message。
        let body = r#"{"event":"new_keys_available","event_id":"evt_9",
            "order_id":"batch-abc","client_order_id":"d5c4fd9460b70fb8e944bd7faa519896",
            "mother_id":"m-1","visibility":"public","message":"20 个 Key 就绪","new_keys":20}"#;

        let parsed = IncomingWebhook::parse(SupplierKind::KiroAppIo, body.as_bytes()).unwrap();

        let IncomingWebhook::NewKeysAvailable {
            event_id,
            purchase_order_id,
            supplier_batch_id,
            new_keys,
            ..
        } = parsed
        else {
            panic!("new_keys_available should trigger a purchase");
        };
        // event_id 原样保留，方便和对方后台的投递记录对账。
        assert_eq!(event_id, "evt_9");
        assert_eq!(purchase_order_id, "d5c4fd9460b70fb8e944bd7faa519896");
        assert_eq!(supplier_batch_id.as_deref(), Some("batch-abc"));
        assert_eq!(new_keys, 20);
    }

    #[test]
    fn kiroapp_io_falls_back_to_a_derived_order_id_when_the_vendor_key_is_unusable() {
        // 采购接口要求 32 hex。对方给的键不合格就自己派生，且对同一事件稳定。
        for bad in ["short", "zzzz56789abcdef0123456789abcdefg"] {
            let body = format!(
                r#"{{"event":"new_keys_available","event_id":"evt_x","client_order_id":"{bad}","new_keys":1}}"#
            );
            let first = IncomingWebhook::parse(SupplierKind::KiroAppIo, body.as_bytes()).unwrap();
            let second = IncomingWebhook::parse(SupplierKind::KiroAppIo, body.as_bytes()).unwrap();
            let (
                IncomingWebhook::NewKeysAvailable {
                    purchase_order_id: first_order,
                    ..
                },
                IncomingWebhook::NewKeysAvailable {
                    purchase_order_id: second_order,
                    ..
                },
            ) = (first, second)
            else {
                panic!("{bad}");
            };
            assert_eq!(first_order, second_order, "{bad}");
            assert_eq!(first_order.len(), 32, "{bad}");
            assert!(
                first_order.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "{bad}"
            );
        }
    }

    #[test]
    fn kiroapp_io_non_arrival_events_never_become_purchase_signals() {
        // 这是最要紧的一条：`all_keys_dead` / `key_revoked_abuse` / 任何不认识的
        // event 名都不能被当成到货信号——否则会顶着 max_purchase 买一车。
        for event in [
            "all_keys_dead",
            "key_revoked_abuse",
            "something_we_have_never_seen",
        ] {
            let body = format!(r#"{{"event":"{event}","event_id":"evt_{event}","new_keys":9}}"#);
            let parsed = IncomingWebhook::parse(SupplierKind::KiroAppIo, body.as_bytes()).unwrap();
            assert!(
                matches!(parsed, IncomingWebhook::Notice { .. }),
                "{event} 不该触发采购"
            );
        }

        // 到货信号本身仍然正常识别。
        assert!(matches!(
            IncomingWebhook::parse(
                SupplierKind::KiroAppIo,
                br#"{"event":"new_keys_available","event_id":"e","new_keys":1}"#
            )
            .unwrap(),
            IncomingWebhook::NewKeysAvailable { .. }
        ));
        assert!(matches!(
            IncomingWebhook::parse(SupplierKind::KiroAppIo, br#"{"event":"test"}"#).unwrap(),
            IncomingWebhook::Test { .. }
        ));
        assert!(IncomingWebhook::parse(SupplierKind::KiroAppIo, b"not json").is_err());
    }

    #[test]
    fn kiro_drop_arrival_survives_the_missing_quantity_and_the_non_hex_batch_id() {
        // Drop 的到货推送没有 `new_keys`，且 `purchase_order_id` 是 `batch_xxx`。
        // 按 kiro-rs 的规则解析这两处都会失败，事件根本落不了库、永远不会采购。
        let body = r#"{"event":"new_keys_available","event_id":"0f1e2d3c4b5a69788796a5b4c3d2e1f0",
            "purchase_order_id":"batch_20260731_01","message":"新一批 Key 已上架"}"#;

        let first = IncomingWebhook::parse(SupplierKind::KiroDrop, body.as_bytes()).unwrap();
        let second = IncomingWebhook::parse(SupplierKind::KiroDrop, body.as_bytes()).unwrap();

        let (
            IncomingWebhook::NewKeysAvailable {
                event_id,
                purchase_order_id,
                supplier_batch_id,
                new_keys,
                ..
            },
            IncomingWebhook::NewKeysAvailable {
                purchase_order_id: replayed_order,
                ..
            },
        ) = (first, second)
        else {
            panic!("new_keys_available should trigger a purchase");
        };

        assert_eq!(event_id, "0f1e2d3c4b5a69788796a5b4c3d2e1f0");
        // 对方的批次号只留痕，不能当幂等键（采购接口硬校验 32 hex）。
        assert_eq!(supplier_batch_id.as_deref(), Some("batch_20260731_01"));
        assert_ne!(purchase_order_id, "batch_20260731_01");
        assert_eq!(purchase_order_id.len(), 32);
        assert!(
            purchase_order_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
        // 幂等键必须对同一事件稳定，否则重推会二次扣费。
        assert_eq!(purchase_order_id, replayed_order);
        // 推送不带数量，下单量由号池缺口/配置夹逼决定。
        assert_eq!(new_keys, 0);
    }

    #[test]
    fn kiro_drop_non_arrival_events_never_become_purchase_signals() {
        // 对方文档明说「所有已配置 Webhook 的用户都会收到全部事件推送」，
        // 所以 all_keys_dead 可能是别人家的号死了，绝不能当采购信号。
        let dead = IncomingWebhook::parse(
            SupplierKind::KiroDrop,
            br#"{"event":"all_keys_dead","event_id":"e1","dead":5}"#,
        )
        .unwrap();
        assert!(matches!(dead, IncomingWebhook::AllKeysDead { dead: 5, .. }));
        // `dead` 缺失也不算错——这个事件不花钱，只留痕。
        assert!(matches!(
            IncomingWebhook::parse(
                SupplierKind::KiroDrop,
                br#"{"event":"all_keys_dead","event_id":"e2"}"#
            )
            .unwrap(),
            IncomingWebhook::AllKeysDead { dead: 0, .. }
        ));
        assert!(matches!(
            IncomingWebhook::parse(
                SupplierKind::KiroDrop,
                br#"{"event":"test","event_id":"e3","message":"hi"}"#
            )
            .unwrap(),
            IncomingWebhook::Test { .. }
        ));
        // 未来新增的事件名一律拒收，不能被误当成到货信号。
        assert!(
            IncomingWebhook::parse(
                SupplierKind::KiroDrop,
                br#"{"event":"key_revoked_abuse","event_id":"e4"}"#
            )
            .is_err()
        );
        assert!(IncomingWebhook::parse(SupplierKind::KiroDrop, b"not json").is_err());
    }

    #[test]
    fn kiro_ceo_test_and_simulated_pushes_are_no_longer_rejected() {
        // 线上这四种载荷都被打回 400（nginx 日志里的 `400 72`），原因分别是
        // event_id 不是 32 hex、message 缺失、dead 为 0。它们都只是连通性验证或
        // 通知，不该因为 kiro-rs 那套「全都必填」的校验而进不了库。
        for body in [
            br#"{"event":"all_keys_dead","event_id":"evt_2026_08_01_a","message":"x","dead":12}"#
                .as_slice(),
            br#"{"event":"all_keys_dead","event_id":"a1000000000000000000000000000004","dead":12}"#,
            br#"{"event":"all_keys_dead","event_id":"a1000000000000000000000000000005","message":"x","dead":0}"#,
            br#"{"event":"test","event_id":"3f2b7c1d-9a4e-4b8f-8c2d-1e5a6b7c8d9e","message":"x"}"#,
        ] {
            assert!(
                IncomingWebhook::parse(SupplierKind::KiroCeo, body).is_ok(),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn kiro_ceo_arrival_reuses_the_vendor_order_id_as_the_idempotency_key() {
        // 对方文档要求把 `purchase_order_id` 原样当 `client_order_id` 用——重投也只
        // 成交一次。多出来的 `zone` 字段必须被忽略而不是让整条推送解析失败。
        let body = r#"{"event":"new_keys_available",
            "event_id":"7f3a9c2e1b4d5a6f8e9c0b1a2d3e4f5a",
            "purchase_order_id":"7f3a9c2e1b4d5a6f8e9c0b1a2d3e4f5a",
            "message":"美国区新增 20 个 Key 已就绪","new_keys":20,"zone":"us"}"#;

        let IncomingWebhook::NewKeysAvailable {
            event_id,
            purchase_order_id,
            new_keys,
            ..
        } = IncomingWebhook::parse(SupplierKind::KiroCeo, body.as_bytes()).unwrap()
        else {
            panic!("new_keys_available should trigger a purchase");
        };
        assert_eq!(event_id, "7f3a9c2e1b4d5a6f8e9c0b1a2d3e4f5a");
        assert_eq!(purchase_order_id, "7f3a9c2e1b4d5a6f8e9c0b1a2d3e4f5a");
        assert_eq!(new_keys, 20);

        // 名下号全灭是通知而非采购信号。
        assert!(matches!(
            IncomingWebhook::parse(
                SupplierKind::KiroCeo,
                br#"{"event":"all_keys_dead","event_id":"3c8d1f0a5b7e2694c1d8a0f3b5e7c9d2",
                    "message":"dead","dead":12}"#
            )
            .unwrap(),
            IncomingWebhook::AllKeysDead { dead: 12, .. }
        ));
    }

    #[test]
    fn pushed_order_id_is_reused_as_the_idempotency_key_only_where_the_vendor_asks_for_it() {
        // Kiro Drop 和 kiro.ceo 的推送都明确要求「把 purchase_order_id 原样传给
        // /api/my/purchase」，Drop 的 message 里就写着这句话。所以 32 hex 的值直接当
        // 幂等键，不另外派生——派生出来的 id 对方不认识，重复推送时也就失去了它那侧的去重。
        //
        // 判据是**对方文档怎么说**，不是字段长得像什么：非 32 hex 的值（Drop 早期的
        // `batch_xxx`）当不了幂等键，那时才派生，原值进 `supplier_batch_id` 留痕。
        let vendor_id = "a19fe933d889757410eb382be103c38e";
        let body = format!(
            r#"{{"event":"new_keys_available","event_id":"{vendor_id}",
                "purchase_order_id":"{vendor_id}","message":"新一批 Key 已上架"}}"#
        );

        let IncomingWebhook::NewKeysAvailable {
            purchase_order_id,
            supplier_batch_id,
            ..
        } = IncomingWebhook::parse(SupplierKind::KiroDrop, body.as_bytes()).unwrap()
        else {
            panic!("arrival push must stay an arrival push");
        };

        // 原样传回，不派生。
        assert_eq!(purchase_order_id, vendor_id);
        // 值本身就是订单号，不需要再往 supplier_batch_id 里留一份。
        assert_eq!(supplier_batch_id, None);

        // kiro.ceo 同样是原样传回。
        let IncomingWebhook::NewKeysAvailable {
            purchase_order_id, ..
        } = IncomingWebhook::parse(SupplierKind::KiroCeo, body.as_bytes()).unwrap()
        else {
            panic!("arrival push must stay an arrival push");
        };
        assert_eq!(purchase_order_id, vendor_id);

        // 非 32 hex 才派生，并把原值留痕——Drop 早期推的就是 `batch_xxx`。
        let legacy = br#"{"event":"new_keys_available","event_id":"evt_a",
            "purchase_order_id":"batch_20260731_01"}"#;
        let IncomingWebhook::NewKeysAvailable {
            purchase_order_id,
            supplier_batch_id,
            ..
        } = IncomingWebhook::parse(SupplierKind::KiroDrop, legacy).unwrap()
        else {
            panic!("arrival push must stay an arrival push");
        };
        assert_eq!(purchase_order_id, derive_order_id("evt_a"));
        assert_eq!(purchase_order_id.len(), 32);
        assert!(purchase_order_id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(supplier_batch_id.as_deref(), Some("batch_20260731_01"));
    }

    #[test]
    fn kiro_drop_tolerates_an_event_id_that_is_not_32_hex() {
        // 对方文档只写「32 位 ID」，没说是 hex。押 kiro-rs 那套强校验就是每条推送都
        // InvalidPayload——采购永远不会发生，而且从日志上看只是「格式不对」。
        let body = br#"{"event":"new_keys_available","event_id":"evt_2026_07_31_a"}"#;
        let IncomingWebhook::NewKeysAvailable {
            event_id,
            purchase_order_id,
            ..
        } = IncomingWebhook::parse(SupplierKind::KiroDrop, body).unwrap()
        else {
            panic!("a non-hex event id must not kill the arrival signal");
        };
        // 原样保留，方便和对方后台的投递记录对账。
        assert_eq!(event_id, "evt_2026_07_31_a");
        assert_eq!(purchase_order_id.len(), 32);

        // 连 event_id 都没有时退化成 body 指纹：同一车重复推仍映射到同一行，
        // 靠 (supplier_id, event_id) 唯一索引挡住第二次下单。
        let anonymous = br#"{"event":"new_keys_available","purchase_order_id":"batch_7"}"#;
        let other = br#"{"event":"new_keys_available","purchase_order_id":"batch_8"}"#;
        let id_of =
            |body: &[u8]| match IncomingWebhook::parse(SupplierKind::KiroDrop, body).unwrap() {
                IncomingWebhook::NewKeysAvailable { event_id, .. } => event_id,
                _ => panic!("an arrival push must stay an arrival push"),
            };
        assert_eq!(id_of(anonymous), id_of(anonymous));
        // 不同批次不能塌成同一个去重键，否则第二车永远买不到。
        assert_ne!(id_of(anonymous), id_of(other));
    }

    #[tokio::test]
    async fn kiroapp_io_dead_key_event_is_recorded_without_contacting_the_purchase_endpoint() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move || {
                let observed = observed.clone();
                async move {
                    *observed.lock().unwrap() += 1;
                    axum::Json(serde_json::json!({
                        "purchased": 1, "remaining": 0, "total_debit": 30,
                        "keys": [{"key": "ksk_should_never_be_bought"}]
                    }))
                }
            }),
        );
        let token = "9".repeat(64);
        let mut supplier = entry("io", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::default());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer.clone(),
        );

        service
            .ingest(
                &token,
                r#"{"event":"key_revoked_abuse","event_id":"evt_abuse","message":"key 被回收"}"#,
            )
            .unwrap();
        service.process_pending().await.unwrap();

        // 一次采购请求都不该发出去，也不该导入任何凭据。
        assert_eq!(*calls.lock().unwrap(), 0, "通知类事件绝不能触发采购");
        assert!(importer.credentials.lock().unwrap().is_empty());
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.event_type, "key_revoked_abuse");
        assert_eq!(stored.status, SupplierEventStatus::Succeeded);
        assert_eq!(stored.purchased_count, 0);
    }

    /// 补货闸测试底座：一个计数采购请求的假上游 + 可预置可用统计的 importer。
    async fn restock_gate_fixture(
        gate_enabled: bool,
        usable_threshold: u32,
        low_quota_threshold: u32,
    ) -> (
        Arc<KeySupplierService>,
        Arc<SupplierEventStore>,
        Arc<FakeImporter>,
        Arc<Mutex<usize>>,
        String,
    ) {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move || {
                let observed = observed.clone();
                async move {
                    *observed.lock().unwrap() += 1;
                    axum::Json(serde_json::json!({
                        "purchased": 1, "remaining": 0, "total_debit": 40,
                        "keys": [{"key": "ksk_restocked"}]
                    }))
                }
            }),
        );
        let token = "d".repeat(64);
        let mut supplier = entry("io", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        supplier.settings.min_purchase = 1;
        supplier.settings.max_purchase = 5;
        supplier.settings.restock_only_when_exhausted = gate_enabled;
        supplier.settings.target_usable = usable_threshold;
        supplier.settings.low_quota_threshold = low_quota_threshold;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::default());
        let service = Arc::new(KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer.clone(),
        ));
        (service, store, importer, calls, token)
    }

    fn arrival_body(event_id: &str) -> String {
        format!(r#"{{"event":"new_keys_available","event_id":"{event_id}","new_keys":2}}"#)
    }

    #[tokio::test]
    async fn restock_gate_skips_purchase_once_the_target_is_reached() {
        let (service, store, importer, calls, token) = restock_gate_fixture(true, 3, 0).await;
        importer.set_health(
            "io",
            SupplierCredentialHealth {
                total: 3,
                usable: 3,
                ..Default::default()
            },
        );

        service
            .ingest(&token, arrival_body("evt_has_usable"))
            .unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 0, "已达目标存量就不该下单");
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        assert!(
            stored
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("已达目标存量"),
            "{:?}",
            stored.message
        );
    }

    /// 用户要的那条：**一家推三次只买到目标存量**，不是推三次买三次。
    ///
    /// 旧实现是「可用数 > 水位就跳过」。填 1 时买到第 1 个后 `1 > 1` 为假，于是继续买，
    /// 同一家连推三次就买三次——这条测试锁住新的缺口语义。
    #[tokio::test]
    async fn per_supplier_target_of_one_buys_once_no_matter_how_many_pushes_arrive() {
        let (service, store, importer, calls, token) = restock_gate_fixture(true, 1, 0).await;
        importer.set_health(
            "io",
            SupplierCredentialHealth {
                total: 0,
                usable: 0,
                ..Default::default()
            },
        );

        service.ingest(&token, arrival_body("evt_push_1")).unwrap();
        service.process_pending().await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 1, "第一次到货应当补到目标存量");

        // 第一单已经把该家补到 1 个可用号。
        importer.set_health(
            "io",
            SupplierCredentialHealth {
                total: 1,
                usable: 1,
                ..Default::default()
            },
        );
        for event_id in ["evt_push_2", "evt_push_3"] {
            service.ingest(&token, arrival_body(event_id)).unwrap();
            service.process_pending().await.unwrap();
        }

        assert_eq!(*calls.lock().unwrap(), 1, "目标存量 1，三次推送只该买一次");
        let events = store.list(9, None, None).unwrap().items;
        assert_eq!(events.len(), 3);
        // 后两条都记跳过并写明原因，而不是静悄悄什么都没发生。
        for skipped in events.iter().take(2) {
            assert_eq!(skipped.status, SupplierEventStatus::Skipped);
            assert_eq!(skipped.pool_usable, Some(1));
            assert_eq!(skipped.pool_deficit, Some(0));
            assert_eq!(skipped.pool_requested, Some(0));
        }
    }

    #[tokio::test]
    async fn restock_gate_buys_when_every_key_is_dead() {
        let (service, _store, importer, calls, token) = restock_gate_fixture(true, 3, 0).await;
        importer.set_health(
            "io",
            SupplierCredentialHealth {
                total: 3,
                usable: 0,
                dead: 3,
                ..Default::default()
            },
        );

        service
            .ingest(&token, arrival_body("evt_all_dead"))
            .unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 1);
    }

    /// 这条盯的是最初的 bug：额度耗尽被禁的号当时算「活」，永远堵住补货。
    #[tokio::test]
    async fn restock_gate_treats_quota_exhausted_keys_as_unusable() {
        let (service, _store, importer, calls, token) = restock_gate_fixture(true, 3, 0).await;
        importer.set_health(
            "io",
            SupplierCredentialHealth {
                total: 4,
                usable: 0,
                // 一个封号，三个额度跑光——都不是「活号」。
                dead: 1,
                quota_exhausted: 3,
                ..Default::default()
            },
        );

        service
            .ingest(&token, arrival_body("evt_quota_dead"))
            .unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "额度耗尽必须算不可用，否则永不补货"
        );
    }

    /// 第二个 bug：号还能用但只剩几百额度。只等 402 就得先把号跑干才补货。
    #[tokio::test]
    async fn restock_gate_counts_low_quota_keys_as_unusable_and_passes_the_threshold_down() {
        let (service, _store, importer, calls, token) = restock_gate_fixture(true, 2, 500).await;
        importer.set_health(
            "io",
            SupplierCredentialHealth {
                total: 2,
                usable: 0,
                low_quota: 2,
                ..Default::default()
            },
        );

        service
            .ingest(&token, arrival_body("evt_low_quota"))
            .unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 1);
        // 配置里的额度水位必须真的传到统计里——传 0 会让水位判定静默失效。
        assert_eq!(*importer.health_thresholds.lock().unwrap(), vec![500.0]);
    }

    #[tokio::test]
    async fn restock_gate_buys_only_the_deficit_up_to_the_target() {
        let (service, _store, importer, calls, token) = restock_gate_fixture(true, 3, 0).await;
        importer.set_health(
            "io",
            SupplierCredentialHealth {
                total: 5,
                usable: 2,
                dead: 3,
                ..Default::default()
            },
        );

        // 目标 3、可用 2：缺口 1，买。
        service
            .ingest(&token, arrival_body("evt_below_target"))
            .unwrap();
        service.process_pending().await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 1);

        // 可用 3 = 目标 3：缺口 0，不买。
        importer.set_health(
            "io",
            SupplierCredentialHealth {
                total: 5,
                usable: 3,
                dead: 2,
                ..Default::default()
            },
        );
        service
            .ingest(&token, arrival_body("evt_at_target"))
            .unwrap();
        service.process_pending().await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 1, "已达目标存量不该再买");
    }

    /// 生产事故：kiro.ceo 按区严格隔离，不传 `zone` 只从美国区取号，美国区空了返 409
    /// 而**不会**用欧洲区顶上。我们拿跨区合计的 `max > 0` 去下一个只打美国区的单，
    /// 于是连续「购买失败」，而欧洲区一直有 4 个货。
    #[tokio::test]
    async fn kiro_ceo_buys_from_the_zone_that_actually_has_stock() {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let observed = bodies.clone();
        let app = Router::new()
            .route(
                "/api/my/stock",
                get(|| async {
                    // 线上真实形状：合计 2，但美国区 0、欧洲区 4（本区单笔上限 2）。
                    axum::Json(serde_json::json!({
                        "max": 2, "max_purchase": 10, "min": 1, "quota": 322, "reserved": 0,
                        "zones": [
                            {"zone": "us", "enabled": true, "available": 0, "max": 0,
                             "stock": 0, "unit_price": 20},
                            {"zone": "eu", "enabled": true, "available": 4, "max": 2,
                             "stock": 4, "unit_price": 15}
                        ]
                    }))
                }),
            )
            .route(
                "/api/my/purchase",
                post(move |body: axum::body::Bytes| {
                    let observed = observed.clone();
                    async move {
                        let text = String::from_utf8(body.to_vec()).unwrap();
                        observed.lock().unwrap().push(text.clone());
                        let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                        // 对方的真实行为：没指定区就只看美国区，空了就 409。
                        if request.get("zone").and_then(serde_json::Value::as_str) != Some("eu") {
                            return (
                                axum::http::StatusCode::CONFLICT,
                                r#"{"error":"库存不足"}"#.to_owned(),
                            );
                        }
                        (
                            axum::http::StatusCode::OK,
                            r#"{"client_order_id":"","purchased":1,"remaining":307,
                                "zone":"eu","unit_price":15,"total_credits":15,
                                "keys":[{"key":"kiro-eu-1"}]}"#
                                .to_owned(),
                        )
                    }
                }),
            );
        let token = "e".repeat(64);
        let mut supplier = entry("ceo", SupplierKind::KiroCeo, &token);
        supplier.settings.base_url = server(app).await;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::default());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer.clone(),
        );

        service
            .ingest(
                &token,
                r#"{"event":"new_keys_available","event_id":"7f3a9c2e1b4d5a6f8e9c0b1a2d3e4f5a",
                    "purchase_order_id":"7f3a9c2e1b4d5a6f8e9c0b1a2d3e4f5a","new_keys":20,
                    "zone":"eu"}"#,
            )
            .unwrap();
        service.process_pending().await.unwrap();

        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(
            stored.status,
            SupplierEventStatus::Succeeded,
            "{:?}",
            stored.message
        );
        assert_eq!(stored.imported_count, 1);

        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1, "选对区就该一次成交，不该先撞一次 409");
        let request: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(request["zone"], "eu", "必须显式带上有货的那个区");
        // 数量受该区单笔上限 2 夹逼，不是推送里的 20，也不是合计的那个数。
        assert_eq!(request["count"], 2);
    }

    /// kiro.ceo 的 409 不等于「原单已成交」。
    ///
    /// 它的文档把 409 定义成「状态冲突」，同时覆盖库存不足、已达最大持有库存上限、
    /// 幂等键撞单三件事，并明说「用同一个 id 重试是安全的」。按「积分已扣、去订单历史
    /// 补取 key」去报，是让人去查一条根本不存在的订单。
    #[tokio::test]
    async fn kiro_ceo_conflict_is_reported_as_a_state_conflict_with_the_vendor_wording() {
        let app = Router::new()
            .route(
                "/api/my/stock",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "max": 9,
                        "zones": [{"zone": "us", "enabled": true, "available": 9,
                                   "max": 0, "unit_price": 20}]
                    }))
                }),
            )
            .route(
                "/api/my/purchase",
                post(|| async {
                    (
                        axum::http::StatusCode::CONFLICT,
                        r#"{"error":"已达最大持有库存上限"}"#,
                    )
                }),
            );
        let token = "c".repeat(64);
        let mut supplier = entry("ceo", SupplierKind::KiroCeo, &token);
        supplier.settings.base_url = server(app).await;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            Arc::new(FakeImporter::default()),
        );

        service
            .ingest(
                &token,
                r#"{"event":"new_keys_available","event_id":"43da1a1cf0e945c9000cbe12fb680ae6",
                    "purchase_order_id":"43da1a1cf0e945c9000cbe12fb680ae6","new_keys":26}"#,
            )
            .unwrap();
        service.process_pending().await.unwrap();

        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        let message = stored.message.as_deref().unwrap_or_default();
        // 对方原话必须带出来——只有它能分辨这三种冲突。
        assert!(message.contains("已达最大持有库存上限"), "{message}");
        // 绝不能声称钱扣了：库存不足和持有上限这两种一分钱没动。
        assert!(!message.contains("积分已扣"), "{message}");
        assert!(!message.contains("补取 key"), "{message}");

        // kiroapp-io 的 409 只有「同一订单号换了参数」一个含义，保持已成交语义。
        assert!(SupplierKind::KiroAppIo.conflict_means_order_settled());
        // ceo 和 Drop 的 409 都是多义的（缺货 / 余额不足 / 持有上限 / 撞单 / 超价），
        // 里面只有撞单扣了钱，所以不能一律报「积分已扣」。
        assert!(!SupplierKind::KiroCeo.conflict_means_order_settled());
        assert!(!SupplierKind::KiroDrop.conflict_means_order_settled());
    }

    /// 单价闸：贵了不买，且「问不到价」也不买。
    #[tokio::test]
    async fn unit_price_ceiling_blocks_expensive_stock_and_unpriced_suppliers() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new()
            .route(
                "/api/me/stock",
                get(|| async {
                    axum::Json(serde_json::json!({"stock": 9, "price_min": 38, "balance": 900}))
                }),
            )
            .route(
                "/api/me/purchase",
                post(move || {
                    let observed = observed.clone();
                    async move {
                        *observed.lock().unwrap() += 1;
                        axum::Json(serde_json::json!({
                            "purchased": 1, "remaining": 8, "total_debit": 38,
                            "keys": [{"key": "ksk_pricey"}]
                        }))
                    }
                }),
            );
        let token = "9".repeat(64);
        let mut supplier = entry("io", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        supplier.settings.max_unit_price = 30.0;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            Arc::new(FakeImporter::default()),
        );

        service
            .ingest(
                &token,
                r#"{"event":"new_keys_available","event_id":"evt_pricey","new_keys":1}"#,
            )
            .unwrap();
        service.process_pending().await.unwrap();

        // 38 > 30：不下单，并把原因写清楚。
        assert_eq!(*calls.lock().unwrap(), 0, "单价超上限不该下单");
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        assert!(
            stored
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("单价高于"),
            "{:?}",
            stored.message
        );
    }

    /// kiro-rs 在下单前报不出单价。配了上限就只能不买——把「不知道价」当免费放行，
    /// 等于这道闸在最需要它的时候失效。
    #[tokio::test]
    async fn unit_price_ceiling_skips_suppliers_that_cannot_quote_a_price() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new()
            .route(
                "/api/my/stock",
                get(|| async { axum::Json(serde_json::json!({"max": 5})) }),
            )
            .route(
                "/api/my/purchase",
                post(move || {
                    let observed = observed.clone();
                    async move {
                        *observed.lock().unwrap() += 1;
                        purchase_json(ORDER_ID, &["ksk_unpriced"])
                    }
                }),
            );
        let mut config = runtime(TOKEN);
        config.base_url = server(app).await;
        config.auto_purchase = true;
        config.max_unit_price = 30.0;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        queued_event(&store, "new_keys_available", Some(ORDER_ID), 1);
        let service = KeySupplierService::with_importer(
            store.clone(),
            config,
            Arc::new(FakeImporter::default()),
        );

        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 0);
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        assert!(
            stored
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("不报价"),
            "{:?}",
            stored.message
        );
    }

    /// 失效保护：开了闸却把目标存量留成 0，是配置没填完，不是「无限买」。
    #[tokio::test]
    async fn restock_gate_with_a_zero_target_buys_nothing() {
        let (service, store, importer, calls, token) = restock_gate_fixture(true, 0, 0).await;
        importer.set_health("io", SupplierCredentialHealth::default());

        service.ingest(&token, arrival_body("evt_zero")).unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 0, "目标存量 0 时宁可不买");
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        assert!(
            stored
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("目标存量不可用"),
            "{:?}",
            stored.message
        );
    }

    #[tokio::test]
    async fn restock_gate_allows_the_first_purchase_into_an_empty_pool() {
        let (service, _store, importer, calls, token) = restock_gate_fixture(true, 1, 0).await;
        // 什么都没买过：total=0，usable=0。必须放行，否则首次补货起不来。
        importer.set_health("io", SupplierCredentialHealth::default());

        service
            .ingest(&token, arrival_body("evt_bootstrap"))
            .unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn restock_gate_does_not_block_manual_purchase() {
        let (service, _store, importer, calls, _token) = restock_gate_fixture(true, 0, 0).await;
        importer.set_health(
            "io",
            SupplierCredentialHealth {
                total: 9,
                usable: 9,
                ..Default::default()
            },
        );

        // 手动采购是人明确要买，闸门不该拦。
        service.manual_purchase_from("io", 1).await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn gate_off_keeps_buying_on_every_arrival() {
        let (service, _store, importer, calls, token) = restock_gate_fixture(false, 0, 0).await;
        importer.set_health(
            "io",
            SupplierCredentialHealth {
                total: 9,
                usable: 9,
                ..Default::default()
            },
        );

        service
            .ingest(&token, arrival_body("evt_gate_off_1"))
            .unwrap();
        service.process_pending().await.unwrap();
        service
            .ingest(&token, arrival_body("evt_gate_off_2"))
            .unwrap();
        service.process_pending().await.unwrap();

        // 关闭时保持历史行为：每条到货都买，一次可用统计都不查。
        assert_eq!(*calls.lock().unwrap(), 2);
        assert!(importer.health_thresholds.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[allow(clippy::type_complexity)]
    async fn kiroapp_io_arrival_purchases_the_batch_and_imports_the_keys() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let stock_calls = Arc::new(Mutex::new(0_usize));
        let stock_observed = stock_calls.clone();
        let app = Router::new()
            .route(
                "/api/me/stock",
                get(move || {
                    let stock_observed = stock_observed.clone();
                    async move {
                        *stock_observed.lock().unwrap() += 1;
                        axum::Json(serde_json::json!({"stock": 2, "price_min": 30, "balance": 900}))
                    }
                }),
            )
            .route(
                "/api/me/purchase",
                post(move |body: axum::body::Bytes| {
                    let observed = observed.clone();
                    async move {
                        observed
                            .lock()
                            .unwrap()
                            .push(String::from_utf8(body.to_vec()).unwrap());
                        axum::Json(serde_json::json!({
                            "purchased": 2, "requested": 2, "remaining": 5, "total_debit": 68,
                            "unit_price": 34, "order_id": "ord-io-9", "replayed": false,
                            "keys": [
                                {"key": "ksk_io_one", "account": "user-1", "password": "pw",
                                 "issuer_url": "https://idc.example", "price": 30},
                                {"key": "ksk_io_two", "account": "user-2", "password": "pw",
                                 "issuer_url": "https://idc.example", "price": 38}
                            ]
                        }))
                    }
                }),
            );
        let token = "8".repeat(64);
        let mut supplier = entry("io", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        supplier.settings.min_purchase = 1;
        supplier.settings.max_purchase = 5;
        supplier.settings.purchase_region_mode = PurchaseRegionMode::Batch;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::default());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer.clone(),
        );

        service
            .ingest(
                &token,
                r#"{"event":"new_keys_available","event_id":"evt_io_1","order_id":"batch-io",
                    "client_order_id":"d5c4fd9460b70fb8e944bd7faa519896","new_keys":2}"#,
            )
            .unwrap();
        service.process_pending().await.unwrap();

        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let sent: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
        assert_eq!(sent["count"], 2);
        // 用对方给的幂等键，并定向到该批次。
        assert_eq!(sent["client_order_id"], "d5c4fd9460b70fb8e944bd7faa519896");
        assert_eq!(sent["order_id"], "batch-io");
        // 和 kiroapp.cc 同理不先查库存：查询和领取不是一个事务。
        assert_eq!(*stock_calls.lock().unwrap(), 0, "领取前不该查库存");

        let imported = importer.credentials.lock().unwrap();
        assert_eq!(imported.len(), 2);
        assert_eq!(imported[0].kiro_api_key.as_deref(), Some("ksk_io_one"));
        assert_eq!(imported[1].kiro_api_key.as_deref(), Some("ksk_io_two"));
        // 单价按 key 落到凭据上：和 added_at / died_at 一起才能算「每存活小时成本」。
        // 按单摊（68/2=34）会把 30 和 38 抹平，比价就失真了。
        assert_eq!(imported[0].purchase_price, Some(30.0));
        assert_eq!(imported[1].purchase_price, Some(38.0));

        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Succeeded);
        assert_eq!(stored.purchased_count, 2);
        assert_eq!(stored.imported_count, 2);
        // 批次号入库，能和对方后台的批次对账。
        assert_eq!(stored.supplier_batch_id.as_deref(), Some("batch-io"));
        // 花费必须落库：没有它就做不了跨供货商预算封顶，事后也算不出账。
        assert_eq!(stored.total_debit, Some(68));
        assert_eq!(stored.unit_price, Some(34.0));
        // 对方订单号和批次号是两个不同的东西，别混用：这个用来查对方的订单历史。
        assert_eq!(stored.supplier_order_id.as_deref(), Some("ord-io-9"));
        assert!(!stored.replayed);
    }

    #[tokio::test]
    async fn kiroapp_io_fixed_region_ignores_the_webhook_batch_id() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move |body: axum::body::Bytes| {
                let observed = observed.clone();
                async move {
                    observed
                        .lock()
                        .unwrap()
                        .push(serde_json::from_slice::<serde_json::Value>(&body).unwrap());
                    axum::Json(serde_json::json!({
                        "purchased": 1,
                        "requested": 1,
                        "remaining": 4,
                        "total_debit": 30,
                        "unit_price": 30,
                        "order_id": "ord-fixed-us",
                        "replayed": false,
                        "region": "us",
                        "keys": [{"key": "ksk_fixed_us", "price": 30}]
                    }))
                }
            }),
        );
        let token = "7".repeat(64);
        let mut supplier = entry("io-fixed", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        supplier.settings.purchase_region_mode = PurchaseRegionMode::Fixed;
        supplier.settings.purchase_region = Some(SupplierRegion::Us);
        let service = KeySupplierService::with_suppliers_and_importer(
            Arc::new(SupplierEventStore::open_in_memory().unwrap()),
            vec![supplier],
            Arc::new(FakeImporter::default()),
        );

        service
            .ingest(
                &token,
                r#"{"event":"new_keys_available","event_id":"evt_io_fixed","order_id":"batch-eu",
                    "client_order_id":"95c4fd9460b70fb8e944bd7faa519897","region":"eu","new_keys":1}"#,
            )
            .unwrap();
        service.process_pending().await.unwrap();

        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["region"], "us");
        assert!(requests[0].get("order_id").is_none());
    }

    /// 搭一套开着号池闸的服务：一家 kiroapp-io 供货商 + 可控的全局可用数。
    ///
    /// 返回 `(service, store, importer, 采购请求计数)`。请求计数是关键——
    /// 「缺口为 0 时不发任何请求」只看事件状态是测不出来的。
    #[allow(clippy::type_complexity)]
    async fn pool_service(
        token: &str,
        target_count: u32,
        usable: usize,
    ) -> (
        KeySupplierService,
        Arc<SupplierEventStore>,
        Arc<FakeImporter>,
        Arc<Mutex<usize>>,
    ) {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move |body: axum::body::Bytes| {
                let observed = observed.clone();
                async move {
                    *observed.lock().unwrap() += 1;
                    let sent: serde_json::Value =
                        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    let count = sent["count"].as_u64().unwrap_or(0);
                    let keys: Vec<serde_json::Value> = (0..count)
                        .map(|index| serde_json::json!({"key": format!("ksk_pool_{index}"), "price": 30}))
                        .collect();
                    axum::Json(serde_json::json!({
                        "purchased": count, "requested": count, "remaining": 99,
                        "total_debit": count * 30, "unit_price": 30,
                        "order_id": "ord-pool", "keys": keys, "replayed": false
                    }))
                }
            }),
        );
        let mut supplier = entry("io", SupplierKind::KiroAppIo, token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        supplier.settings.min_purchase = 1;
        supplier.settings.max_purchase = 50;
        supplier.settings.source_channel = "Webhook 自动采购".to_string();

        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::default());
        importer.set_pool_usable(usable);
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer.clone(),
        )
        .with_pool_config(PoolRuntimeConfig {
            enabled: true,
            target_count,
            low_quota_threshold: 0,
        });
        (service, store, importer, calls)
    }

    fn arrival(event_id: &str, order_id: &str, new_keys: u32) -> String {
        format!(
            r#"{{"event":"new_keys_available","event_id":"{event_id}",
                "client_order_id":"{order_id}","new_keys":{new_keys}}}"#
        )
    }

    /// 号池闸：缺口说了算，推送带的数量只留痕不作依据。
    #[tokio::test]
    async fn pool_gate_buys_only_the_deficit_regardless_of_the_pushed_count() {
        let token = "a".repeat(64);
        // 目标 5、池里已有 3 个可用 → 缺口 2。推送说有 40 个也只买 2。
        let (service, store, importer, calls) = pool_service(&token, 5, 3).await;

        service
            .ingest(&token, arrival("evt_pool_gap", &"d".repeat(32), 40))
            .unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(
            importer.credentials.lock().unwrap().len(),
            2,
            "只该补缺口那 2 个"
        );

        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Succeeded);
        assert_eq!(stored.purchased_count, 2);
        assert_eq!(stored.pool_usable, Some(3));
        assert_eq!(stored.pool_deficit, Some(2));
        assert_eq!(stored.pool_requested, Some(2));
        // 推送带的数量仍原样留痕供对账。
        assert_eq!(stored.quantity, 40);
    }

    /// 缺口为 0 时**一个 HTTP 请求都不该发出去**。
    ///
    /// 只断言事件状态是不够的：查库存和下单都是网络往返，池子满了还打出去就是
    /// 白给对方压力、也白等一次超时。所以这里用请求计数断言。
    #[tokio::test]
    async fn pool_gate_sends_no_request_when_the_pool_is_already_full() {
        let token = "b".repeat(64);
        let (service, store, importer, calls) = pool_service(&token, 3, 3).await;

        service
            .ingest(&token, arrival("evt_pool_full", &"e".repeat(32), 10))
            .unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 0, "池子满了不该发任何请求");
        assert!(importer.credentials.lock().unwrap().is_empty());

        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        assert_eq!(stored.message.as_deref(), Some("号池已达目标存量"));
        // 跳过路径同样要落水位快照，否则「为什么没买」无从查证。
        assert_eq!(stored.pool_usable, Some(3));
        assert_eq!(stored.pool_deficit, Some(0));
        assert_eq!(stored.pool_requested, Some(0));
        let decision = stored.decision_snapshot.as_ref().unwrap();
        assert_eq!(decision.outcome, SupplierDecisionOutcome::Skipped);
        assert_eq!(decision.reason.as_deref(), Some("号池已达目标存量"));
        assert_eq!(decision.target.configured, Some(3));
        assert_eq!(decision.target.credited_at_decision, Some(3));
        assert_eq!(decision.target.deficit, Some(0));
        assert_eq!(decision.target.reached, Some(true));
    }

    /// 可用数已经超过目标存量时也只是不买，绝不去处置多出来的凭据。
    #[tokio::test]
    async fn pool_over_target_only_stops_buying_and_never_touches_credentials() {
        let token = "c".repeat(64);
        let (service, store, importer, calls) = pool_service(&token, 2, 7).await;

        service
            .ingest(&token, arrival("evt_pool_over", &"f".repeat(32), 5))
            .unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 0);
        assert!(importer.credentials.lock().unwrap().is_empty());
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        assert_eq!(stored.pool_deficit, Some(0));
    }

    /// 启用但没填目标存量（中毒配置）→ 跳过采购，而不是退回不受限的逐家采购。
    #[tokio::test]
    async fn poisoned_pool_config_skips_instead_of_falling_back_to_per_supplier_buying() {
        let token = "1".repeat(64);
        // target_count = 0 就是 `PoolRuntimeConfig::poisoned()` 的形状。
        let (service, store, importer, calls) = pool_service(&token, 0, 0).await;

        service
            .ingest(&token, arrival("evt_pool_poison", &"2".repeat(32), 5))
            .unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 0, "配置非法时绝不能继续花钱");
        assert!(importer.credentials.lock().unwrap().is_empty());
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        assert_eq!(
            stored.message.as_deref(),
            Some("号池目标存量不可用，跳过采购")
        );
    }

    /// 缺口低于该家单笔下限时放弃，**不放大到下限凑单**。
    ///
    /// 放大会买超目标存量，而且缺口越小超得越多——那正是这道闸要防的事。
    #[tokio::test]
    async fn pool_gate_gives_up_rather_than_rounding_up_to_the_supplier_minimum() {
        let token = "3".repeat(64);
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move || {
                let observed = observed.clone();
                async move {
                    *observed.lock().unwrap() += 1;
                    axum::Json(serde_json::json!({"purchased": 0, "remaining": 0, "keys": []}))
                }
            }),
        );
        let mut supplier = entry("io", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        // 缺口 1，但这家单笔至少买 5。
        supplier.settings.min_purchase = 5;
        supplier.settings.max_purchase = 50;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::default());
        importer.set_pool_usable(2);
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer.clone(),
        )
        .with_pool_config(PoolRuntimeConfig {
            enabled: true,
            target_count: 3,
            low_quota_threshold: 0,
        });

        service
            .ingest(&token, arrival("evt_pool_min", &"4".repeat(32), 10))
            .unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 0, "不该为凑下限而买超");
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        assert_eq!(stored.message.as_deref(), Some("缺口低于该供货商单笔下限"));
        assert_eq!(stored.pool_deficit, Some(1));
        assert_eq!(stored.pool_requested, Some(0));
    }

    /// 先到先得的核心测试：两家先后推来，缺口只有 1，先处理的买到、后处理的跳过。
    ///
    /// 这条同时验证三件事：`claim_next` 的全局 FIFO 决定先后、`processing_lock`
    /// 串行化、以及每次触发重算缺口（导入后凭据立即对下一次统计可见）。
    /// 三者缺一，两家就会各买一个、总共买 2 个，直接买超。
    #[tokio::test]
    async fn first_come_first_served_consumes_the_shared_deficit_exactly_once() {
        let first_token = "5".repeat(64);
        let second_token = "6".repeat(64);
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = calls.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move |body: axum::body::Bytes| {
                let observed = observed.clone();
                async move {
                    observed
                        .lock()
                        .unwrap()
                        .push(String::from_utf8(body.to_vec()).unwrap());
                    axum::Json(serde_json::json!({
                        "purchased": 1, "requested": 1, "remaining": 9, "total_debit": 30,
                        "keys": [{"key": "ksk_race", "price": 30}]
                    }))
                }
            }),
        );
        let base_url = server(app).await;

        let mut first = entry("io-first", SupplierKind::KiroAppIo, &first_token);
        first.settings.base_url = base_url.clone();
        first.settings.api_key = "km_secret".to_string();
        first.settings.min_purchase = 1;
        first.settings.max_purchase = 50;
        let mut second = entry("io-second", SupplierKind::KiroAppIo, &second_token);
        second.settings.base_url = base_url;
        second.settings.api_key = "km_secret".to_string();
        second.settings.min_purchase = 1;
        second.settings.max_purchase = 50;

        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::default());
        importer.set_pool_usable(0);
        // 生产的 add_credential 是同步写 entries，导入一返回就能被统计看到。
        // 号池闸的正确性依赖这个语义，替身必须模拟它。
        importer.grow_pool_on_import();
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![first, second],
            importer.clone(),
        )
        .with_pool_config(PoolRuntimeConfig {
            enabled: true,
            target_count: 1,
            low_quota_threshold: 0,
        });

        // 两家几乎同时推来，第一家的事件 id 更小（先落库）。
        service
            .ingest(&first_token, arrival("evt_race_1", &"7".repeat(32), 5))
            .unwrap();
        service
            .ingest(&second_token, arrival("evt_race_2", &"8".repeat(32), 5))
            .unwrap();
        assert_eq!(service.process_pending().await.unwrap(), 2);

        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "缺口只有 1，第二家不该再下单——否则就买超了"
        );
        assert_eq!(importer.credentials.lock().unwrap().len(), 1);

        let events = store.list(10, None, None).unwrap().items;
        let winner = events
            .iter()
            .find(|event| event.event_id == "evt_race_1")
            .unwrap();
        let loser = events
            .iter()
            .find(|event| event.event_id == "evt_race_2")
            .unwrap();
        assert_eq!(winner.status, SupplierEventStatus::Succeeded);
        assert_eq!(winner.purchased_count, 1);
        assert_eq!(winner.pool_usable, Some(0));
        assert_eq!(loser.status, SupplierEventStatus::Skipped);
        assert_eq!(loser.message.as_deref(), Some("号池已达目标存量"));
        // 后处理的那条看到的可用数已经是 1——重算缺口生效了。
        assert_eq!(loser.pool_usable, Some(1));
        assert_eq!(loser.pool_deficit, Some(0));
    }

    /// 号池闸启用时，各家自己的补货闸整个让位。
    ///
    /// 两套水位并存会交叉出第三种行为，而且「为什么没买」要同时看两处配置。
    #[tokio::test]
    async fn pool_gate_ignores_the_per_supplier_restock_gate() {
        let token = "9".repeat(64);
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move || {
                let observed = observed.clone();
                async move {
                    *observed.lock().unwrap() += 1;
                    axum::Json(serde_json::json!({
                        "purchased": 1, "requested": 1, "remaining": 9, "total_debit": 30,
                        "keys": [{"key": "ksk_override", "price": 30}]
                    }))
                }
            }),
        );
        let mut supplier = entry("io", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        supplier.settings.min_purchase = 1;
        supplier.settings.max_purchase = 50;
        // 逐家补货闸配成「名下还有 10 个可用就不买」，若它仍生效就会拦住本次采购。
        supplier.settings.restock_only_when_exhausted = true;
        supplier.settings.target_usable = 0;

        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::default());
        importer.set_health(
            "io",
            SupplierCredentialHealth {
                total: 10,
                usable: 10,
                ..Default::default()
            },
        );
        // 全局可用数 0 → 缺口 1，号池闸应放行。
        importer.set_pool_usable(0);
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer.clone(),
        )
        .with_pool_config(PoolRuntimeConfig {
            enabled: true,
            target_count: 1,
            low_quota_threshold: 250,
        });

        service
            .ingest(&token, arrival("evt_override", &"0".repeat(32), 5))
            .unwrap();
        service.process_pending().await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 1, "逐家补货闸不该再参与判定");
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Succeeded);
        // 逐家统计一次都没被查过，全局统计用的是号池自己的额度水位。
        assert!(
            importer.health_thresholds.lock().unwrap().is_empty(),
            "启用号池后不该再查逐家健康度"
        );
        assert_eq!(importer.pool_thresholds(), vec![250.0]);
    }

    /// 手动采购不受目标存量约束：那是人明确要买。
    #[tokio::test]
    async fn manual_purchase_bypasses_the_pool_target() {
        let token = "e".repeat(64);
        // 池子已满（目标 2、可用 2），自动采购会跳过。
        let (service, store, importer, calls) = pool_service(&token, 2, 2).await;

        let result = service.manual_purchase_from("io", 3).await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 1, "手动采购不该被号池拦住");
        assert_eq!(result.purchased, 3);
        assert_eq!(importer.credentials.lock().unwrap().len(), 3);
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.event_type, "manual_purchase");
        // 未经号池引擎，因此没有水位快照——这本身就是「这笔没走号池」的标记。
        assert_eq!(stored.pool_usable, None);
        assert_eq!(stored.pool_deficit, None);
    }

    /// 号池关闭时行为与本特性上线前一致：逐家 `maxPurchase` 说了算。
    #[tokio::test]
    async fn disabled_pool_keeps_the_legacy_per_supplier_behaviour() {
        let token = "d".repeat(64);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = seen.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move |body: axum::body::Bytes| {
                let observed = observed.clone();
                async move {
                    observed
                        .lock()
                        .unwrap()
                        .push(String::from_utf8(body.to_vec()).unwrap());
                    axum::Json(serde_json::json!({
                        "purchased": 4, "requested": 4, "remaining": 9, "total_debit": 120,
                        "keys": (0..4).map(|i| serde_json::json!({"key": format!("ksk_legacy_{i}")}))
                            .collect::<Vec<_>>()
                    }))
                }
            }),
        );
        let mut supplier = entry("io", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        supplier.settings.min_purchase = 1;
        supplier.settings.max_purchase = 4;
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::default());
        // 号池统计返回 0，若号池闸误生效会按缺口买；关闭时它根本不该被查。
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer.clone(),
        );

        service
            .ingest(&token, arrival("evt_legacy", &"c".repeat(32), 10))
            .unwrap();
        service.process_pending().await.unwrap();

        // 推送要 10 个，被逐家 maxPurchase 夹到 4 —— 与改动前一致。
        let sent: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0]).unwrap();
        assert_eq!(sent["count"], 4);
        assert!(
            importer.pool_calls.lock().unwrap().is_empty(),
            "号池关闭时不该查全局统计"
        );
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.pool_usable, None, "关闭时不写水位列");
    }

    /// 号池状态接口：纯读、可重复调用、如实反映四类拆分与两类识别计数。
    #[tokio::test]
    async fn pool_status_is_read_only_and_explains_the_numbers() {
        let token = "7".repeat(64);
        let (service, _store, importer, calls) = pool_service(&token, 5, 2).await;
        {
            let mut pool = importer.pool.lock().unwrap();
            pool.health.total = 6;
            pool.health.usable = 2;
            pool.health.dead = 3;
            pool.health.quota_exhausted = 1;
            pool.by_supplier_id = 4;
            pool.by_legacy_channel = 2;
        }

        let first = service.pool_status().unwrap();
        assert!(first.enabled);
        assert_eq!(first.target_count, 5);
        assert_eq!(first.global_usable, 2);
        assert_eq!(first.deficit, 3);
        // 「池里 6 个号怎么可用数只有 2」——答案在四类拆分里。
        assert_eq!(first.health.dead, 3);
        assert_eq!(first.health.quota_exhausted, 1);
        assert_eq!(first.by_supplier_id, 4);
        assert_eq!(first.by_legacy_channel, 2);
        // 「我买的号怎么没算进去」——对着这个列表比备注就有答案。
        assert_eq!(first.matched_channels, vec!["Webhook 自动采购".to_string()]);

        let second = service.pool_status().unwrap();
        assert_eq!(first.global_usable, second.global_usable);
        assert_eq!(first.matched_channels, second.matched_channels);
        assert_eq!(*calls.lock().unwrap(), 0, "状态查询不该发任何请求");
    }

    /// 备注为空的供货商不能把空串带进匹配集合。
    ///
    /// 空串会命中所有无备注凭据，把全部手工号算进水位，缺口顶成 0、自动采购静默
    /// 失效——而日志里只有一条「号池已达目标存量」。
    #[tokio::test]
    async fn blank_source_channel_is_excluded_from_the_matching_set() {
        let mut normal = entry("io", SupplierKind::KiroAppIo, &"8".repeat(64));
        normal.settings.source_channel = "Webhook 自动采购".to_string();
        let mut blank = entry("blank", SupplierKind::KiroAppIo, &"9".repeat(64));
        blank.settings.source_channel = "   ".to_string();
        let mut duplicate = entry("dup", SupplierKind::KiroAppIo, &"a".repeat(64));
        // 同名备注要去重，否则匹配集合会长出重复项。
        duplicate.settings.source_channel = "Webhook 自动采购".to_string();

        let importer = Arc::new(FakeImporter::default());
        let service = KeySupplierService::with_suppliers_and_importer(
            Arc::new(SupplierEventStore::open_in_memory().unwrap()),
            vec![normal, blank, duplicate],
            importer.clone(),
        )
        .with_pool_config(PoolRuntimeConfig {
            enabled: true,
            target_count: 3,
            low_quota_threshold: 0,
        });

        let status = service.pool_status().unwrap();
        assert_eq!(
            status.matched_channels,
            vec!["Webhook 自动采购".to_string()],
            "空备注不该进匹配集合，同名备注要去重"
        );
        // 传给统计函数的集合也必须已剔空。
        let (_, channels) = importer.pool_calls.lock().unwrap()[0].clone();
        assert_eq!(channels, vec!["Webhook 自动采购".to_string()]);
    }

    /// 生产实现必须覆盖 `pool_health`，不能落到返回全零的默认实现上。
    ///
    /// 默认实现返回全零意味着全局可用数恒为 0、缺口恒等于目标存量，会持续买到上限。
    /// 这个失效方向不会报任何错，只会一直花钱，所以需要一条契约测试兜住。
    #[test]
    fn production_importer_overrides_pool_health() {
        let path = temp_config_path("pool-health-contract");
        let mut credential = KiroCredentials {
            kiro_api_key: Some("ksk_pool_contract".to_owned()),
            auth_method: Some("api_key".to_owned()),
            api_region: Some(API_KEY_AUTH_REGION.to_owned()),
            ..Default::default()
        };
        credential.supplier_id = Some("io".to_owned());
        let token_manager = Arc::new(
            MultiTokenManager::new(
                Config::default(),
                vec![credential],
                None,
                Some(path.clone()),
                true,
            )
            .unwrap(),
        );
        let importer = TokenManagerCredentialImporter::new(token_manager);

        let pool = importer.pool_health(0.0, &HashSet::new());

        // 默认实现会返回全零。真正转发到 token manager 才能看到这个采购来的号。
        assert_eq!(
            pool.health.usable, 1,
            "生产实现落到了默认实现上：全局可用数恒为 0 会让缺口永远等于目标存量"
        );
        assert_eq!(pool.by_supplier_id, 1);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn purchase_cost_is_persisted_even_when_the_import_fails() {
        // 钱已经扣了但导入失败：走的是 fail_with_summary。金额必须跟着一起落库，
        // 否则预算累计会把这单算成 0 花费，越买越以为自己没花钱。
        let app = Router::new().route(
            "/api/me/purchase",
            post(|| async {
                axum::Json(serde_json::json!({
                    "purchased": 1, "requested": 1, "remaining": 5, "total_debit": 30,
                    "unit_price": 30, "order_id": "ord-io-fail",
                    "keys": [{"key": "ksk_io_lost", "price": 30}]
                }))
            }),
        );
        let token = "4".repeat(64);
        let mut supplier = entry("io", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::with_outcomes(vec![Err(anyhow::anyhow!(
            "磁盘写入失败"
        ))]));
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer,
        );

        service
            .ingest(
                &token,
                r#"{"event":"new_keys_available","event_id":"evt_io_fail",
                    "client_order_id":"d5c4fd9460b70fb8e944bd7faa519896","new_keys":1}"#,
            )
            .unwrap();
        service.process_pending().await.unwrap();

        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Failed);
        assert_eq!(stored.purchased_count, 1);
        assert_eq!(stored.failed_count, 1);
        assert_eq!(stored.total_debit, Some(30), "导入失败不能把花费抹成 0");
        assert_eq!(stored.unit_price, Some(30.0));
        assert_eq!(stored.supplier_order_id.as_deref(), Some("ord-io-fail"));
    }

    #[tokio::test]
    async fn order_conflict_is_skipped_with_an_actionable_reason_instead_of_failed() {
        // 409 = 原单已成交但参数不一致。记 failed 会让人反复点 retry 而每次都撞同一个
        // 409，付过钱的 key 一直挂在对方账上；记 skipped 并写清该去核对什么。
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move || {
                let observed = observed.clone();
                async move {
                    *observed.lock().unwrap() += 1;
                    (
                        axum::http::StatusCode::CONFLICT,
                        r#"{"error":"该订单号已存在且参数不一致"}"#,
                    )
                }
            }),
        );
        let token = "3".repeat(64);
        let mut supplier = entry("io", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::default());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer.clone(),
        );

        service
            .ingest(
                &token,
                r#"{"event":"new_keys_available","event_id":"evt_io_409",
                    "client_order_id":"d5c4fd9460b70fb8e944bd7faa519896","new_keys":1}"#,
            )
            .unwrap();
        // 一整轮处理不该因为这条事件而报错——它不是系统故障。
        assert_eq!(service.process_pending().await.unwrap(), 1);

        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        let message = stored.message.as_deref().unwrap_or_default();
        assert!(message.contains("已成交"), "{message}");
        assert!(message.contains("核对"), "{message}");
        // 没拿到 key 就不该导入任何东西。
        assert!(importer.credentials.lock().unwrap().is_empty());
        assert_eq!(*calls.lock().unwrap(), 1, "409 不该重试");
    }

    #[tokio::test]
    async fn kiroapp_io_insufficient_balance_is_skipped_rather_than_failed() {
        let app = Router::new().route(
            "/api/me/purchase",
            post(|| async { (axum::http::StatusCode::FORBIDDEN, r#"{"error":"余额不足"}"#) }),
        );
        let token = "6".repeat(64);
        let mut supplier = entry("io", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            Arc::new(FakeImporter::default()),
        );

        service
            .ingest(
                &token,
                r#"{"event":"new_keys_available","event_id":"evt_poor","new_keys":2}"#,
            )
            .unwrap();
        service.process_pending().await.unwrap();

        // 积分不够是「去充值」而不是「对方故障」，记 skipped 并写明原因。
        // 跳过原因写在 message 里（last_error 只在 failed 时写），和缺货跳过一致。
        let stored = &store.list(1, None, None).unwrap().items[0];
        assert_eq!(stored.status, SupplierEventStatus::Skipped);
        assert!(
            stored
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("充值"),
            "{:?}",
            stored.message
        );
        assert!(stored.last_error.is_none());
    }

    #[tokio::test]
    async fn transient_supplier_failure_is_deferred_and_replayed_with_the_same_request() {
        // 供货商广播「新一批已上架」的那一瞬间它自己往往还没准备好，采购接口短暂返 500。
        // 以前这条事件直接进 failed 终态（`claim_next` 只捡 received），一次几秒的抖动
        // 就等于永久丢一条到货通知。现在压回队列等自动重试，并钉住已经发出去的数量：
        // 订单号由 event_id 派生，重放时数量变了幂等协议会返 409（原单已成交、钱扣了、
        // key 没到手）——那恰好是重试要避免的结果。
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let observed = bodies.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move |body: axum::body::Bytes| {
                let observed = observed.clone();
                async move {
                    let mut bodies = observed.lock().unwrap();
                    bodies.push(String::from_utf8(body.to_vec()).unwrap());
                    // 一轮处理里客户端自己会试三次，都撞在同一个坏窗口上。
                    if bodies.len() <= 3 {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            r#"{"error":"服务暂时不可用"}"#.to_owned(),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            r#"{"purchased":2,"remaining":9,"total_debit":60,
                                "keys":[{"key":"ksk_a"},{"key":"ksk_b"}]}"#
                                .to_owned(),
                        )
                    }
                }
            }),
        );
        let token = "7".repeat(64);
        let mut supplier = entry("io", SupplierKind::KiroAppIo, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "km_secret".to_string();
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let importer = Arc::new(FakeImporter::default());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            importer.clone(),
        );

        service
            .ingest(
                &token,
                r#"{"event":"new_keys_available","event_id":"evt_flaky","new_keys":2}"#,
            )
            .unwrap();
        assert_eq!(service.process_pending().await.unwrap(), 1);

        let deferred = store.list(1, None, None).unwrap().items.remove(0);
        assert_eq!(deferred.status, SupplierEventStatus::Received);
        assert_eq!(deferred.purchase_count, Some(2));
        assert!(deferred.retry_after.is_some());
        assert!(
            deferred
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("500"),
            "{:?}",
            deferred.last_error
        );
        // 还没到点，这一轮不该再动它。
        assert_eq!(service.process_pending().await.unwrap(), 0);
        assert_eq!(bodies.lock().unwrap().len(), 3);

        // 人工重试跳过退避，第四次请求命中恢复后的供货商。
        store.retry(deferred.id).unwrap();
        assert_eq!(service.process_pending().await.unwrap(), 1);

        let settled = store.list(1, None, None).unwrap().items.remove(0);
        assert_eq!(settled.status, SupplierEventStatus::Succeeded);
        assert_eq!(settled.purchased_count, 2);
        assert_eq!(settled.imported_count, 2);
        assert_eq!(importer.credentials.lock().unwrap().len(), 2);

        // 重放必须逐字节等于第一次：同一订单号 + 同一数量才会命中幂等而不是 409。
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 4);
        assert_eq!(bodies[0], bodies[3]);
    }

    #[tokio::test]
    async fn non_idempotent_claim_failure_is_never_auto_retried() {
        // kiro-app 的 `/openapi/claim` 没有订单号。5xx 之后我们分不清积分扣没扣，
        // 自动重放一次就可能是第二笔扣款——只能停在 failed 让人工核对。
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/openapi/claim",
            post(move || {
                let observed = observed.clone();
                async move {
                    *observed.lock().unwrap() += 1;
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        r#"{"error":{"type":"server_error"}}"#,
                    )
                }
            }),
        );
        let token = "8".repeat(64);
        let mut supplier = entry("app", SupplierKind::KiroApp, &token);
        supplier.settings.base_url = server(app).await;
        supplier.settings.api_key = "app-secret".to_string();
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = KeySupplierService::with_suppliers_and_importer(
            store.clone(),
            vec![supplier],
            Arc::new(FakeImporter::default()),
        );

        service
            .ingest(&token, r#"{"id":"batch-app-500","count":1}"#)
            .unwrap();
        service.process_pending().await.unwrap();

        let failed = store.list(1, None, None).unwrap().items.remove(0);
        assert_eq!(failed.status, SupplierEventStatus::Failed);
        assert!(failed.retry_after.is_none());
        assert!(failed.purchase_count.is_none());
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn auto_retry_is_bounded_and_only_covers_transient_failures() {
        use crate::admin::key_supplier::client::SupplierError;

        let store = SupplierEventStore::open_in_memory().unwrap();
        store
            .insert_event(IncomingSupplierEvent {
                supplier_id: "io".to_string(),
                event_id: "evt".to_string(),
                event_type: "new_keys_available".to_string(),
                purchase_order_id: Some("a".repeat(32)),
                supplier_batch_id: None,
                event_region: None,
                message: None,
                quantity: 1,
            })
            .unwrap();
        let mut event = store.claim_next().unwrap().unwrap();

        let deferred = defer_or_fail(
            &event,
            true,
            Some(1),
            SupplierError::Http {
                status: 500,
                message: "boom".to_string(),
            },
        );
        assert!(matches!(
            deferred,
            Ok(ProcessAction::Deferred {
                purchase_count: Some(1),
                ..
            })
        ));

        // 4xx 重放一万次也是同一个结果，直接失败。
        for status in [400, 401, 404] {
            let error = SupplierError::Http {
                status,
                message: "nope".to_string(),
            };
            assert!(
                defer_or_fail(&event, true, Some(1), error).is_err(),
                "{status} 不该自动重试"
            );
        }

        // 429 给了建议等待时间就听它的，别比它更急。这是 `retry_after` 第一次被用上。
        let Ok(ProcessAction::Deferred { retry_after, .. }) = defer_or_fail(
            &event,
            true,
            None,
            SupplierError::RateLimited {
                retry_after: Some(600),
                message: "slow down".to_string(),
            },
        ) else {
            panic!("429 应当压回队列");
        };
        assert!(retry_after >= Utc::now() + ChronoDuration::seconds(590));

        // 手动采购不自动重试：人就在旁边等，十分钟后突然冒出一笔扣款是惊吓。
        let mut manual = event.clone();
        manual.event_type = "manual_purchase".to_string();
        assert!(
            defer_or_fail(
                &manual,
                true,
                Some(1),
                SupplierError::Network("reset".to_string())
            )
            .is_err()
        );

        // 重试额度用尽后转终态，免得一条坏事件永远在队列里打转。
        event.attempts = EVENT_RETRY_DELAYS.len() as i64 + 1;
        assert!(
            defer_or_fail(
                &event,
                true,
                Some(1),
                SupplierError::Network("reset".to_string())
            )
            .is_err()
        );
    }

    #[test]
    fn kiroapp_stable_event_id_is_reused_across_body_changes() {
        // 对方给了稳定 id 时以它为准：同一批次即使 body 其它字段变了也判重。
        let first = IncomingWebhook::parse(SupplierKind::KiroApp, br#"{"id":"batch-9","count":1}"#)
            .unwrap();
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
                get(|| async {
                    axum::Json(serde_json::json!({"availableKeys": 5, "keyPrice": 1.5}))
                }),
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

        service
            .ingest(&token, r#"{"event":"stock.ready"}"#)
            .unwrap();
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
        let (service, path) =
            multi_service(vec![entry("app", SupplierKind::KiroApp, &"1".repeat(64))]);

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
        let (service, path) =
            multi_service(vec![entry("first", SupplierKind::KiroRs, &"a".repeat(64))]);

        let mut update = SupplierEntryUpdate {
            id: Some("kiroapp".to_owned()),
            name: "kiroapp.cc".to_owned(),
            kind: SupplierKind::KiroApp,
            enabled: true,
            import_overrides: None,
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
        let (after, migrated) =
            crate::admin::key_supplier::config::load_suppliers(&config).unwrap();
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
