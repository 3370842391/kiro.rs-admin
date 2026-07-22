//! 批发业务逻辑（P2 探活 + P3 号池同步/质保 + ksk 交付）
//!
//! 依赖：`WholesaleStore`（数据）+ `MultiTokenManager`（探活 / 建 ksk）+ `health`（分类）。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::kiro::token_manager::MultiTokenManager;

use super::health::{classify_from_error_text, AccountHealth};
use super::store::{holding_status, mother_state, Holding, SharedWholesaleStore};

/// 母号自动退役阈值：近 1h 该母号名下判死 >= 此数 → 母号置 dead。
const MOTHER_DEATH_THRESHOLD_1H: i64 = 3;

/// 批发服务配置（可由 config.json 覆盖，先用常量默认）
#[derive(Debug, Clone)]
pub struct WholesaleConfig {
    pub price_cents: i64,
    pub warranty_secs: i64,
    /// 探活频率（秒），界面相对实时用 60~120
    pub probe_interval_secs: u64,
    /// 交付前是否额外发一次极小推理确认（暂不实现，占位）
    pub validate_with_inference: bool,
}

impl Default for WholesaleConfig {
    fn default() -> Self {
        Self {
            price_cents: super::store::DEFAULT_PRICE_CENTS,
            warranty_secs: super::store::DEFAULT_WARRANTY_SECS,
            probe_interval_secs: 90,
            validate_with_inference: false,
        }
    }
}

pub struct WholesaleService {
    pub store: SharedWholesaleStore,
    pub token_manager: Arc<MultiTokenManager>,
    pub config: WholesaleConfig,
}

/// sync 结果中缺货说明
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shortfall {
    pub reason: String, // insufficient_balance / pool_exhausted
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing: Option<i64>,
}

/// sync 汇总结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub uid: String,
    pub target: i64,
    pub alive: i64,
    pub added_this_call: i64,
    pub charged_cents: i64,
    pub balance_cents: i64,
    pub pool: Vec<PoolItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shortfall: Option<Shortfall>,
}

/// 号池里对客户展示的单个号
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolItem {
    pub public_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>, // 仅活号返回;死号不返回 key
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    pub mother_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<String>,
    pub born_at: String,
    pub warranty_until: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub died_at: Option<String>,
    /// 存活秒数(死号:died_at-born_at;活号:now-born_at)
    pub alive_secs: i64,
}

impl WholesaleService {
    pub fn new(
        store: SharedWholesaleStore,
        token_manager: Arc<MultiTokenManager>,
        config: WholesaleConfig,
    ) -> Self {
        Self { store, token_manager, config }
    }

    /// 探活一个凭据 → 健康度。用 ListAvailableModels，403(封号)即死。
    pub async fn probe_credential(&self, credential_id: u64) -> AccountHealth {
        match self.token_manager.get_available_models_for(credential_id).await {
            Ok(_) => AccountHealth::Active,
            Err(e) => {
                let text = format!("{e:#}");
                classify_from_error_text(&text)
            }
        }
    }

    /// 交付一个号:占号→验活→建 ksk→扣费入库。
    /// 成功返回 Holding;验活失败/建号失败会释放号并返回 Err(可继续换下一个);
    /// 余额不足返回特殊 Err("insufficient_balance")。
    async fn deliver_one(
        &self,
        customer_id: i64,
        region: Option<&str>,
        exclude_mothers: &[String],
        label_prefix: &str,
    ) -> Result<Option<Holding>, String> {
        // 1) 原子占号(排除死号母号,只挑新鲜/活跃母号)
        let acc = self
            .store
            .reserve_account(region, exclude_mothers, false)
            .map_err(|e| format!("occupy_failed: {e}"))?;
        let acc = match acc {
            Some(a) => a,
            None => return Ok(None), // 池空
        };

        // 2) 验活
        let health = self.probe_credential(acc.credential_id as u64).await;
        if !health.is_alive() {
            // 死号:移出池并记母号死亡;瞬态:也先释放(下轮再挑)
            if health.is_dead() {
                let _ = self.store.record_child_death(&acc.mother_id);
                let _ = self.store.set_mother_state_if_dying(&acc.mother_id, MOTHER_DEATH_THRESHOLD_1H);
            }
            let _ = self.store.release_account(acc.credential_id);
            return Err(format!("account_not_alive:{}", health.as_status_str()));
        }

        // 3) 建 ksk
        let label = format!("{label_prefix}-{}", acc.credential_id);
        let (ksk_id, raw_key) = match self.token_manager.create_ksk_for(acc.credential_id as u64, &label).await {
            Ok(v) => v,
            Err(e) => {
                let _ = self.store.release_account(acc.credential_id);
                return Err(format!("ksk_failed: {e}"));
            }
        };

        // 4) public_id
        let public_id = self
            .store
            .public_id_for(acc.credential_id)
            .ok()
            .flatten()
            .unwrap_or_else(|| format!("ACC-{}", acc.credential_id));

        // 5) 扣费 + 入库(事务)。ksk_cipher 先明文存(P7 再加密)
        let cipher = raw_key.clone().into_bytes();
        match self.store.deliver_holding(
            customer_id,
            acc.credential_id,
            &acc.mother_id,
            &public_id,
            Some(&ksk_id),
            Some(&cipher),
            acc.region.as_deref(),
            self.config.price_cents,
            self.config.warranty_secs,
        ) {
            Ok(mut h) => {
                // 把明文 key 挂到返回体(仅本次响应用,不进日志)
                h.ksk_cipher = Some(raw_key.into_bytes());
                Ok(Some(h))
            }
            Err(e) => {
                // 扣费失败(余额不足):号已建但没入库。释放号,余额不足向上传
                let _ = self.store.release_account(acc.credential_id);
                Err(e) // "insufficient_balance"
            }
        }
    }

    /// 号池同步:把客户名下正常号补齐到 target。
    /// 这就是 /wholesale/sync 的核心。需在客户级串行锁下调用(防补成 2N)。
    pub async fn sync_pool(
        &self,
        customer_id: i64,
        target_override: Option<i64>,
        region: Option<&str>,
        exclude_mothers: &[String],
        label_prefix: &str,
    ) -> Result<SyncResult, String> {
        let customer = self
            .store
            .customer_by_id(customer_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "customer_not_found".to_string())?;
        if customer.disabled {
            return Err("customer_disabled".to_string());
        }
        let target = target_override.unwrap_or(customer.target).max(0);

        let alive = self.store.active_count(customer_id).map_err(|e| e.to_string())?;
        let mut need = (target - alive).max(0);
        let mut added = 0i64;
        let mut charged = 0i64;
        let mut shortfall = None;

        while need > 0 {
            // 余额不足 → 停止获取
            let bal = self.store.customer_balance(customer_id).map_err(|e| e.to_string())?;
            if bal < self.config.price_cents {
                shortfall = Some(Shortfall { reason: "insufficient_balance".to_string(), missing: Some(need) });
                break;
            }
            match self.deliver_one(customer_id, region, exclude_mothers, label_prefix).await {
                Ok(Some(_h)) => {
                    added += 1;
                    charged += self.config.price_cents;
                    need -= 1;
                }
                Ok(None) => {
                    // 池空
                    shortfall = Some(Shortfall { reason: "pool_exhausted".to_string(), missing: Some(need) });
                    break;
                }
                Err(e) if e == "insufficient_balance" => {
                    shortfall = Some(Shortfall { reason: "insufficient_balance".to_string(), missing: Some(need) });
                    break;
                }
                Err(_e) => {
                    // 验活失败/建号失败:换下一个号,不消耗 need 计数上限保护
                    // 防死循环:给一个尝试预算(need*3 次)
                    // 简化:用一个独立计数器
                    // 见下方 attempts 保护
                    return self.sync_with_attempt_budget(
                        customer_id, target, region, exclude_mothers, label_prefix, added, charged,
                    ).await;
                }
            }
        }

        self.build_sync_result(&customer.uid, customer_id, target, added, charged, shortfall)
    }

    /// 带尝试预算的补号(验活失败会换号,避免坏号拖垮):最多尝试 need*4 次。
    #[allow(clippy::too_many_arguments)]
    async fn sync_with_attempt_budget(
        &self,
        customer_id: i64,
        target: i64,
        region: Option<&str>,
        exclude_mothers: &[String],
        label_prefix: &str,
        mut added: i64,
        mut charged: i64,
    ) -> Result<SyncResult, String> {
        let customer_uid = self
            .store
            .customer_by_id(customer_id)
            .map_err(|e| e.to_string())?
            .map(|c| c.uid)
            .unwrap_or_default();
        let alive = self.store.active_count(customer_id).map_err(|e| e.to_string())?;
        let mut need = (target - alive).max(0);
        let mut attempts = 0i64;
        let budget = (need * 4).max(4);
        let mut shortfall = None;

        while need > 0 && attempts < budget {
            attempts += 1;
            let bal = self.store.customer_balance(customer_id).map_err(|e| e.to_string())?;
            if bal < self.config.price_cents {
                shortfall = Some(Shortfall { reason: "insufficient_balance".to_string(), missing: Some(need) });
                break;
            }
            match self.deliver_one(customer_id, region, exclude_mothers, label_prefix).await {
                Ok(Some(_)) => { added += 1; charged += self.config.price_cents; need -= 1; }
                Ok(None) => {
                    shortfall = Some(Shortfall { reason: "pool_exhausted".to_string(), missing: Some(need) });
                    break;
                }
                Err(e) if e == "insufficient_balance" => {
                    shortfall = Some(Shortfall { reason: "insufficient_balance".to_string(), missing: Some(need) });
                    break;
                }
                Err(_) => continue, // 坏号,换下一个
            }
        }
        if need > 0 && shortfall.is_none() {
            // 预算耗尽仍没补够 → 视为池里坏号太多
            shortfall = Some(Shortfall { reason: "pool_exhausted".to_string(), missing: Some(need) });
        }
        self.build_sync_result(&customer_uid, customer_id, target, added, charged, shortfall)
    }

    fn build_sync_result(
        &self,
        uid: &str,
        customer_id: i64,
        target: i64,
        added: i64,
        charged: i64,
        shortfall: Option<Shortfall>,
    ) -> Result<SyncResult, String> {
        let pool = self.current_pool(customer_id, true)?;
        let alive = self.store.active_count(customer_id).map_err(|e| e.to_string())?;
        let balance = self.store.customer_balance(customer_id).map_err(|e| e.to_string())?;
        Ok(SyncResult {
            uid: uid.to_string(),
            target,
            alive,
            added_this_call: added,
            charged_cents: charged,
            balance_cents: balance,
            pool,
            shortfall,
        })
    }

    /// 组装客户当前号池视图(含活号 key)。`with_key` 控制是否附 api_key 明文。
    pub fn current_pool(&self, customer_id: i64, with_key: bool) -> Result<Vec<PoolItem>, String> {
        let holdings = self.store.holdings_for_customer(customer_id, false).map_err(|e| e.to_string())?;
        Ok(holdings.into_iter().map(|h| Self::to_pool_item(h, with_key)).collect())
    }

    fn to_pool_item(h: Holding, with_key: bool) -> PoolItem {
        let born = chrono::DateTime::parse_from_rfc3339(&h.born_at).ok();
        let end = h
            .died_at
            .as_deref()
            .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        let alive_secs = born
            .map(|b| (end - b.with_timezone(&chrono::Utc)).num_seconds().max(0))
            .unwrap_or(0);
        let api_key = if with_key && h.status == holding_status::ACTIVE {
            h.ksk_cipher.as_ref().and_then(|c| String::from_utf8(c.clone()).ok())
        } else {
            None
        };
        PoolItem {
            public_id: h.public_id,
            api_key,
            status: h.status,
            region: h.region,
            mother_id: h.mother_id,
            quota: h.last_quota,
            born_at: h.born_at,
            warranty_until: h.warranty_until,
            died_at: h.died_at,
            alive_secs,
        }
    }

    /// 后台探活一轮:扫所有活着的 holdings,更新状态;命中死号触发质保 + 母号联动。
    pub async fn probe_round(&self) {
        let targets = match self.store.holdings_to_probe() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("wholesale 探活取列表失败: {e}");
                return;
            }
        };
        for (holding_id, credential_id, mother_id, _customer_id, _warranty, _status) in targets {
            let health = self.probe_credential(credential_id as u64).await;
            match health {
                AccountHealth::Dead => {
                    // 判死 + 质保退款(在保且未退)
                    match self.store.mark_holding_dead(holding_id) {
                        Ok(refunded) => {
                            tracing::info!(
                                holding_id, credential_id, refunded,
                                "wholesale 探活判死,已处理质保"
                            );
                        }
                        Err(e) => tracing::warn!(holding_id, "wholesale 判死失败: {e}"),
                    }
                    // 母号死亡联动
                    if let Ok(deaths) = self.store.record_child_death(&mother_id) {
                        if deaths >= MOTHER_DEATH_THRESHOLD_1H {
                            let _ = self.store.set_mother_state(&mother_id, mother_state::DEAD);
                            tracing::warn!(mother_id, deaths, "母号近1h死亡数超阈值,已置 dead");
                        }
                    }
                }
                AccountHealth::LowQuota => {
                    let _ = self.store.update_holding_probe(holding_id, holding_status::LOW_QUOTA, None);
                }
                AccountHealth::Active => {
                    let _ = self.store.update_holding_probe(holding_id, holding_status::ACTIVE, None);
                }
                // 瞬态/未知:不改终态,只更新探测时间(status 传原值 active 保守)
                AccountHealth::TransientAuth | AccountHealth::Unknown(_) => {
                    // 不动 status,仅记录一次探测(用 active 占位会误伤 low_quota;这里跳过更新)
                }
            }
        }
    }
}
