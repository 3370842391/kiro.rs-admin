//! 号池风险体检与封号复盘。
//!
//! 这两个视图是 2026-08-31 那次成批封号排查的直接产物。当时判断「号池会不会再被
//! 成批端掉」用到的每一个数，都得 SSH 上服务器跑 SQL 才拿得到：
//!
//! - 429 占比与「有多少分钟一个 429 都没有」
//! - 52 个号只用了 6 个出口，平均一个 IP 挂 8.7 个
//! - 20 分钟内跨多个出口连掉几个号（清扫特征）
//!
//! 面板上一个都没有。这个模块把它们算成结构化结果，让同样的判断在界面上一眼可得。
//!
//! # 为什么阈值写死在代码里
//!
//! 这些数字（429 占比 5%、单出口 3 个号、零 429 分钟占比 50%）都来自线上实测，
//! 不是拍脑袋，也不该让运营去调——调松了就失去预警意义。真要改，改这里并在
//! 注释里写清楚新证据。

use std::collections::BTreeMap;

use serde::Serialize;

use super::proxy_ban_stats::{ProxyBanEvent, SweepSummary, normalize_proxy_key};
use super::rpm_infer::RpmMinuteBucket;

/// 429 占比警戒线。
///
/// 线上被成批判死的那批号，单号 429 占比在 8%~12%；健康的号基本见不到 429。
/// 取 5% 作为「已经在持续超限投递」的判据。
const RATE_LIMIT_WARN_PCT: f64 = 5.0;

/// 单个出口上允许挂多少个号。
///
/// 同出口的号会一起暴露，一个被上游标记，其余的容易连坐——2026-08-31 那次
/// 同出口连坐正是主要传播路径。3 是权衡：完全一号一 IP 成本太高，超过 3 个
/// 就意味着一个 IP 出事会带走一大片。
const MAX_ACCOUNTS_PER_EXIT: usize = 3;

/// 「零 429 分钟」占比的健康线。
///
/// 关键不在 429 总数，而在有没有喘息的时刻：一分钟都不断说明投递速率始终高于
/// 上游天花板，这是「已知超限仍持续投递」的形态。线上出事时是 0/181。
const HEALTHY_QUIET_MINUTE_PCT: f64 = 50.0;

/// 号池体检结论的严重度。
///
/// 只有两档：结论列表本身就代表「有问题」，没问题时不产出条目，所以不需要
/// 一个 `Ok` 变体——那会让「列表里有一条 ok 结论」和「列表为空」两种表达并存。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RiskSeverity {
    /// 有隐患，但还没到会立刻掉号的程度
    Warn,
    /// 正在发生或极可能马上发生成批掉号
    Critical,
}

/// 一条体检结论。`detail` 说清「凭什么这么判」，避免界面上只有一个红点。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskFinding {
    /// 稳定标识，前端据此决定图标与跳转目标
    pub code: &'static str,
    pub severity: RiskSeverity,
    /// 一行结论
    pub title: String,
    /// 判据与建议动作
    pub detail: String,
}

/// 限流形态
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitShape {
    /// 观察窗口（分钟）
    pub window_minutes: i64,
    pub attempts: u64,
    pub success: u64,
    pub rate_limited: u64,
    /// 429 占总跳数的百分比
    pub rate_limited_pct: f64,
    /// 有流量的分钟数
    pub minutes_with_traffic: u64,
    /// 其中一个 429 都没有的分钟数
    pub quiet_minutes: u64,
    /// 上一项占比
    pub quiet_minute_pct: f64,
}

/// 出口集中度
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitConcentration {
    /// 启用中的账号数
    pub accounts: usize,
    /// 这些账号一共用了多少个不同出口
    pub exits: usize,
    /// 平均每个出口挂多少个号
    pub avg_accounts_per_exit: f64,
    /// 挂了超过 [`MAX_ACCOUNTS_PER_EXIT`] 个号的出口，从多到少
    pub crowded: Vec<CrowdedExit>,
    /// 走直连（无代理）的账号数。这些号暴露的是服务器本机 IP
    pub direct_accounts: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CrowdedExit {
    /// 归一化出口键 `host:port`
    pub exit: String,
    pub accounts: usize,
    /// 该出口历史累计烧号数
    pub burned: u64,
}

/// 号池体检结果
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolHealth {
    pub severity_counts: SeverityCounts,
    pub findings: Vec<RiskFinding>,
    pub rate_limit: RateLimitShape,
    pub exits: ExitConcentration,
    /// 近期是否检测到批量清扫特征
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sweep: Option<SweepView>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeverityCounts {
    pub critical: usize,
    pub warn: usize,
}

/// [`SweepSummary`] 的可序列化视图
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepView {
    pub bans: usize,
    pub distinct_exits: usize,
    pub window_minutes: i64,
    pub credentials: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub survival_min_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub survival_max_secs: Option<i64>,
}

impl From<SweepSummary> for SweepView {
    fn from(value: SweepSummary) -> Self {
        Self {
            bans: value.bans,
            distinct_exits: value.distinct_exits,
            window_minutes: value.window_mins,
            credentials: value.credentials,
            survival_min_secs: value.survival_min_secs,
            survival_max_secs: value.survival_max_secs,
        }
    }
}

/// 计算限流形态。
///
/// `buckets` 是按 (凭据, 分钟) 聚合的桶，这里按分钟合并——「哪一分钟全池都没吃
/// 429」才是要看的，单个号安静没有意义。
pub fn rate_limit_shape(buckets: &[RpmMinuteBucket], window_minutes: i64) -> RateLimitShape {
    let mut per_minute: BTreeMap<i64, (u64, u64)> = BTreeMap::new();
    for bucket in buckets {
        let slot = per_minute.entry(bucket.minute_epoch).or_default();
        slot.0 += u64::from(bucket.successes);
        slot.1 += u64::from(bucket.rate_limited);
    }

    let success: u64 = per_minute.values().map(|(s, _)| s).sum();
    let rate_limited: u64 = per_minute.values().map(|(_, r)| r).sum();
    let attempts = success + rate_limited;
    let minutes_with_traffic = per_minute
        .values()
        .filter(|(s, r)| *s > 0 || *r > 0)
        .count() as u64;
    let quiet_minutes = per_minute
        .values()
        .filter(|(s, r)| *s > 0 && *r == 0)
        .count() as u64;

    RateLimitShape {
        window_minutes,
        attempts,
        success,
        rate_limited,
        rate_limited_pct: pct(rate_limited, attempts),
        minutes_with_traffic,
        quiet_minutes,
        quiet_minute_pct: pct(quiet_minutes, minutes_with_traffic),
    }
}

/// 一个启用中的账号，出口视角只需要这两项
pub struct AccountExit<'a> {
    pub proxy_url: Option<&'a str>,
}

/// 计算出口集中度。`burned` 是出口键到历史烧号数的映射。
pub fn exit_concentration(
    accounts: &[AccountExit<'_>],
    burned: &BTreeMap<String, u64>,
) -> ExitConcentration {
    let mut per_exit: BTreeMap<String, usize> = BTreeMap::new();
    let mut direct_accounts = 0usize;
    for account in accounts {
        let key = normalize_proxy_key(account.proxy_url);
        if key == super::proxy_ban_stats::DIRECT_KEY {
            direct_accounts += 1;
        }
        *per_exit.entry(key).or_default() += 1;
    }

    let mut crowded: Vec<CrowdedExit> = per_exit
        .iter()
        .filter(|(_, n)| **n > MAX_ACCOUNTS_PER_EXIT)
        .map(|(exit, n)| CrowdedExit {
            exit: exit.clone(),
            accounts: *n,
            burned: burned.get(exit).copied().unwrap_or(0),
        })
        .collect();
    crowded.sort_by(|a, b| b.accounts.cmp(&a.accounts).then(a.exit.cmp(&b.exit)));

    let exits = per_exit.len();
    ExitConcentration {
        accounts: accounts.len(),
        exits,
        avg_accounts_per_exit: if exits == 0 {
            0.0
        } else {
            round2(accounts.len() as f64 / exits as f64)
        },
        crowded,
        direct_accounts,
    }
}

/// 由各项指标推出体检结论。
///
/// 顺序即展示顺序，最该先处理的排最前。
pub fn assess(
    rate_limit: RateLimitShape,
    exits: ExitConcentration,
    sweep: Option<SweepView>,
) -> PoolHealth {
    let mut findings = Vec::new();

    if let Some(sweep) = &sweep {
        findings.push(RiskFinding {
            code: "sweep",
            severity: RiskSeverity::Critical,
            title: format!(
                "疑似上游批量清扫：{} 分钟内跨 {} 个出口封了 {} 个号",
                sweep.window_minutes, sweep.distinct_exits, sweep.bans
            ),
            detail: format!(
                "存活时长跨度 {} ~ {}。跨度越大越不可能是「每个号各自到寿命」——\
                 上游按墙钟无差别扫号时，刚导入的和跑了一天的会一起没。\
                 这种情况下换代理没用，先停止导入新号，避免继续投料。",
                sweep
                    .survival_min_secs
                    .map(format_secs)
                    .unwrap_or_else(|| "未知".into()),
                sweep
                    .survival_max_secs
                    .map(format_secs)
                    .unwrap_or_else(|| "未知".into()),
            ),
        });
    }

    if exits.direct_accounts > 0 {
        findings.push(RiskFinding {
            code: "direct_exit",
            severity: RiskSeverity::Critical,
            title: format!("{} 个号在走直连，暴露服务器本机 IP", exits.direct_accounts),
            detail: "服务器真实 IP 被上游标记后，从它出去过的号会接连被判死，\
                     而封号会记在各自的代理头上，很难查到根因。给这些号绑上代理。"
                .to_string(),
        });
    }

    // 有流量才谈限流形态：号池闲着的时候 429 占比恒为 0，报「健康」是假信号
    if rate_limit.attempts > 0 {
        if rate_limit.rate_limited_pct >= RATE_LIMIT_WARN_PCT {
            findings.push(RiskFinding {
                code: "rate_limit_high",
                severity: RiskSeverity::Warn,
                title: format!(
                    "429 占比 {:.1}%，投递速率高于上游天花板",
                    rate_limit.rate_limited_pct
                ),
                detail: format!(
                    "近 {} 分钟 {} 跳里有 {} 个 429。持续超限投递是账号被判死的主要信号之一。\
                     按每个号卡片上的「推算 RPM」下调它们的上限——推算值带「见429」\
                     就说明该号已经触顶。",
                    rate_limit.window_minutes, rate_limit.attempts, rate_limit.rate_limited
                ),
            });
        }

        // 分钟数太少时占比没有统计意义，10 分钟是最低样本量
        if rate_limit.minutes_with_traffic >= 10
            && rate_limit.quiet_minute_pct < HEALTHY_QUIET_MINUTE_PCT
        {
            findings.push(RiskFinding {
                code: "no_quiet_minute",
                severity: if rate_limit.quiet_minutes == 0 {
                    RiskSeverity::Critical
                } else {
                    RiskSeverity::Warn
                },
                title: format!(
                    "{} 分钟里只有 {} 分钟没吃到 429",
                    rate_limit.minutes_with_traffic, rate_limit.quiet_minutes
                ),
                detail: "关键不在 429 总数，而在有没有喘息的时刻。一分钟都不断，\
                         说明投递速率始终高于上游给号的天花板，在上游看来就是\
                         「已知超限仍持续投递」。线上成批掉号前正是这个形态。"
                    .to_string(),
            });
        }
    }

    if !exits.crowded.is_empty() {
        let worst = &exits.crowded[0];
        let burned_note = if worst.burned > 0 {
            format!("，其中 {} 已经烧过 {} 个号", worst.exit, worst.burned)
        } else {
            String::new()
        };
        findings.push(RiskFinding {
            code: "exit_crowded",
            severity: if worst.accounts >= MAX_ACCOUNTS_PER_EXIT * 3 {
                RiskSeverity::Critical
            } else {
                RiskSeverity::Warn
            },
            title: format!(
                "{} 个出口上各挂了超过 {} 个号，最多的一个挂了 {} 个{}",
                exits.crowded.len(),
                MAX_ACCOUNTS_PER_EXIT,
                worst.accounts,
                burned_note
            ),
            detail: format!(
                "当前 {} 个号只用了 {} 个出口，平均每个 {:.1} 个。同出口的号会一起暴露，\
                 一个被标记其余容易连坐。补出口，目标是单个出口不超过 {} 个号。",
                exits.accounts, exits.exits, exits.avg_accounts_per_exit, MAX_ACCOUNTS_PER_EXIT
            ),
        });
    }

    let counts = SeverityCounts {
        critical: findings
            .iter()
            .filter(|f| f.severity == RiskSeverity::Critical)
            .count(),
        warn: findings
            .iter()
            .filter(|f| f.severity == RiskSeverity::Warn)
            .count(),
    };

    PoolHealth {
        severity_counts: counts,
        findings,
        rate_limit,
        exits,
        sweep,
    }
}

// ============ 封号复盘 ============

/// 复盘里的一条封号记录
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostmortemEvent {
    pub credential_id: u64,
    /// 归一化出口键
    pub exit: String,
    pub banned_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub survival_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub successes_before_ban: Option<u64>,
    /// 上游封号措辞的归类
    pub kind: &'static str,
}

/// 一波集中封号
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BanWave {
    pub started_at: String,
    pub ended_at: String,
    /// 这波持续了多少秒
    pub span_secs: i64,
    pub bans: usize,
    /// 涉及多少个不同出口
    pub distinct_exits: usize,
    /// 是否符合清扫特征：跨多个出口且存活时长差异大
    pub looks_like_sweep: bool,
    /// 存活时长的最小 / 最大值
    #[serde(skip_serializing_if = "Option::is_none")]
    pub survival_min_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub survival_max_secs: Option<i64>,
    /// 按出口分组的计数，从多到少
    pub by_exit: Vec<ExitBanCount>,
    pub events: Vec<PostmortemEvent>,
    /// 给运营的一句话结论
    pub verdict: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitBanCount {
    pub exit: String,
    pub bans: usize,
}

/// 两次封号相隔超过这个时间就算两波。
///
/// 30 分钟：线上观测到的成批清扫都在 10 分钟内打完，而正常的零星掉号间隔以小时计。
const WAVE_GAP_SECS: i64 = 30 * 60;

/// 判定为清扫需要跨越的出口数。同一个出口连掉几个号是「出口脏」，不是清扫。
const SWEEP_MIN_EXITS: usize = 2;

/// 存活时长差异达到这个倍数，就不像「各自到寿命」。
const SWEEP_SURVIVAL_SPREAD: f64 = 3.0;

/// 把封号明细切成一波一波，并给出归因结论。
///
/// `events` 是 (出口键, 事件)，顺序不限。返回按时间倒序（最近的一波在前）。
pub fn build_waves(mut events: Vec<(String, ProxyBanEvent)>) -> Vec<BanWave> {
    events.sort_by(|a, b| a.1.banned_at.cmp(&b.1.banned_at));

    let mut waves: Vec<Vec<(String, ProxyBanEvent)>> = Vec::new();
    let mut current: Vec<(String, ProxyBanEvent)> = Vec::new();
    let mut last_ts: Option<i64> = None;

    for item in events {
        let ts = parse_epoch(&item.1.banned_at);
        let split = match (last_ts, ts) {
            (Some(prev), Some(now)) => now - prev > WAVE_GAP_SECS,
            // 时间解析不了就不切，宁可并进当前波，也别造出一堆单条波
            _ => false,
        };
        if split && !current.is_empty() {
            waves.push(std::mem::take(&mut current));
        }
        if ts.is_some() {
            last_ts = ts;
        }
        current.push(item);
    }
    if !current.is_empty() {
        waves.push(current);
    }

    let mut out: Vec<BanWave> = waves.into_iter().map(summarize_wave).collect();
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    out
}

fn summarize_wave(items: Vec<(String, ProxyBanEvent)>) -> BanWave {
    let started_at = items
        .first()
        .map(|(_, e)| e.banned_at.clone())
        .unwrap_or_default();
    let ended_at = items
        .last()
        .map(|(_, e)| e.banned_at.clone())
        .unwrap_or_default();
    let span_secs = match (parse_epoch(&started_at), parse_epoch(&ended_at)) {
        (Some(a), Some(b)) => (b - a).max(0),
        _ => 0,
    };

    let mut per_exit: BTreeMap<String, usize> = BTreeMap::new();
    let mut survivals: Vec<i64> = Vec::new();
    for (exit, event) in &items {
        *per_exit.entry(exit.clone()).or_default() += 1;
        if let Some(secs) = event.survival_secs {
            survivals.push(secs);
        }
    }
    survivals.sort_unstable();
    let survival_min = survivals.first().copied();
    let survival_max = survivals.last().copied();

    let distinct_exits = per_exit.len();
    // 清扫的判据是「同时性」而不是「数量」：跨多个出口、且存活时长差异大，
    // 说明这批号不是各自到寿命，而是被同一个墙钟事件一起收掉的。
    let spread_ok = match (survival_min, survival_max) {
        (Some(min), Some(max)) if min > 0 => (max as f64 / min as f64) >= SWEEP_SURVIVAL_SPREAD,
        _ => false,
    };
    let looks_like_sweep = items.len() >= 3 && distinct_exits >= SWEEP_MIN_EXITS && spread_ok;

    let mut by_exit: Vec<ExitBanCount> = per_exit
        .into_iter()
        .map(|(exit, bans)| ExitBanCount { exit, bans })
        .collect();
    by_exit.sort_by(|a, b| b.bans.cmp(&a.bans).then(a.exit.cmp(&b.exit)));

    let verdict = if looks_like_sweep {
        format!(
            "疑似上游批量清扫：{} 分钟内跨 {} 个出口掉了 {} 个号，存活时长从 {} 到 {} 都有。\
             换代理救不了这一类，先停手别继续投料。",
            (span_secs / 60).max(1),
            distinct_exits,
            items.len(),
            survival_min.map(format_secs).unwrap_or_else(|| "?".into()),
            survival_max.map(format_secs).unwrap_or_else(|| "?".into()),
        )
    } else if distinct_exits == 1 {
        let exit = by_exit.first().map(|e| e.exit.as_str()).unwrap_or("?");
        format!(
            "集中在单个出口 {exit} 上掉了 {} 个号，更像这个 IP 自己的问题。\
             把还活着的号从它上面迁走，并在采购时避开。",
            items.len()
        )
    } else {
        format!(
            "{} 个号分散在 {} 个出口上掉落，未见明显的同时性。\
             展开逐条看存活时长与死前请求量：几乎没发请求就死说明出口脏，\
             跑了几千次才死是被打死的，换 IP 解决不了。",
            items.len(),
            distinct_exits
        )
    };

    BanWave {
        started_at,
        ended_at,
        span_secs,
        bans: items.len(),
        distinct_exits,
        looks_like_sweep,
        survival_min_secs: survival_min,
        survival_max_secs: survival_max,
        by_exit,
        events: items
            .into_iter()
            .map(|(exit, event)| PostmortemEvent {
                credential_id: event.credential_id,
                exit,
                banned_at: event.banned_at,
                survival_secs: event.survival_secs,
                successes_before_ban: event.successes_before_ban,
                kind: crate::wholesale::health::ban_message_kind(
                    event.reason.as_deref().unwrap_or(""),
                ),
            })
            .collect(),
        verdict,
    }
}

fn parse_epoch(rfc3339: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|dt| dt.timestamp())
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    round2(part as f64 / total as f64 * 100.0)
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn format_secs(secs: i64) -> String {
    if secs < 60 {
        format!("{secs} 秒")
    } else if secs < 3600 {
        format!("{} 分钟", secs / 60)
    } else {
        format!("{:.1} 小时", secs as f64 / 3600.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(minute: i64, successes: u32, rate_limited: u32) -> RpmMinuteBucket {
        RpmMinuteBucket {
            credential_id: 1,
            minute_epoch: minute,
            successes,
            rate_limited,
        }
    }

    #[test]
    fn quiet_minutes_count_only_minutes_that_had_traffic() {
        // 没有流量的分钟既不算安静也不算吵：号池闲着的时候不该报「健康」
        let shape = rate_limit_shape(
            &[
                bucket(60, 10, 0),
                bucket(120, 10, 2),
                bucket(180, 0, 0), // 无流量
            ],
            3,
        );
        assert_eq!(shape.minutes_with_traffic, 2);
        assert_eq!(shape.quiet_minutes, 1);
        assert_eq!(shape.quiet_minute_pct, 50.0);
        assert_eq!(shape.attempts, 22);
        assert_eq!(shape.rate_limited, 2);
    }

    #[test]
    fn buckets_of_the_same_minute_merge_across_credentials() {
        // 「哪一分钟全池都没吃 429」才是要看的；单个号安静没有意义
        let shape = rate_limit_shape(
            &[
                RpmMinuteBucket {
                    credential_id: 1,
                    minute_epoch: 60,
                    successes: 5,
                    rate_limited: 0,
                },
                RpmMinuteBucket {
                    credential_id: 2,
                    minute_epoch: 60,
                    successes: 5,
                    rate_limited: 3,
                },
            ],
            1,
        );
        assert_eq!(shape.minutes_with_traffic, 1);
        assert_eq!(
            shape.quiet_minutes, 0,
            "该分钟有号吃了 429，就不算安静分钟"
        );
    }

    fn account(url: Option<&str>) -> AccountExit<'_> {
        AccountExit { proxy_url: url }
    }

    #[test]
    fn crowded_exits_are_ranked_and_carry_burn_history() {
        let burned = BTreeMap::from([("1.1.1.1:1080".to_string(), 4u64)]);
        let accounts = vec![
            account(Some("socks5://u:p@1.1.1.1:1080")),
            account(Some("socks5://x:y@1.1.1.1:1080")),
            account(Some("socks5://a:b@1.1.1.1:1080")),
            account(Some("socks5://c:d@1.1.1.1:1080")),
            account(Some("socks5://u:p@2.2.2.2:1080")),
        ];
        let out = exit_concentration(&accounts, &burned);

        assert_eq!(out.accounts, 5);
        assert_eq!(out.exits, 2);
        assert_eq!(out.avg_accounts_per_exit, 2.5);
        assert_eq!(out.crowded.len(), 1, "只有超过 3 个号的出口算拥挤");
        assert_eq!(out.crowded[0].exit, "1.1.1.1:1080");
        assert_eq!(out.crowded[0].accounts, 4);
        assert_eq!(out.crowded[0].burned, 4);
    }

    #[test]
    fn rotating_proxy_credentials_do_not_split_one_exit_into_two() {
        // 机场会轮换 socks5 密码，按完整 URL 去重会把同一个 IP 算成两个出口，
        // 集中度就被低估了
        let accounts = vec![
            account(Some("socks5://old:pass@1.1.1.1:1080")),
            account(Some("socks5://new:secret@1.1.1.1:1080")),
        ];
        let out = exit_concentration(&accounts, &BTreeMap::new());
        assert_eq!(out.exits, 1);
    }

    #[test]
    fn direct_accounts_are_counted_separately() {
        let accounts = vec![account(None), account(Some("direct")), account(Some("1.1.1.1:1"))];
        let out = exit_concentration(&accounts, &BTreeMap::new());
        assert_eq!(out.direct_accounts, 2, "无代理与显式 direct 都算直连");
    }

    #[test]
    fn idle_pool_is_not_reported_as_healthy_or_unhealthy() {
        // 没有流量时 429 占比恒为 0，此时报任何限流结论都是假信号
        let health = assess(
            rate_limit_shape(&[], 60),
            exit_concentration(&[], &BTreeMap::new()),
            None,
        );
        assert!(
            !health.findings.iter().any(|f| f.code == "rate_limit_high"),
            "空窗口不该报限流问题"
        );
        assert!(
            !health.findings.iter().any(|f| f.code == "no_quiet_minute"),
            "空窗口不该报安静分钟问题"
        );
    }

    #[test]
    fn zero_quiet_minutes_is_critical_not_merely_a_warning() {
        // 线上成批掉号前正是这个形态：0/181 分钟安静
        let buckets: Vec<RpmMinuteBucket> = (0..20)
            .map(|i| bucket(i * 60, 100, 10))
            .collect();
        let health = assess(
            rate_limit_shape(&buckets, 20),
            exit_concentration(&[], &BTreeMap::new()),
            None,
        );
        let finding = health
            .findings
            .iter()
            .find(|f| f.code == "no_quiet_minute")
            .expect("应报出没有安静分钟");
        assert_eq!(finding.severity, RiskSeverity::Critical);
        assert_eq!(health.severity_counts.critical, 1);
    }

    #[test]
    fn healthy_pool_reports_nothing() {
        let buckets: Vec<RpmMinuteBucket> = (0..20).map(|i| bucket(i * 60, 100, 0)).collect();
        let accounts = vec![account(Some("1.1.1.1:1")), account(Some("2.2.2.2:1"))];
        let health = assess(
            rate_limit_shape(&buckets, 20),
            exit_concentration(&accounts, &BTreeMap::new()),
            None,
        );
        assert!(health.findings.is_empty(), "健康号池不该报任何问题");
        assert_eq!(health.severity_counts.critical, 0);
    }

    fn event(id: u64, at: &str, survival: i64) -> ProxyBanEvent {
        ProxyBanEvent {
            credential_id: id,
            email: None,
            banned_at: at.to_string(),
            added_at: None,
            survival_secs: Some(survival),
            successes_before_ban: Some(100),
            requests_before_ban: Some(100),
            reason: Some("unusual user activity".to_string()),
            proxy_url: None,
        }
    }

    #[test]
    fn bans_far_apart_in_time_are_separate_waves() {
        let waves = build_waves(vec![
            ("a:1".into(), event(1, "2026-08-31T12:00:00+00:00", 3600)),
            ("a:1".into(), event(2, "2026-08-31T12:02:00+00:00", 3600)),
            // 隔了两小时
            ("a:1".into(), event(3, "2026-08-31T14:00:00+00:00", 3600)),
        ]);
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[0].bans, 1, "最近的一波排最前");
        assert_eq!(waves[1].bans, 2);
    }

    #[test]
    fn sweep_needs_multiple_exits_and_a_wide_survival_spread() {
        // 跨出口 + 存活时长差异大 = 不是各自到寿命
        let waves = build_waves(vec![
            ("a:1".into(), event(1, "2026-08-31T12:00:00+00:00", 600)),
            ("b:1".into(), event(2, "2026-08-31T12:01:00+00:00", 20_000)),
            ("c:1".into(), event(3, "2026-08-31T12:02:00+00:00", 70_000)),
        ]);
        assert_eq!(waves.len(), 1);
        assert!(waves[0].looks_like_sweep);
        assert!(waves[0].verdict.contains("批量清扫"));
    }

    #[test]
    fn same_exit_burning_accounts_is_not_called_a_sweep() {
        // 同一个出口连掉几个号是「这个 IP 脏」，结论与清扫完全不同
        let waves = build_waves(vec![
            ("dirty:1".into(), event(1, "2026-08-31T12:00:00+00:00", 600)),
            ("dirty:1".into(), event(2, "2026-08-31T12:01:00+00:00", 20_000)),
            ("dirty:1".into(), event(3, "2026-08-31T12:02:00+00:00", 70_000)),
        ]);
        assert!(!waves[0].looks_like_sweep);
        assert_eq!(waves[0].distinct_exits, 1);
        assert!(waves[0].verdict.contains("这个 IP 自己的问题"));
    }

    #[test]
    fn similar_survival_times_across_exits_are_not_a_sweep() {
        // 都活了差不多久 = 更像各自到寿命，不该扣清扫的帽子
        let waves = build_waves(vec![
            ("a:1".into(), event(1, "2026-08-31T12:00:00+00:00", 3600)),
            ("b:1".into(), event(2, "2026-08-31T12:01:00+00:00", 3700)),
            ("c:1".into(), event(3, "2026-08-31T12:02:00+00:00", 3800)),
        ]);
        assert!(!waves[0].looks_like_sweep);
    }

    #[test]
    fn wave_groups_by_exit_from_most_to_least() {
        let waves = build_waves(vec![
            ("a:1".into(), event(1, "2026-08-31T12:00:00+00:00", 600)),
            ("b:1".into(), event(2, "2026-08-31T12:01:00+00:00", 20_000)),
            ("b:1".into(), event(3, "2026-08-31T12:02:00+00:00", 70_000)),
        ]);
        assert_eq!(waves[0].by_exit[0].exit, "b:1");
        assert_eq!(waves[0].by_exit[0].bans, 2);
    }
}
