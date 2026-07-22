//! NewAPI 收入与 RS 上游 Credits 成本的利润领域模型。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// ¥45 / 2000 Credits。
pub const DEFAULT_PROFIT_CREDIT_PRICE: f64 = 45.0 / 2000.0;
pub const DEFAULT_PROFIT_QUOTA_PER_UNIT: f64 = 500_000.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfitConfig {
    pub newapi_base: Option<String>,
    pub newapi_token: Option<String>,
    pub newapi_user: Option<String>,
    pub credit_price: f64,
    pub quota_per_unit: f64,
}

impl Default for ProfitConfig {
    fn default() -> Self {
        Self {
            newapi_base: None,
            newapi_token: None,
            newapi_user: None,
            credit_price: DEFAULT_PROFIT_CREDIT_PRICE,
            quota_per_unit: DEFAULT_PROFIT_QUOTA_PER_UNIT,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JoinedProfitRow {
    pub trace_id: Option<String>,
    pub key_id: u64,
    pub key_name: String,
    pub group: String,
    pub model: String,
    pub user: String,
    pub quota: i64,
    pub credits: f64,
    pub matched: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfitGroupStat {
    pub name: String,
    pub key_id: Option<u64>,
    pub key_name: Option<String>,
    pub count: u64,
    pub revenue: f64,
    pub credits: f64,
    pub cost: f64,
    pub profit: f64,
    pub missing_cost: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfitReport {
    pub rows: u64,
    pub matched: u64,
    pub unmatched: u64,
    pub missing_cost: u64,
    pub revenue: f64,
    pub matched_revenue: f64,
    pub unmatched_revenue: f64,
    pub credits: f64,
    pub cost: f64,
    pub profit: f64,
    pub margin_pct: f64,
    pub by_key: Vec<ProfitGroupStat>,
    pub by_group: Vec<ProfitGroupStat>,
    pub by_model: Vec<ProfitGroupStat>,
    pub by_user: Vec<ProfitGroupStat>,
}

pub fn aggregate_rows<I>(rows: I, config: ProfitConfig) -> ProfitReport
where
    I: IntoIterator<Item = JoinedProfitRow>,
{
    let credit_price = if config.credit_price.is_finite() && config.credit_price > 0.0 {
        config.credit_price
    } else {
        DEFAULT_PROFIT_CREDIT_PRICE
    };
    let quota_per_unit = if config.quota_per_unit.is_finite() && config.quota_per_unit > 0.0 {
        config.quota_per_unit
    } else {
        DEFAULT_PROFIT_QUOTA_PER_UNIT
    };

    let mut report = ProfitReport::default();
    let mut by_key = BTreeMap::<String, ProfitGroupStat>::new();
    let mut by_group = BTreeMap::<String, ProfitGroupStat>::new();
    let mut by_model = BTreeMap::<String, ProfitGroupStat>::new();
    let mut by_user = BTreeMap::<String, ProfitGroupStat>::new();

    for row in rows {
        report.rows += 1;
        let revenue = if row.quota > 0 {
            row.quota as f64 / quota_per_unit
        } else {
            0.0
        };
        report.revenue += revenue;

        if !row.matched {
            report.unmatched += 1;
            report.unmatched_revenue += revenue;
            continue;
        }

        report.matched += 1;
        report.matched_revenue += revenue;
        let has_credits = row.credits.is_finite() && row.credits > 0.0;
        if has_credits {
            report.credits += row.credits;
            report.cost += row.credits * credit_price;
        } else {
            report.missing_cost += 1;
        }
        let cost = if has_credits {
            row.credits * credit_price
        } else {
            0.0
        };
        let missing_cost = u64::from(!has_credits);

        add_stat(
            &mut by_key,
            row.key_name.clone(),
            Some(row.key_id),
            Some(row.key_name.clone()),
            revenue,
            row.credits,
            cost,
            missing_cost,
        );
        add_stat(
            &mut by_group,
            row.group.clone(),
            None,
            None,
            revenue,
            row.credits,
            cost,
            missing_cost,
        );
        add_stat(
            &mut by_model,
            row.model.clone(),
            None,
            None,
            revenue,
            row.credits,
            cost,
            missing_cost,
        );
        add_stat(
            &mut by_user,
            row.user.clone(),
            None,
            None,
            revenue,
            row.credits,
            cost,
            missing_cost,
        );
    }

    report.profit = report.matched_revenue - report.cost;
    if report.matched_revenue > 0.0 {
        report.margin_pct = report.profit / report.matched_revenue * 100.0;
    }
    report.by_key = sorted_stats(by_key);
    report.by_group = sorted_stats(by_group);
    report.by_model = sorted_stats(by_model);
    report.by_user = sorted_stats(by_user);
    report
}

fn add_stat(
    target: &mut BTreeMap<String, ProfitGroupStat>,
    name: String,
    key_id: Option<u64>,
    key_name: Option<String>,
    revenue: f64,
    credits: f64,
    cost: f64,
    missing_cost: u64,
) {
    let stat = target.entry(name.clone()).or_insert_with(|| ProfitGroupStat {
        name,
        key_id,
        key_name,
        ..ProfitGroupStat::default()
    });
    stat.count += 1;
    stat.revenue += revenue;
    if credits.is_finite() && credits > 0.0 {
        stat.credits += credits;
    }
    stat.cost += cost;
    stat.profit += revenue - cost;
    stat.missing_cost += missing_cost;
}

fn sorted_stats(stats: BTreeMap<String, ProfitGroupStat>) -> Vec<ProfitGroupStat> {
    let mut values: Vec<_> = stats.into_values().collect();
    values.sort_by(|a, b| {
        b.profit
            .total_cmp(&a.profit)
            .then_with(|| a.name.cmp(&b.name))
    });
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(group: &str, key_name: &str, credits: f64, quota: i64) -> JoinedProfitRow {
        JoinedProfitRow {
            trace_id: Some(format!("trace-{key_name}")),
            key_id: 1,
            key_name: key_name.to_string(),
            group: group.to_string(),
            model: "claude-opus-4.8".to_string(),
            user: "alice".to_string(),
            quota,
            credits,
            matched: true,
        }
    }

    #[test]
    fn report_uses_fractional_credits_and_default_price() {
        let report = aggregate_rows(
            vec![joined("g05", "key-a", 0.25, 1)],
            ProfitConfig::default(),
        );
        assert!((report.cost - 0.005625).abs() < 1e-9);
        assert_eq!(report.missing_cost, 0);
    }

    #[test]
    fn missing_credits_are_not_fallback_billed() {
        let report = aggregate_rows(
            vec![joined("g08", "key-b", 0.0, 2)],
            ProfitConfig::default(),
        );
        assert_eq!(report.missing_cost, 1);
        assert_eq!(report.cost, 0.0);
    }

    #[test]
    fn group_rows_remain_separate() {
        let report = aggregate_rows(
            vec![
                joined("ratio-005", "key-a", 1.0, 1),
                joined("ratio-008", "key-b", 1.0, 1),
            ],
            ProfitConfig::default(),
        );
        assert_eq!(report.by_group.len(), 2);
    }

    #[test]
    fn unmatched_revenue_is_visible_but_not_claimed_as_profit() {
        let mut row = joined("g", "key", 1.0, 500_000);
        row.matched = false;
        let report = aggregate_rows(vec![row], ProfitConfig::default());
        assert_eq!(report.unmatched, 1);
        assert_eq!(report.unmatched_revenue, 1.0);
        assert_eq!(report.profit, 0.0);
    }
}
