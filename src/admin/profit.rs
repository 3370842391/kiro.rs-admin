//! NewAPI 收入与 RS 上游 Credits 成本的利润领域模型。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;

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

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct NewapiLogItem {
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub model_name: String,
    #[serde(default)]
    pub token_name: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub quota: i64,
    #[serde(default)]
    pub channel_id: u64,
    #[serde(default)]
    pub upstream_request_id: String,
}

#[derive(Debug, Deserialize)]
struct NewapiLogResponse {
    success: bool,
    #[serde(default)]
    message: String,
    data: NewapiLogPage,
}

#[derive(Debug, Deserialize)]
struct NewapiLogPage {
    #[serde(default)]
    items: Vec<NewapiLogItem>,
    #[serde(default)]
    total: usize,
}

/// Fetches all NewAPI consume logs in a bounded time window.
pub async fn fetch_newapi_logs(
    config: &ProfitConfig,
    start_timestamp: i64,
    end_timestamp: i64,
) -> anyhow::Result<Vec<NewapiLogItem>> {
    let base = config
        .newapi_base
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("NewAPI 地址未配置"))?;
    let token = config
        .newapi_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("NewAPI 访问令牌未配置"))?;
    let user = config
        .newapi_user
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("NewAPI 管理员用户 ID 未配置"))?;
    if start_timestamp >= end_timestamp {
        anyhow::bail!("利润报表时间范围无效");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .no_proxy()
        .build()?;
    let endpoint = format!("{}/api/log/", base.trim_end_matches('/'));
    let mut all = Vec::new();
    for page in 1..=1000usize {
        let response = client
            .get(&endpoint)
            .header(reqwest::header::AUTHORIZATION, token)
            .header("New-Api-User", user)
            .query(&[
                ("p", page.to_string()),
                ("page_size", "100".to_string()),
                ("type", "2".to_string()),
                ("start_timestamp", start_timestamp.to_string()),
                ("end_timestamp", end_timestamp.to_string()),
            ])
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            anyhow::bail!("NewAPI 日志接口 HTTP {}: {}", status, truncate_error(&body));
        }
        let mut parsed: NewapiLogResponse = serde_json::from_str(&body)
            .map_err(|error| anyhow::anyhow!("NewAPI 日志响应无法解析: {error}"))?;
        if !parsed.success {
            anyhow::bail!("NewAPI 日志接口错误: {}", parsed.message);
        }
        let page_len = parsed.data.items.len();
        all.append(&mut parsed.data.items);
        if page_len < 100 || (parsed.data.total > 0 && all.len() >= parsed.data.total) {
            return Ok(all);
        }
    }
    anyhow::bail!("NewAPI 日志分页超过 1000 页，已停止统计")
}

fn truncate_error(value: &str) -> String {
    let value = value.trim();
    let mut output: String = value.chars().take(300).collect();
    if output.chars().count() < value.chars().count() {
        output.push('…');
    }
    output
}

/// RS 客户端 Key 的报表元数据快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfitKeyMetadata {
    pub key_id: u64,
    pub key_name: String,
    pub group: Option<String>,
}

/// 将 NewAPI 收入日志与 RS trace 做精确 ID 关联。
///
/// 不做时间、模型或 token 名称的模糊匹配；这样无法确认成本的日志会进入
/// unmatched，避免把其他请求的 Credits 错算到当前客户。
pub fn join_newapi_logs(
    logs: Vec<NewapiLogItem>,
    traces: Vec<crate::admin::trace_db::ProfitTraceRecord>,
    keys: Vec<ProfitKeyMetadata>,
) -> Vec<JoinedProfitRow> {
    let trace_by_id: HashMap<String, _> = traces
        .into_iter()
        .map(|trace| (trace.trace_id.clone(), trace))
        .collect();
    let key_by_id: HashMap<u64, ProfitKeyMetadata> =
        keys.into_iter().map(|key| (key.key_id, key)).collect();

    logs.into_iter()
        .map(|log| {
            let trace = trace_by_id.get(&log.upstream_request_id);
            let key = trace.and_then(|trace| key_by_id.get(&trace.key_id));
            let key_id = trace.map_or(0, |trace| trace.key_id);
            let key_name = key.map(|key| key.key_name.clone()).unwrap_or_else(|| {
                if key_id == 0 {
                    "system".to_string()
                } else if log.token_name.trim().is_empty() {
                    "unknown-key".to_string()
                } else {
                    log.token_name.clone()
                }
            });
            let group = key
                .and_then(|key| key.group.as_deref())
                .map(str::trim)
                .filter(|group| !group.is_empty())
                .map(str::to_string)
                .or_else(|| (!log.group.trim().is_empty()).then(|| log.group.trim().to_string()))
                .unwrap_or_else(|| "未分组".to_string());
            JoinedProfitRow {
                trace_id: (!log.upstream_request_id.trim().is_empty())
                    .then_some(log.upstream_request_id),
                key_id,
                key_name,
                group,
                model: if !log.model_name.trim().is_empty() {
                    log.model_name
                } else {
                    trace.map(|trace| trace.model.clone()).unwrap_or_default()
                },
                user: log.username,
                quota: log.quota,
                credits: trace.map_or(0.0, |trace| trace.credits),
                matched: trace.is_some(),
            }
        })
        .collect()
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
    pub attributed_credits: f64,
    pub unattributed_credits: f64,
    pub attributed_cost: f64,
    pub unattributed_cost: f64,
    pub attributed_revenue: f64,
    pub unattributed_revenue: f64,
    pub observed_channel_ids: Vec<u64>,
    pub observed_key_ids: Vec<u64>,
    pub ledger_scope_confirmed: bool,
    pub by_key: Vec<ProfitGroupStat>,
    pub by_group: Vec<ProfitGroupStat>,
    pub by_model: Vec<ProfitGroupStat>,
    pub by_user: Vec<ProfitGroupStat>,
}

#[derive(Debug, Clone)]
struct LedgerAttribution {
    trace_id: String,
    key_id: u64,
    model: String,
    credits: f64,
}

#[derive(Debug, Clone, Default)]
struct UsageTraceSummary {
    key_id: u64,
    model: String,
    credits: f64,
}

/// 以 usage JSONL 为成本事实源，两阶段识别本次窗口实际属于 RS 的渠道与 Key。
pub fn aggregate_ledger_report(
    logs: Vec<NewapiLogItem>,
    usage: Vec<crate::admin::usage_stats::UsageRecord>,
    traces: Vec<crate::admin::trace_db::ProfitTraceRecord>,
    keys: Vec<ProfitKeyMetadata>,
    config: ProfitConfig,
) -> ProfitReport {
    let credit_price = effective_credit_price(&config);
    let quota_per_unit = effective_quota_per_unit(&config);
    let key_by_id: HashMap<u64, ProfitKeyMetadata> =
        keys.into_iter().map(|key| (key.key_id, key)).collect();
    let trace_by_id: HashMap<String, crate::admin::trace_db::ProfitTraceRecord> = traces
        .into_iter()
        .map(|trace| (trace.trace_id.clone(), trace))
        .collect();

    let mut usage_by_trace = HashMap::<String, UsageTraceSummary>::new();
    for record in &usage {
        let Some(trace_id) = record
            .trace_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let entry = usage_by_trace
            .entry(trace_id.to_string())
            .or_insert_with(|| UsageTraceSummary {
                key_id: record.key_id,
                model: record.model.clone(),
                credits: 0.0,
            });
        if entry.key_id != record.key_id {
            entry.key_id = 0;
        }
        if entry.model.is_empty() {
            entry.model = record.model.clone();
        }
        if record.credits.is_finite() && record.credits > 0.0 {
            entry.credits += record.credits;
        }
    }

    let attributions: Vec<Option<LedgerAttribution>> = logs
        .iter()
        .map(|log| {
            let trace_id = log.upstream_request_id.trim();
            if trace_id.is_empty() {
                return None;
            }
            let attribution = if let Some(summary) = usage_by_trace.get(trace_id) {
                LedgerAttribution {
                    trace_id: trace_id.to_string(),
                    key_id: summary.key_id,
                    model: summary.model.clone(),
                    credits: summary.credits,
                }
            } else if let Some(trace) = trace_by_id.get(trace_id) {
                LedgerAttribution {
                    trace_id: trace_id.to_string(),
                    key_id: trace.key_id,
                    model: trace.model.clone(),
                    credits: positive_finite(trace.credits),
                }
            } else {
                return None;
            };
            key_by_id.contains_key(&attribution.key_id).then_some(attribution)
        })
        .collect();

    let observed_channel_ids: BTreeSet<u64> = logs
        .iter()
        .zip(&attributions)
        .filter_map(|(log, attribution)| {
            (log.channel_id != 0 && attribution.is_some()).then_some(log.channel_id)
        })
        .collect();
    let observed_key_ids: BTreeSet<u64> = logs
        .iter()
        .zip(&attributions)
        .filter_map(|(log, attribution)| {
            if log.channel_id == 0 {
                None
            } else {
                attribution.as_ref().map(|value| value.key_id)
            }
        })
        .collect();

    let mut report = ProfitReport {
        rows: logs.len() as u64,
        observed_channel_ids: observed_channel_ids.iter().copied().collect(),
        observed_key_ids: observed_key_ids.iter().copied().collect(),
        ledger_scope_confirmed: !observed_channel_ids.is_empty() && !observed_key_ids.is_empty(),
        ..ProfitReport::default()
    };
    if !report.ledger_scope_confirmed {
        report.unmatched = report.rows;
        return report;
    }

    let mut credits_by_key = BTreeMap::<u64, f64>::new();
    for record in &usage {
        if observed_key_ids.contains(&record.key_id) {
            let credits = positive_finite(record.credits);
            if credits > 0.0 {
                *credits_by_key.entry(record.key_id).or_default() += credits;
            }
        }
    }
    report.credits = credits_by_key.values().sum();
    report.cost = report.credits * credit_price;

    let mut remaining_credits = credits_by_key;
    let mut attributed_trace_ids = HashSet::<String>::new();
    let mut by_key = BTreeMap::<String, ProfitGroupStat>::new();
    let mut by_group = BTreeMap::<String, ProfitGroupStat>::new();
    let mut by_model = BTreeMap::<String, ProfitGroupStat>::new();
    let mut by_user = BTreeMap::<String, ProfitGroupStat>::new();
    report.rows = 0;

    for (log, attribution) in logs.into_iter().zip(attributions) {
        if !observed_channel_ids.contains(&log.channel_id) {
            continue;
        }
        report.rows += 1;
        let revenue = quota_revenue(log.quota, quota_per_unit);
        report.revenue += revenue;
        let Some(attribution) = attribution else {
            report.unmatched += 1;
            report.unmatched_revenue += revenue;
            report.unattributed_revenue += revenue;
            continue;
        };

        report.matched += 1;
        report.matched_revenue += revenue;
        report.attributed_revenue += revenue;
        let available = remaining_credits.entry(attribution.key_id).or_default();
        let attributed_credits = if attributed_trace_ids.insert(attribution.trace_id.clone()) {
            let value = attribution.credits.min(*available).max(0.0);
            *available -= value;
            value
        } else {
            0.0
        };
        report.attributed_credits += attributed_credits;
        let cost = attributed_credits * credit_price;
        let missing_cost = u64::from(attributed_credits <= 0.0);
        report.missing_cost += missing_cost;

        let key = key_by_id
            .get(&attribution.key_id)
            .expect("attribution only contains known RS keys");
        let group = key
            .group
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| (!log.group.trim().is_empty()).then(|| log.group.trim().to_string()))
            .unwrap_or_else(|| "未分组".to_string());
        let model = if log.model_name.trim().is_empty() {
            attribution.model
        } else {
            log.model_name
        };
        add_stat(
            &mut by_key,
            key.key_name.clone(),
            Some(key.key_id),
            Some(key.key_name.clone()),
            revenue,
            attributed_credits,
            cost,
            missing_cost,
        );
        add_stat(
            &mut by_group,
            group,
            None,
            None,
            revenue,
            attributed_credits,
            cost,
            missing_cost,
        );
        add_stat(
            &mut by_model,
            model,
            None,
            None,
            revenue,
            attributed_credits,
            cost,
            missing_cost,
        );
        add_stat(
            &mut by_user,
            log.username,
            None,
            None,
            revenue,
            attributed_credits,
            cost,
            missing_cost,
        );
    }

    report.unattributed_credits = (report.credits - report.attributed_credits).max(0.0);
    report.attributed_cost = report.attributed_credits * credit_price;
    report.unattributed_cost = report.unattributed_credits * credit_price;
    report.unattributed_revenue = (report.revenue - report.attributed_revenue).max(0.0);
    report.profit = report.revenue - report.cost;
    if report.revenue > 0.0 {
        report.margin_pct = report.profit / report.revenue * 100.0;
    }
    report.by_key = sorted_stats(by_key);
    report.by_group = sorted_stats(by_group);
    report.by_model = sorted_stats(by_model);
    report.by_user = sorted_stats(by_user);
    report
}

fn effective_credit_price(config: &ProfitConfig) -> f64 {
    if config.credit_price.is_finite() && config.credit_price > 0.0 {
        config.credit_price
    } else {
        DEFAULT_PROFIT_CREDIT_PRICE
    }
}

fn effective_quota_per_unit(config: &ProfitConfig) -> f64 {
    if config.quota_per_unit.is_finite() && config.quota_per_unit > 0.0 {
        config.quota_per_unit
    } else {
        DEFAULT_PROFIT_QUOTA_PER_UNIT
    }
}

fn positive_finite(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

fn quota_revenue(quota: i64, quota_per_unit: f64) -> f64 {
    if quota > 0 {
        quota as f64 / quota_per_unit
    } else {
        0.0
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfitReportRequest {
    pub minutes: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfitReportResponse {
    pub start_timestamp: i64,
    pub end_timestamp: i64,
    pub minutes: u32,
    pub credit_price: f64,
    pub quota_per_unit: f64,
    #[serde(flatten)]
    pub report: ProfitReport,
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
    let stat = target
        .entry(name.clone())
        .or_insert_with(|| ProfitGroupStat {
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
    use crate::admin::trace_db::ProfitTraceRecord;
    use crate::admin::usage_stats::UsageRecord;

    fn newapi_log(trace_id: &str, channel_id: u64, quota: i64) -> NewapiLogItem {
        NewapiLogItem {
            channel_id,
            quota,
            upstream_request_id: trace_id.to_string(),
            token_name: "rs-key".to_string(),
            model_name: "claude-opus-4-8".to_string(),
            username: "alice".to_string(),
            ..NewapiLogItem::default()
        }
    }

    fn usage(
        trace_id: Option<&str>,
        key_id: u64,
        credits: f64,
        status: &str,
    ) -> UsageRecord {
        UsageRecord {
            ts: "2026-07-23T01:00:00Z".to_string(),
            trace_id: trace_id.map(str::to_string),
            key_id,
            credential_id: 11,
            model: "claude-opus-4-8".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            credits,
            duration_ms: 10,
            status: status.to_string(),
        }
    }

    fn key(key_id: u64, name: &str, group: &str) -> ProfitKeyMetadata {
        ProfitKeyMetadata {
            key_id,
            key_name: name.to_string(),
            group: Some(group.to_string()),
        }
    }

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

    #[test]
    fn ledger_report_uses_all_usage_cost_for_observed_rs_keys() {
        let logs = vec![
            newapi_log("matched", 19, 500_000),
            newapi_log("legacy-missing", 19, 500_000),
            newapi_log("gpt", 1, 500_000),
        ];
        let usage = vec![
            usage(Some("matched"), 3, 1.0, "success"),
            usage(None, 3, 9.0, "success"),
            usage(None, 3, 2.0, "error"),
            usage(None, 99, 100.0, "success"),
        ];
        let traces = vec![ProfitTraceRecord {
            trace_id: "legacy-missing".to_string(),
            key_id: 3,
            model: "claude-opus-4-8".to_string(),
            credits: 0.0,
            final_status: "error".to_string(),
        }];
        let report = aggregate_ledger_report(
            logs,
            usage,
            traces,
            vec![key(3, "rs-key", "rs")],
            ProfitConfig::default(),
        );
        assert!(report.ledger_scope_confirmed);
        assert_eq!(report.observed_channel_ids, vec![19]);
        assert_eq!(report.observed_key_ids, vec![3]);
        assert_eq!(report.revenue, 2.0);
        assert_eq!(report.credits, 12.0);
        assert_eq!(report.attributed_credits, 1.0);
        assert_eq!(report.unattributed_credits, 11.0);
        assert!((report.cost - 0.27).abs() < 1e-9);
        assert!((report.profit - 1.73).abs() < 1e-9);
    }

    #[test]
    fn ledger_report_fails_closed_without_observed_channel_or_key() {
        let report = aggregate_ledger_report(
            vec![newapi_log("missing", 19, 500_000)],
            vec![usage(None, 3, 10.0, "success")],
            Vec::new(),
            vec![key(3, "rs-key", "rs")],
            ProfitConfig::default(),
        );
        assert!(!report.ledger_scope_confirmed);
        assert_eq!(report.cost, 0.0);
        assert_eq!(report.profit, 0.0);
    }

    #[tokio::test]
    async fn newapi_client_paginates_and_sends_admin_headers() {
        use axum::extract::Query;
        use axum::http::HeaderMap;
        use axum::routing::get;
        use axum::{Json, Router};
        use serde_json::json;
        use std::collections::HashMap;

        async fn logs(
            Query(query): Query<HashMap<String, String>>,
            headers: HeaderMap,
        ) -> Json<serde_json::Value> {
            assert_eq!(headers.get("authorization").unwrap(), "secret-token");
            assert_eq!(headers.get("new-api-user").unwrap(), "1");
            let page = query
                .get("p")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(1);
            let items = if page == 1 {
                (0..100)
                    .map(|index| {
                        json!({
                            "quota": 10,
                            "username": "alice",
                            "model_name": "claude-opus-4.8",
                            "token_name": "ratio-005",
                            "upstream_request_id": format!("trace-{index}")
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                vec![json!({
                    "quota": 20,
                    "username": "bob",
                    "model_name": "claude-opus-4.8",
                    "token_name": "ratio-008",
                    "upstream_request_id": "trace-last"
                })]
            };
            Json(json!({
                "success": true,
                "message": "",
                "data": { "items": items, "total": 101, "page": page, "page_size": 100 }
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/api/log/", get(logs)))
                .await
                .unwrap();
        });

        let config = ProfitConfig {
            newapi_base: Some(format!("http://{address}")),
            newapi_token: Some("secret-token".to_string()),
            newapi_user: Some("1".to_string()),
            ..ProfitConfig::default()
        };
        let items = fetch_newapi_logs(&config, 1, 2).await.unwrap();
        assert_eq!(items.len(), 101);
        assert_eq!(items.last().unwrap().token_name, "ratio-008");
    }

    #[test]
    fn newapi_join_uses_exact_trace_and_rs_key_metadata() {
        let logs = vec![
            NewapiLogItem {
                username: "alice".to_string(),
                model_name: "claude-opus-4-8".to_string(),
                token_name: "newapi-token".to_string(),
                group: "newapi-fallback".to_string(),
                quota: 40_000,
                upstream_request_id: "trace-exact".to_string(),
                ..NewapiLogItem::default()
            },
            NewapiLogItem {
                username: "bob".to_string(),
                quota: 10_000,
                upstream_request_id: "trace-missing".to_string(),
                ..NewapiLogItem::default()
            },
        ];
        let traces = vec![ProfitTraceRecord {
            trace_id: "trace-exact".to_string(),
            key_id: 7,
            model: "upstream-model".to_string(),
            credits: 0.5,
            final_status: "success".to_string(),
        }];
        let keys = vec![ProfitKeyMetadata {
            key_id: 7,
            key_name: "ratio-005-key".to_string(),
            group: Some("0.05".to_string()),
        }];

        let rows = join_newapi_logs(logs, traces, keys);

        assert_eq!(rows.len(), 2);
        assert!(rows[0].matched);
        assert_eq!(rows[0].trace_id.as_deref(), Some("trace-exact"));
        assert_eq!(rows[0].key_id, 7);
        assert_eq!(rows[0].key_name, "ratio-005-key");
        assert_eq!(rows[0].group, "0.05");
        assert_eq!(rows[0].model, "claude-opus-4-8");
        assert_eq!(rows[0].credits, 0.5);
        assert!(!rows[1].matched);
        assert_eq!(rows[1].credits, 0.0);
    }
}
