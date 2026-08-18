//! 每个号能产生多少人民币（credential_earnings）
//!
//! 号池此前只能看到「积分」，没法回答运营真正关心的问题：这个号赚回本了吗、还能再赚
//! 多少、平均活多久才够本。
//!
//! # 换算链
//!
//! ```text
//! 上游 meteringEvent.usage  →  credits（Kiro 计费单位）
//! NewAPI 消费流水 quota      →  ¥ 收入（quota / quotaPerUnit）
//! ```
//!
//! 两边靠 `trace_id` 一一对应，所以「1 个 credit 能卖出多少钱」**不需要人工配置，能实测**：
//!
//! ```text
//! 实测卖价 = 窗口内 NewAPI 收入(¥) / 同窗口内消耗的 credits
//! ```
//!
//! 这个比例刻意不写死。它随模型结构漂移——同样 2000 积分，跑 opus 的号和跑 haiku 的号
//! 能卖的钱差好几倍——所以只认最近一次实测值，并把样本量一起暴露出去，让运营自己判断
//! 这个数可不可信。
//!
//! # 成本为什么要手填
//!
//! `KiroCredentials::purchase_price` 记的是供货商积分，各家单位不同又没有汇率，换不出
//! 人民币。而且不同渠道、不同批次的进价差别很大。所以买入价与额度都做成可手填字段
//! （[`crate::kiro::model::credentials::KiroCredentials::cost_rmb`] /
//! `quota_credits`），填了才算，没填就只报收入不报利润——宁可留空，不要给一个假的利润数。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use parking_lot::Mutex;

/// 实测卖价：1 个上游 credit 能换来多少人民币收入
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SellRate {
    /// ¥ / credit
    pub rmb_per_credit: f64,
    /// 实测窗口内的收入合计（¥）
    pub revenue_rmb: f64,
    /// 实测窗口内消耗的 credits 合计
    pub credits: f64,
    /// 参与实测的 NewAPI 流水条数。太小的话这个卖价不可信。
    pub samples: u64,
    /// 实测时间（RFC3339）
    pub measured_at: String,
    /// 实测覆盖的时间窗口（分钟）
    pub window_minutes: u64,
}

/// 单个号的收益核算。
///
/// 所有金额字段都是 `Option`：缺少卖价或买入价时保持 `None`，由前端显示为「未知」。
/// 用 0 代替未知会让汇总出来的利润凭空变好看。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEarnings {
    /// 额度积分（手填优先，其次上游 `usageLimit`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_credits: Option<f64>,
    /// 额度这个数是哪来的
    pub quota_source: QuotaSource,
    /// 我们通过这个号实际消耗掉的 credits。
    ///
    /// 取自本地计量而不是上游 `currentUsage`：后者可能包含我们买到之前的消耗，
    /// 那部分不是我们的收入。
    pub credits_used: f64,
    /// 还剩多少 credits 可用
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits_remaining: Option<f64>,
    /// 已产生的收入（¥）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revenue_rmb: Option<f64>,
    /// 剩余额度还能产生多少（¥）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_rmb: Option<f64>,
    /// 买入成本（¥，手填）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_rmb: Option<f64>,
    /// 净利润 = 已产生 − 成本
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profit_rmb: Option<f64>,
    /// 回本进度。1.0 = 刚好回本，>1 = 已盈利。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payback_ratio: Option<f64>,
    /// 从加入到现在（或到判死）的小时数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alive_hours: Option<f64>,
    /// 每存活小时产出（¥/小时）。跨渠道比较用这个口径：
    /// 单价便宜但活得短的号，实际可能更贵。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revenue_per_hour: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QuotaSource {
    /// 查不到也没手填
    #[default]
    Unknown,
    /// 运营手填
    Manual,
    /// 上游 getUsageLimits
    Upstream,
}

/// 一个号的输入事实，由调用方从凭据 / 余额缓存 / 计量数据里凑好
#[derive(Debug, Clone, Default)]
pub struct EarningsInput {
    /// 手填额度积分
    pub manual_quota_credits: Option<f64>,
    /// 上游查到的额度上限
    pub upstream_usage_limit: Option<f64>,
    /// 上游查到的已用额度（仅在没有本地计量数据时兜底）
    pub upstream_current_usage: Option<f64>,
    /// 本地计量到的 credits 消耗
    pub metered_credits: Option<f64>,
    /// 手填买入价（¥）
    pub cost_rmb: Option<f64>,
    /// 存活小时数
    pub alive_hours: Option<f64>,
}

/// 算一个号的收益。`sell_rate` 为 None（还没实测出卖价）时只填积分维度，金额留空。
pub fn compute(input: &EarningsInput, sell_rate: Option<&SellRate>) -> CredentialEarnings {
    let (quota_credits, quota_source) = match (input.manual_quota_credits, input.upstream_usage_limit)
    {
        (Some(manual), _) if manual > 0.0 => (Some(manual), QuotaSource::Manual),
        (_, Some(upstream)) if upstream > 0.0 => (Some(upstream), QuotaSource::Upstream),
        _ => (None, QuotaSource::Unknown),
    };

    // 本地计量优先。上游 currentUsage 只在没有本地数据时兜底：它可能含我们买到
    // 之前的消耗，把那部分算成我们的收入会虚高。
    let credits_used = input
        .metered_credits
        .or(input.upstream_current_usage)
        .unwrap_or(0.0)
        .max(0.0);

    let credits_remaining = quota_credits.map(|q| (q - credits_used).max(0.0));

    let rate = sell_rate.map(|r| r.rmb_per_credit).filter(|r| *r > 0.0);
    let revenue_rmb = rate.map(|r| credits_used * r);
    let remaining_rmb = rate.zip(credits_remaining).map(|(r, left)| left * r);

    let cost_rmb = input.cost_rmb.filter(|c| *c > 0.0);
    let profit_rmb = revenue_rmb.zip(cost_rmb).map(|(rev, cost)| rev - cost);
    let payback_ratio = revenue_rmb.zip(cost_rmb).map(|(rev, cost)| rev / cost);

    let alive_hours = input.alive_hours.filter(|h| *h > 0.0);
    let revenue_per_hour = revenue_rmb.zip(alive_hours).map(|(rev, h)| rev / h);

    CredentialEarnings {
        quota_credits,
        quota_source,
        credits_used,
        credits_remaining,
        revenue_rmb,
        remaining_rmb,
        cost_rmb,
        profit_rmb,
        payback_ratio,
        alive_hours,
        revenue_per_hour,
    }
}

/// 号池级汇总
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EarningsSummary {
    /// 参与统计的号数
    pub accounts: usize,
    /// 其中填了买入价的号数。没填的不进成本与利润，只进收入。
    pub accounts_with_cost: usize,
    pub total_cost_rmb: f64,
    pub total_revenue_rmb: f64,
    /// 剩余额度还能产生多少（¥）——这是号池的存货价值
    pub total_remaining_rmb: f64,
    pub profit_rmb: f64,
    pub margin_pct: f64,
    /// 已回本的号数（收入 >= 成本）
    pub paid_back_accounts: usize,
    /// 平均每号每存活小时产出（¥/小时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revenue_per_hour: Option<f64>,
    /// 回本需要活多久（小时）= 平均成本 / 平均时薪。
    ///
    /// 把它和实测存活时长放在一起，就能直接回答「按现在的封号速度，这批号到底赚不赚钱」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payback_hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sell_rate: Option<SellRate>,
}

/// 汇总一批号的收益。只统计有成本的号的成本，避免拿部分成本除全部收入得出假毛利率。
pub fn summarize(items: &[CredentialEarnings], sell_rate: Option<SellRate>) -> EarningsSummary {
    let mut s = EarningsSummary {
        accounts: items.len(),
        sell_rate,
        ..Default::default()
    };
    let mut hourly: Vec<f64> = Vec::new();
    let mut costs: Vec<f64> = Vec::new();

    for e in items {
        s.total_revenue_rmb += e.revenue_rmb.unwrap_or(0.0);
        s.total_remaining_rmb += e.remaining_rmb.unwrap_or(0.0);
        if let Some(cost) = e.cost_rmb {
            s.accounts_with_cost += 1;
            s.total_cost_rmb += cost;
            costs.push(cost);
        }
        if e.payback_ratio.is_some_and(|r| r >= 1.0) {
            s.paid_back_accounts += 1;
        }
        if let Some(h) = e.revenue_per_hour {
            hourly.push(h);
        }
    }

    s.profit_rmb = s.total_revenue_rmb - s.total_cost_rmb;
    s.margin_pct = if s.total_revenue_rmb > 0.0 {
        s.profit_rmb / s.total_revenue_rmb * 100.0
    } else {
        0.0
    };

    if !hourly.is_empty() {
        let avg = hourly.iter().sum::<f64>() / hourly.len() as f64;
        s.revenue_per_hour = Some(avg);
        if avg > 0.0 && !costs.is_empty() {
            let avg_cost = costs.iter().sum::<f64>() / costs.len() as f64;
            s.payback_hours = Some(avg_cost / avg);
        }
    }
    s
}

// ============ 卖价的持久化 ============

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoreData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sell_rate: Option<SellRate>,
}

/// 实测卖价的缓存。
///
/// 必须落盘：凭据列表接口不能每次都去打 NewAPI（慢且有配额），所以只在跑利润报表时
/// 更新一次，之后一直用缓存。重启不该把它丢掉。
pub struct SellRateStore {
    data: Mutex<StoreData>,
    path: Option<PathBuf>,
}

impl SellRateStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let data = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<StoreData>(&s).ok())
            .unwrap_or_default();
        Self {
            data: Mutex::new(data),
            path,
        }
    }

    pub fn get(&self) -> Option<SellRate> {
        self.data.lock().sell_rate.clone()
    }

    /// 从一次利润报表的结果里提取卖价。
    ///
    /// `credits <= 0` 或口径未确认时不写：那种情况下算出来的比例没有意义，
    /// 留着上一次的实测值比换成一个假数更好。
    pub fn record_from_report(
        &self,
        revenue_rmb: f64,
        credits: f64,
        samples: u64,
        window_minutes: u64,
        scope_confirmed: bool,
    ) -> Option<SellRate> {
        if !scope_confirmed || credits <= 0.0 || revenue_rmb <= 0.0 {
            return None;
        }
        let rate = SellRate {
            rmb_per_credit: revenue_rmb / credits,
            revenue_rmb,
            credits,
            samples,
            measured_at: chrono::Utc::now().to_rfc3339(),
            window_minutes,
        };
        self.data.lock().sell_rate = Some(rate.clone());
        self.persist();
        tracing::info!(
            rmb_per_credit = rate.rmb_per_credit,
            revenue_rmb,
            credits,
            samples,
            window_minutes,
            "已更新实测卖价（¥/credit）"
        );
        Some(rate)
    }

    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let json = {
            let data = self.data.lock();
            match serde_json::to_string_pretty(&*data) {
                Ok(j) => j,
                Err(error) => {
                    tracing::warn!(%error, "实测卖价序列化失败");
                    return;
                }
            }
        };
        if let Err(error) =
            atomicwrites::AtomicFile::new(path, atomicwrites::OverwriteBehavior::AllowOverwrite)
                .write(|f| std::io::Write::write_all(f, json.as_bytes()))
        {
            tracing::warn!(%error, path = %path.display(), "实测卖价落盘失败");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(rmb_per_credit: f64) -> SellRate {
        SellRate {
            rmb_per_credit,
            revenue_rmb: 100.0,
            credits: 100.0 / rmb_per_credit,
            samples: 500,
            measured_at: "2026-08-18T00:00:00+00:00".to_string(),
            window_minutes: 60,
        }
    }

    #[test]
    fn manual_quota_wins_over_upstream() {
        // 卖家标称与上游返回不一致时以手填为准；上游查不到也还能算
        let e = compute(
            &EarningsInput {
                manual_quota_credits: Some(2000.0),
                upstream_usage_limit: Some(1500.0),
                metered_credits: Some(500.0),
                ..Default::default()
            },
            Some(&rate(0.05)),
        );
        assert_eq!(e.quota_credits, Some(2000.0));
        assert_eq!(e.quota_source, QuotaSource::Manual);
        assert_eq!(e.credits_remaining, Some(1500.0));
    }

    #[test]
    fn falls_back_to_upstream_quota_then_unknown() {
        let e = compute(
            &EarningsInput {
                upstream_usage_limit: Some(2000.0),
                metered_credits: Some(100.0),
                ..Default::default()
            },
            Some(&rate(0.05)),
        );
        assert_eq!(e.quota_source, QuotaSource::Upstream);
        assert_eq!(e.credits_remaining, Some(1900.0));

        let e = compute(&EarningsInput::default(), Some(&rate(0.05)));
        assert_eq!(e.quota_source, QuotaSource::Unknown);
        assert_eq!(e.credits_remaining, None);
        // 额度未知时不该编一个剩余金额
        assert_eq!(e.remaining_rmb, None);
    }

    #[test]
    fn local_metering_beats_upstream_current_usage() {
        // 上游 currentUsage 可能含我们买到之前的消耗，算成我们的收入会虚高
        let e = compute(
            &EarningsInput {
                manual_quota_credits: Some(2000.0),
                upstream_current_usage: Some(1800.0),
                metered_credits: Some(300.0),
                ..Default::default()
            },
            Some(&rate(0.05)),
        );
        assert_eq!(e.credits_used, 300.0);
        assert_eq!(e.revenue_rmb, Some(15.0));
    }

    #[test]
    fn without_sell_rate_amounts_stay_unknown_not_zero() {
        // 没实测出卖价时金额必须留空。填 0 会让汇总出来的利润凭空变好看。
        let e = compute(
            &EarningsInput {
                manual_quota_credits: Some(2000.0),
                metered_credits: Some(500.0),
                cost_rmb: Some(45.0),
                ..Default::default()
            },
            None,
        );
        assert_eq!(e.credits_used, 500.0);
        assert_eq!(e.credits_remaining, Some(1500.0));
        assert_eq!(e.revenue_rmb, None);
        assert_eq!(e.profit_rmb, None);
        assert_eq!(e.payback_ratio, None);
    }

    #[test]
    fn without_cost_reports_revenue_but_not_profit() {
        let e = compute(
            &EarningsInput {
                manual_quota_credits: Some(2000.0),
                metered_credits: Some(1000.0),
                cost_rmb: None,
                ..Default::default()
            },
            Some(&rate(0.05)),
        );
        assert_eq!(e.revenue_rmb, Some(50.0));
        assert_eq!(e.profit_rmb, None, "没填成本就不该给出利润数");
    }

    #[test]
    fn payback_and_hourly_rate() {
        let e = compute(
            &EarningsInput {
                manual_quota_credits: Some(2000.0),
                metered_credits: Some(1200.0),
                cost_rmb: Some(45.0),
                alive_hours: Some(2.0),
                ..Default::default()
            },
            Some(&rate(0.05)),
        );
        assert_eq!(e.revenue_rmb, Some(60.0));
        assert_eq!(e.profit_rmb, Some(15.0));
        assert_eq!(e.payback_ratio, Some(60.0 / 45.0));
        assert_eq!(e.revenue_per_hour, Some(30.0));
    }

    #[test]
    fn summary_only_counts_costs_that_were_filled_in() {
        // 拿部分成本去除全部收入会得出一个假的高毛利率
        let filled = compute(
            &EarningsInput {
                manual_quota_credits: Some(2000.0),
                metered_credits: Some(1200.0),
                cost_rmb: Some(45.0),
                alive_hours: Some(2.0),
                ..Default::default()
            },
            Some(&rate(0.05)),
        );
        let unfilled = compute(
            &EarningsInput {
                manual_quota_credits: Some(2000.0),
                metered_credits: Some(1200.0),
                alive_hours: Some(2.0),
                ..Default::default()
            },
            Some(&rate(0.05)),
        );
        let s = summarize(&[filled, unfilled], Some(rate(0.05)));
        assert_eq!(s.accounts, 2);
        assert_eq!(s.accounts_with_cost, 1);
        assert_eq!(s.total_revenue_rmb, 120.0);
        assert_eq!(s.total_cost_rmb, 45.0);
        assert_eq!(s.paid_back_accounts, 1);
        // 回本小时数 = 平均成本 45 / 平均时薪 30
        assert_eq!(s.payback_hours, Some(1.5));
    }

    #[test]
    fn sell_rate_is_not_overwritten_by_meaningless_reports() {
        let store = SellRateStore::new(None);
        assert!(store.get().is_none());

        // 口径未确认 / 没有 credits / 没有收入，都不该写进缓存
        assert!(store.record_from_report(100.0, 2000.0, 500, 60, false).is_none());
        assert!(store.record_from_report(100.0, 0.0, 500, 60, true).is_none());
        assert!(store.record_from_report(0.0, 2000.0, 500, 60, true).is_none());
        assert!(store.get().is_none());

        let got = store
            .record_from_report(100.0, 2000.0, 500, 60, true)
            .expect("正常报表应写入");
        assert_eq!(got.rmb_per_credit, 0.05);
        assert_eq!(store.get().map(|r| r.rmb_per_credit), Some(0.05));

        // 之后来一份无效报表，不该把已实测的值抹掉
        assert!(store.record_from_report(50.0, 0.0, 10, 60, true).is_none());
        assert_eq!(store.get().map(|r| r.rmb_per_credit), Some(0.05));
    }
}
