//! 代理封号台账（proxy_ban_stats.json）
//!
//! 回答一个此前无法回答的问题：**这个代理 IP 历史上一共烧掉了几个号**。
//!
//! 为什么必须单独存一份：判死的号只在 `credentials.json` 里留 `diedAt`，而
//! `cleanup_dead_credentials` 会在保留期（默认几小时）后把整条记录删掉。号一删，
//! 「它当时挂在哪个代理上」这条线索就永久消失了——线上实测保留期只有 1 小时，
//! 于是运营看到的永远是「最近一小时封了几个」，历史封号数归零。本台账是
//! append-only 的，与凭据生命周期解耦，号被删掉之后统计依然在。
//!
//! 归一化键：代理身份取 `host:port`，**丢掉 scheme 与账号密码**。机场提供的
//! socks5 认证信息会轮换，同一个出口 IP 换了密码不该被算成两个代理；反过来，
//! 同一个 host 不同端口通常是不同出口，必须分开算，所以端口保留。

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;

/// 每个代理最多保留多少条封号明细（聚合计数不受此限制，只截断明细列表）
const MAX_EVENTS_PER_PROXY: usize = 500;

/// 批量清扫检测：观察窗口（分钟）
const SWEEP_WINDOW_MINS: i64 = 20;
/// 窗口内至少这么多次封号才算异常
const SWEEP_MIN_BANS: usize = 3;
/// 且必须横跨这么多个不同出口——同一个出口连掉几个号是另一回事
const SWEEP_MIN_EXITS: usize = 2;

/// 单次封号事件
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyBanEvent {
    /// 被封凭据 ID（号删了之后这个 ID 不再存在，仅作去重与追溯用）
    pub credential_id: u64,
    /// 被封账号邮箱，便于人工核对
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// 判死时间（RFC3339）
    pub banned_at: String,
    /// 该号加入池子的时间（RFC3339）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    /// 存活秒数 = banned_at - added_at。判断「一挂上去就被封」的核心指标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub survival_secs: Option<i64>,
    /// 该号死前打过的成功请求数。
    ///
    /// 区分「IP 脏」和「我们打太狠」的最直接判据：几乎没发请求就死，说明出口
    /// 在上游那边已经被标记；打了几千次才死，那是自己把号用死的，换 IP 没用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successes_before_ban: Option<u64>,
    /// 该号死前的总请求数（成功 + 各类失败）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_before_ban: Option<u64>,
    /// 上游封号文案片段（截断）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 观测到的完整代理 URL（已抹掉账号密码），保留 scheme 便于识别代理类型
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
}

/// 单个代理的历史封号档案
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyBanRecord {
    /// 历史累计封号数。永不因明细截断或号被删而减少。
    #[serde(default)]
    pub total_bans: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_ban_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ban_at: Option<String>,
    /// 曾经绑定过这个代理的凭据 ID（含已删除的）。作为封号率的分母。
    #[serde(default)]
    pub accounts_seen: Vec<u64>,
    /// 封号明细，最多 [`MAX_EVENTS_PER_PROXY`] 条，新的在后
    #[serde(default)]
    pub events: Vec<ProxyBanEvent>,
}

/// 对外暴露的聚合视图（挂到代理池列表上）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyBanSummary {
    /// 历史累计封号数
    pub total_bans: u64,
    /// 最近 24 小时封号数
    pub bans_24h: u64,
    /// 最近 7 天封号数
    pub bans_7d: u64,
    /// 曾绑定过的账号总数（封号率分母）
    pub accounts_seen: u64,
    /// 封号率 = total_bans / accounts_seen，0~1；分母为 0 时返回 None
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ban_rate: Option<f64>,
    /// 被封账号的存活时长中位数（秒）。越短说明这个 IP 越「脏」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median_survival_secs: Option<i64>,
    /// 被封账号死前成功请求数的中位数。接近 0 = 出口已被上游标记；
    /// 数值很大 = 号是被打死的，换 IP 解决不了。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median_successes_before_ban: Option<u64>,
    /// 被封账号分布在多少个不同的加入日。1 = 全是同一批号，不足以归咎于这个出口。
    pub distinct_batch_days: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_ban_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ban_at: Option<String>,
}

/// 批量清扫的观测摘要
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepSummary {
    pub bans: usize,
    pub distinct_exits: usize,
    pub window_mins: i64,
    pub credentials: Vec<u64>,
    /// 窗口内最短 / 最长存活时长。两者差距越大，越不可能是「每个号各自到寿命」。
    pub survival_min_secs: Option<i64>,
    pub survival_max_secs: Option<i64>,
}

/// 一次封号观测，由调用方填好后交给台账
#[derive(Debug, Clone)]
pub struct BanObservation {
    pub credential_id: u64,
    pub email: Option<String>,
    pub banned_at: String,
    pub added_at: Option<String>,
    pub reason: Option<String>,
    /// 实际使用的代理 URL；None 表示直连
    pub proxy_url: Option<String>,
    pub successes_before_ban: Option<u64>,
    pub requests_before_ban: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LedgerData {
    #[serde(default = "schema_version")]
    version: u32,
    #[serde(default)]
    proxies: BTreeMap<String, ProxyBanRecord>,
}

fn schema_version() -> u32 {
    1
}

/// 直连（无代理）在台账里的固定键
pub const DIRECT_KEY: &str = "(direct)";

/// 把代理 URL 归一化成稳定身份键：`host:port`，丢弃 scheme 与认证信息。
///
/// 认证信息必须丢：机场会轮换 socks5 密码，同一出口 IP 不该分裂成多条统计。
/// 端口必须留：同 host 不同端口通常是不同出口线路。
pub fn normalize_proxy_key(url: Option<&str>) -> String {
    let Some(raw) = url.map(str::trim).filter(|s| !s.is_empty()) else {
        return DIRECT_KEY.to_string();
    };
    if raw.eq_ignore_ascii_case("direct") {
        return DIRECT_KEY.to_string();
    }
    let after_scheme = raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw);
    // user:pass@host:port —— 取最后一个 '@' 之后，密码里含 '@' 时也能切对
    let host_port = after_scheme
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(after_scheme);
    // 去掉可能的路径 / query 尾巴
    let host_port = host_port
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(host_port)
        .trim();
    if host_port.is_empty() {
        DIRECT_KEY.to_string()
    } else {
        host_port.to_ascii_lowercase()
    }
}

/// 抹掉代理 URL 里的账号密码，保留 scheme 与 host:port，可安全写进台账/日志。
pub fn redact_proxy_url(url: &str) -> String {
    let url = url.trim();
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
            format!("{}://{}", scheme, host)
        }
        None => url
            .rsplit_once('@')
            .map(|(_, h)| h.to_string())
            .unwrap_or_else(|| url.to_string()),
    }
}

fn truncate_reason(reason: &str) -> String {
    const MAX: usize = 240;
    let cleaned = reason.replace(['\n', '\r'], " ");
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX).collect();
    out.push('…');
    out
}

pub struct ProxyBanLedger {
    data: Mutex<LedgerData>,
    path: Option<PathBuf>,
    /// 新增封号时被唤醒。让隔离守卫在封号发生的那一刻就动手，而不是等下一个
    /// 轮询周期——一次封号聚集里两个号只隔了 84 秒，轮询间隔就是纯损失。
    ban_signal: Arc<Notify>,
}

impl ProxyBanLedger {
    pub fn new(path: Option<PathBuf>) -> Self {
        let data = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<LedgerData>(&s).ok())
            .unwrap_or_else(|| LedgerData {
                version: schema_version(),
                proxies: BTreeMap::new(),
            });
        Self {
            data: Mutex::new(data),
            path,
            ban_signal: Arc::new(Notify::new()),
        }
    }

    /// 订阅封号事件。等待者在下一次 [`Self::record_ban`] 真正新增记录时被唤醒。
    pub fn ban_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.ban_signal)
    }

    /// 每个代理在最近 `window_hours` 小时内的封号数。
    ///
    /// 与 [`ProxyBanSummary::bans_24h`] 分开是因为隔离窗口可配置；另外这里支持
    /// `since` 下限——出口被手动重新启用后，之前的封号不该立刻把它再送回隔离。
    pub fn bans_in_window(
        &self,
        window_hours: u32,
        since: &dyn Fn(&str) -> Option<chrono::DateTime<chrono::Utc>>,
    ) -> BTreeMap<String, u64> {
        let cutoff = chrono::Utc::now() - chrono::Duration::hours(window_hours.max(1) as i64);
        let data = self.data.lock();
        data.proxies
            .iter()
            .map(|(key, record)| {
                let floor = since(key).map(|at| at.max(cutoff)).unwrap_or(cutoff);
                let count = record
                    .events
                    .iter()
                    .filter_map(|e| chrono::DateTime::parse_from_rfc3339(&e.banned_at).ok())
                    .filter(|ts| ts.with_timezone(&chrono::Utc) >= floor)
                    .count() as u64;
                (key.clone(), count)
            })
            .collect()
    }

    /// 记一次封号。同一 (代理, 凭据) 只计一次，重放与回填都不会重复累加。
    ///
    /// 返回是否真的新增了一条（false = 已记录过）。
    pub fn record_ban(&self, obs: BanObservation) -> bool {
        let key = normalize_proxy_key(obs.proxy_url.as_deref());
        let survival_secs = survival_between(obs.added_at.as_deref(), &obs.banned_at);

        let mut data = self.data.lock();
        let record = data.proxies.entry(key.clone()).or_default();

        if record
            .events
            .iter()
            .any(|e| e.credential_id == obs.credential_id)
        {
            return false;
        }

        if let Err(pos) = record.accounts_seen.binary_search(&obs.credential_id) {
            record.accounts_seen.insert(pos, obs.credential_id);
        }

        record.total_bans += 1;
        if record
            .first_ban_at
            .as_deref()
            .is_none_or(|first| obs.banned_at.as_str() < first)
        {
            record.first_ban_at = Some(obs.banned_at.clone());
        }
        if record
            .last_ban_at
            .as_deref()
            .is_none_or(|last| obs.banned_at.as_str() > last)
        {
            record.last_ban_at = Some(obs.banned_at.clone());
        }

        record.events.push(ProxyBanEvent {
            credential_id: obs.credential_id,
            email: obs.email,
            banned_at: obs.banned_at,
            added_at: obs.added_at,
            survival_secs,
            successes_before_ban: obs.successes_before_ban,
            requests_before_ban: obs.requests_before_ban,
            reason: obs.reason.as_deref().map(truncate_reason),
            proxy_url: obs.proxy_url.as_deref().map(redact_proxy_url),
        });
        if record.events.len() > MAX_EVENTS_PER_PROXY {
            let overflow = record.events.len() - MAX_EVENTS_PER_PROXY;
            record.events.drain(..overflow);
        }

        drop(data);
        self.persist();
        self.ban_signal.notify_waiters();
        true
    }

    /// 检测「上游正在批量清扫」：短窗口内多次封号且横跨多个出口。
    ///
    /// 逐条封号日志看不出这个形态——必须把时间线拉出来横向对比才能发现「刚导入
    /// 6 分钟的号和活了 3 小时的号一起死」。而这恰恰是「上游按墙钟批量扫号」与
    /// 「每个号各自到寿命」的分水岭：前者说明封号与出口、用量都无关，折腾代理池
    /// 是白费力气。2026-08-17 那次排查靠人工重建时间线才看出来，所以让程序直接讲。
    ///
    /// 要求横跨 `SWEEP_MIN_EXITS` 个出口：同一个出口连掉几个号是「出口可能脏」，
    /// 与「无差别清扫」是两个结论，不能混。
    pub fn detect_sweep(&self) -> Option<SweepSummary> {
        let cutoff = chrono::Utc::now() - chrono::Duration::minutes(SWEEP_WINDOW_MINS);
        let mut exits = std::collections::BTreeSet::new();
        let mut credentials = Vec::new();
        let mut survivals = Vec::new();
        {
            let data = self.data.lock();
            for (key, record) in data.proxies.iter() {
                for event in &record.events {
                    let recent = chrono::DateTime::parse_from_rfc3339(&event.banned_at)
                        .ok()
                        .is_some_and(|ts| ts.with_timezone(&chrono::Utc) >= cutoff);
                    if recent {
                        exits.insert(key.clone());
                        credentials.push(event.credential_id);
                        if let Some(secs) = event.survival_secs {
                            survivals.push(secs);
                        }
                    }
                }
            }
        }

        if credentials.len() < SWEEP_MIN_BANS || exits.len() < SWEEP_MIN_EXITS {
            return None;
        }
        survivals.sort_unstable();
        credentials.sort_unstable();
        Some(SweepSummary {
            bans: credentials.len(),
            distinct_exits: exits.len(),
            window_mins: SWEEP_WINDOW_MINS,
            credentials,
            survival_min_secs: survivals.first().copied(),
            survival_max_secs: survivals.last().copied(),
        })
    }

    /// 刚记下一次封号后调用：命中清扫特征就打一条汇总。
    pub fn warn_if_sweep(&self) {
        let Some(sweep) = self.detect_sweep() else {
            return;
        };
        tracing::error!(
            bans = sweep.bans,
            distinct_exits = sweep.distinct_exits,
            window_mins = sweep.window_mins,
            credentials = ?sweep.credentials,
            survival_min_secs = sweep.survival_min_secs.unwrap_or(-1),
            survival_max_secs = sweep.survival_max_secs.unwrap_or(-1),
            "疑似上游批量清扫：多个出口同时掉号。存活时长跨度越大，越说明封号与出口和用量无关"
        );
    }

    /// 登记「这些凭据当前绑在这些代理上」，为封号率提供分母。
    ///
    /// 由代理池列表接口与健康检查调度周期性调用；累积集合，号被删也不回退。
    /// 返回新增的绑定条数，无新增时不落盘。
    pub fn observe_bindings(&self, bindings: impl IntoIterator<Item = (Option<String>, u64)>) {
        let mut added = 0usize;
        {
            let mut data = self.data.lock();
            for (proxy_url, credential_id) in bindings {
                let key = normalize_proxy_key(proxy_url.as_deref());
                let record = data.proxies.entry(key).or_default();
                if let Err(pos) = record.accounts_seen.binary_search(&credential_id) {
                    record.accounts_seen.insert(pos, credential_id);
                    added += 1;
                }
            }
        }
        if added > 0 {
            self.persist();
        }
    }

    /// 取某个代理 URL 的聚合视图
    pub fn summary_for(&self, proxy_url: Option<&str>) -> ProxyBanSummary {
        let key = normalize_proxy_key(proxy_url);
        let data = self.data.lock();
        data.proxies
            .get(&key)
            .map(summarize)
            .unwrap_or_else(ProxyBanSummary::default)
    }

    /// 全量聚合视图，键为归一化后的 `host:port`
    pub fn all_summaries(&self) -> BTreeMap<String, ProxyBanSummary> {
        let data = self.data.lock();
        data.proxies
            .iter()
            .map(|(k, v)| (k.clone(), summarize(v)))
            .collect()
    }

    /// 取某个代理的封号明细（新的在前）
    pub fn events_for(&self, proxy_url: Option<&str>, limit: usize) -> Vec<ProxyBanEvent> {
        let key = normalize_proxy_key(proxy_url);
        let data = self.data.lock();
        data.proxies
            .get(&key)
            .map(|r| r.events.iter().rev().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    /// 全量封号明细（跨代理，新的在前），用于「封号时间线」视图
    pub fn recent_events(&self, limit: usize) -> Vec<(String, ProxyBanEvent)> {
        let data = self.data.lock();
        let mut all: Vec<(String, ProxyBanEvent)> = data
            .proxies
            .iter()
            .flat_map(|(k, r)| r.events.iter().map(move |e| (k.clone(), e.clone())))
            .collect();
        all.sort_by(|a, b| b.1.banned_at.cmp(&a.1.banned_at));
        all.truncate(limit);
        all
    }

    /// 清空某个代理的台账（换了出口 IP 之后重新计数）
    pub fn reset(&self, proxy_url: Option<&str>) -> bool {
        let key = normalize_proxy_key(proxy_url);
        let removed = self.data.lock().proxies.remove(&key).is_some();
        if removed {
            self.persist();
        }
        removed
    }

    /// 从现有凭据回填历史封号记录。
    ///
    /// 用于首次升级：`credentials.json` 里还没被清理掉的死号（有 `diedAt`）
    /// 直接进台账，不至于升级当天统计从零开始。幂等，可重复调用。
    pub fn backfill_from_credentials(
        &self,
        credentials: impl IntoIterator<Item = BanObservation>,
    ) -> usize {
        credentials
            .into_iter()
            .filter(|obs| self.record_ban(obs.clone()))
            .count()
    }

    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let json = {
            let data = self.data.lock();
            match serde_json::to_string_pretty(&*data) {
                Ok(j) => j,
                Err(error) => {
                    tracing::warn!(%error, "代理封号台账序列化失败");
                    return;
                }
            }
        };
        if let Err(error) = atomicwrites::AtomicFile::new(
            path,
            atomicwrites::OverwriteBehavior::AllowOverwrite,
        )
        .write(|f| std::io::Write::write_all(f, json.as_bytes()))
        {
            tracing::warn!(%error, path = %path.display(), "代理封号台账落盘失败");
        }
    }
}

fn survival_between(added_at: Option<&str>, banned_at: &str) -> Option<i64> {
    let added = chrono::DateTime::parse_from_rfc3339(added_at?).ok()?;
    let banned = chrono::DateTime::parse_from_rfc3339(banned_at).ok()?;
    let secs = (banned - added).num_seconds();
    (secs >= 0).then_some(secs)
}

fn summarize(record: &ProxyBanRecord) -> ProxyBanSummary {
    let now = chrono::Utc::now();
    let cutoff_24h = now - chrono::Duration::hours(24);
    let cutoff_7d = now - chrono::Duration::days(7);

    let mut bans_24h = 0u64;
    let mut bans_7d = 0u64;
    let mut survivals: Vec<i64> = Vec::new();
    let mut successes: Vec<u64> = Vec::new();
    let mut batch_days: std::collections::BTreeSet<String> = Default::default();
    for event in &record.events {
        if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&event.banned_at) {
            let ts = ts.with_timezone(&chrono::Utc);
            if ts >= cutoff_24h {
                bans_24h += 1;
            }
            if ts >= cutoff_7d {
                bans_7d += 1;
            }
        }
        if let Some(s) = event.survival_secs {
            survivals.push(s);
        }
        if let Some(s) = event.successes_before_ban {
            successes.push(s);
        }
        // 「批次」用加入日近似：同一天导进来的一般是同一批料
        if let Some(added) = event.added_at.as_deref().filter(|s| s.len() >= 10) {
            batch_days.insert(added[..10].to_string());
        }
    }

    survivals.sort_unstable();
    let median_survival_secs = (!survivals.is_empty()).then(|| survivals[survivals.len() / 2]);
    successes.sort_unstable();
    let median_successes_before_ban =
        (!successes.is_empty()).then(|| successes[successes.len() / 2]);

    let accounts_seen = record.accounts_seen.len() as u64;
    let ban_rate =
        (accounts_seen > 0).then(|| record.total_bans as f64 / accounts_seen as f64);

    ProxyBanSummary {
        total_bans: record.total_bans,
        bans_24h,
        bans_7d,
        accounts_seen,
        ban_rate,
        median_survival_secs,
        median_successes_before_ban,
        distinct_batch_days: batch_days.len() as u64,
        first_ban_at: record.first_ban_at.clone(),
        last_ban_at: record.last_ban_at.clone(),
    }
}

// ============ 风险研判（建议模式，不自动执行） ============
//
// 只输出结论和理由，不碰代理的启用状态。自动隔离是另一个决定，打开之前先让
// 这套判据在真实数据上跑一段时间，确认它不会把整个池子判死。

/// 判定所需的最小分母。1/1 = 100% 封号率，但什么都说明不了。
const MIN_ACCOUNTS_FOR_VERDICT: u64 = 5;
/// 判定所需的最小分子。1~2 次可能只是巧合。
const MIN_BANS_FOR_VERDICT: u64 = 3;
/// 封号率置信下界阈值。用下界而不是原始比率，小样本会被自动压下去。
const BAN_RATE_LB_THRESHOLD: f64 = 0.30;
/// 相对倍数：必须显著高于池内中位数才算「这个 IP 有问题」。
///
/// 这条是整套判据里最关键的一条。如果全池封号率都很高，根因在打法而不在某个
/// 出口；此时禁用最差的那个只会把流量挤到下一个，然后一个个烧光整个池子。
const RELATIVE_MULTIPLE: f64 = 2.0;
/// 被封账号至少要横跨这么多个加入日。同一批号可能本身就是脏料，
/// 某个出口恰好接了一整批的话，它看着脏但其实无辜。
const MIN_BATCH_DAYS: u64 = 2;
/// 隔离后池中必须仍有这么多可分配代理，否则只告警不建议动手。
const MIN_ASSIGNABLE_AFTER: usize = 3;
/// 「秒死」通道：存活中位数低于此值（秒）视为出口已被上游标记。
const FAST_BURN_SURVIVAL_SECS: i64 = 900;
const FAST_BURN_MIN_BANS_24H: u64 = 3;
/// 死前成功请求数低于此值，说明号几乎没干活就没了，锅在出口而不在用量。
const LOW_SUCCESS_BEFORE_BAN: u64 = 20;

/// 降权陡度。`weight = exp(-K * 超出中位数的封号率)`：
/// 超出 30 个百分点时权重降到约 5%，基本退出正常轮换。
const PENALTY_K: f64 = 10.0;
/// 权重下限。不降到 0 是刻意的——完全断流会让它的统计永远冻结，
/// 出口换了干净 IP 也再无翻身机会，只能靠人工清零台账。
const MIN_SELECTION_WEIGHT: f64 = 0.02;
/// 降权力度的样本平滑项：`confidence = n / (n + 平滑项)`。
///
/// 光靠 Wilson 下界不够。1/1 的下界是 0.21，直接按它降权会把一个刚上线、
/// 只是恰好第一个号出事的出口打进惩罚档——正是要避免的误杀。乘上这个
/// 随样本量爬升的置信系数后，1 个号只贡献 1/7 的力度，12 个号贡献 2/3。
const PENALTY_SAMPLE_SMOOTHING: f64 = 6.0;
/// 权重高于此值算正常档
const TIER_NORMAL_WEIGHT: f64 = 0.6;
/// 权重高于此值算降权档，低于则为惩罚档（仅在前两档用尽时兜底）
const TIER_DEGRADED_WEIGHT: f64 = 0.2;

/// 代理在候选排序中的档位。档位是排序主键，同档内沿用原有均衡策略。
///
/// 关键性质：权重按**相对池内中位数**计算，所以当全池封号率一样高时，
/// 所有出口都落在 Normal 档，排序完全退回原策略——不会出现「全都被降权、
/// 结果谁也选不上」，也不会在根因是请求打法时误伤某一个出口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SelectionTier {
    /// 正常参与轮换
    Normal,
    /// 降权：只在正常档用尽后才轮到
    Degraded,
    /// 惩罚：基本不会被选中，仅作最后兜底
    Penalized,
}

impl SelectionTier {
    pub fn rank(self) -> u8 {
        match self {
            SelectionTier::Normal => 0,
            SelectionTier::Degraded => 1,
            SelectionTier::Penalized => 2,
        }
    }

    fn from_weight(weight: f64) -> Self {
        if weight >= TIER_NORMAL_WEIGHT {
            SelectionTier::Normal
        } else if weight >= TIER_DEGRADED_WEIGHT {
            SelectionTier::Degraded
        } else {
            SelectionTier::Penalized
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RiskLevel {
    /// 无封号记录
    Ok,
    /// 有封号但证据不足以归咎于这个出口
    Watch,
    /// 封号率确实偏高，但有检验没通过（样本太小 / 单一批次 / 全池都高）
    Suspect,
    /// 各项检验都通过，建议隔离
    QuarantineRecommended,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyRiskAssessment {
    pub level: RiskLevel,
    /// 封号率的 Wilson 95% 置信下界
    pub ban_rate_lower_bound: f64,
    /// 参照用的池内封号率中位数
    pub pool_median_ban_rate: f64,
    /// 全池合计封号率（总封号 / 总绑定过的号）。降权与「是否显著」都以它为基线。
    ///
    /// 暴露给前端是为了让运营能自己判断：出口的封号数只要没超过这个基线，就只是
    /// 「服役期间赶上过几次全池清扫」，不是它自己的问题。少了这个参照，界面上
    /// 那句「烧号 4 个 · 44%」会一直把人往错误结论上带。
    pub pooled_ban_rate: f64,
    /// 该出口是否**统计上**显著高于全池基线（置信下界高过基线才算）
    pub above_pool_baseline: bool,
    /// 是否建议隔离。仅作展示，不会自动改代理的启用状态。
    pub recommend_quarantine: bool,
    /// 候选排序权重（0~1）。相对池内中位数计算，全池一样烂时所有人都是 1。
    pub selection_weight: f64,
    /// 由权重换算出的档位，排序时作为主键
    pub selection_tier: SelectionTier,
    /// 支持「这个出口有问题」的证据
    pub reasons: Vec<String>,
    /// 阻止下结论的原因。原始封号率很高但没被建议隔离时，这里说明为什么。
    pub blockers: Vec<String>,
}

impl ProxyRiskAssessment {
    /// 无封号记录时的空结论
    pub fn none() -> Self {
        Self {
            level: RiskLevel::Ok,
            ban_rate_lower_bound: 0.0,
            pool_median_ban_rate: 0.0,
            pooled_ban_rate: 0.0,
            above_pool_baseline: false,
            recommend_quarantine: false,
            selection_weight: 1.0,
            selection_tier: SelectionTier::Normal,
            reasons: Vec::new(),
            blockers: Vec::new(),
        }
    }
}

impl Default for ProxyRiskAssessment {
    fn default() -> Self {
        Self::none()
    }
}

/// 二项比例的 Wilson 置信下界（95%）。
///
/// 用它代替原始比率 x/n：小样本时下界会被自动压低，不必为 1/1、2/3 这类
/// 情况写一堆特判。
pub fn wilson_lower_bound(successes: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let n = total as f64;
    let phat = successes as f64 / n;
    let z = 1.96_f64;
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let centre = phat + z2 / (2.0 * n);
    let margin = z * (phat * (1.0 - phat) / n + z2 / (4.0 * n * n)).sqrt();
    ((centre - margin) / denom).max(0.0)
}

fn median_f64(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    // 偶数个取两个中位数的均值。取上中位数会让「只有两个出口」时基准恰好等于
    // 那个坏出口，excess 恒为 0，再脏也永远不会被降权。
    if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

/// 对整池做风险研判。需要全池视角，因为「显著高于中位数」是相对判断。
///
/// `assignable_count` 是当前可分配代理数，用于容量下限检查。
pub fn assess_pool_risk(
    summaries: &BTreeMap<String, ProxyBanSummary>,
    assignable_count: usize,
) -> BTreeMap<String, ProxyRiskAssessment> {
    let pool_median_ban_rate = median_f64(
        summaries
            .values()
            .filter(|s| s.accounts_seen > 0)
            .map(|s| s.ban_rate.unwrap_or(0.0))
            .collect(),
    );
    let pool_median_survival = median_f64(
        summaries
            .values()
            .filter_map(|s| s.median_survival_secs)
            .map(|v| v as f64)
            .collect(),
    );
    let capacity_ok = assignable_count.saturating_sub(1) >= MIN_ASSIGNABLE_AFTER;

    // 降权基准取**全池合计封号率**（总封号 / 总绑定过的号），而不是各出口下界的中位数。
    //
    // 中位数在这里是错的参照。封号是全池性事件——上游按墙钟无差别清扫当时在跑的号，
    // 随机落在各个出口上。曝光量小的出口很容易一次都没摊上，于是一堆 0 把中位数拉到
    // 接近 0，任何摊到封号的出口都成了「离群点」。2026-08-17 线上就是这样把两个统计
    // 上完全正常的出口降到 0.42 / 0.47 权重：全池基线 27%，它们的封号率置信下界只有
    // 19% 和 22%，本来连基线都没够上。
    //
    // 合计率回答的才是正确的问题：这个出口是否**比全池平均更容易烧号**。
    let pooled_ban_rate = {
        let bans: u64 = summaries.values().map(|s| s.total_bans).sum();
        let seen: u64 = summaries
            .values()
            .map(|s| s.accounts_seen.max(s.total_bans))
            .sum();
        if seen == 0 {
            0.0
        } else {
            bans as f64 / seen as f64
        }
    };

    summaries
        .iter()
        .map(|(key, s)| {
            let lb = wilson_lower_bound(s.total_bans, s.accounts_seen.max(s.total_bans));
            let rate = s.ban_rate.unwrap_or(0.0);
            let mut reasons = Vec::new();
            let mut blockers = Vec::new();

            // 超出全池合计封号率多少才降权。全池一样烂时 excess 恒为 0，
            // 所有出口权重都是 1，排序退回原策略。
            let excess = (lb - pooled_ban_rate).max(0.0);
            // 再按样本量打折：号越少，越不敢下手
            let observed = s.accounts_seen.max(s.total_bans) as f64;
            let confidence = observed / (observed + PENALTY_SAMPLE_SMOOTHING);
            let selection_weight = (-PENALTY_K * excess * confidence)
                .exp()
                .clamp(MIN_SELECTION_WEIGHT, 1.0);
            let selection_tier = SelectionTier::from_weight(selection_weight);
            let above_pool_baseline = excess > 0.0;

            if s.total_bans == 0 {
                return (
                    key.clone(),
                    ProxyRiskAssessment {
                        level: RiskLevel::Ok,
                        ban_rate_lower_bound: 0.0,
                        pool_median_ban_rate,
                        pooled_ban_rate,
                        above_pool_baseline: false,
                        recommend_quarantine: false,
                        selection_weight: 1.0,
                        selection_tier: SelectionTier::Normal,
                        reasons,
                        blockers,
                    },
                );
            }

            // 这条要放在最前面：它决定运营看到「烧号 4 个 · 44%」时该不该当回事。
            if above_pool_baseline {
                reasons.push(format!(
                    "封号率置信下界 {:.0}% 高于全池基线 {:.0}%，确实比平均更容易烧号",
                    lb * 100.0,
                    pooled_ban_rate * 100.0
                ));
            } else {
                blockers.push(format!(
                    "未超全池基线：本出口封号率置信下界 {:.0}%，全池合计 {:.0}%（{} 个号封了 {} 个）。\
                     这些封号只说明它服役期间赶上过全池清扫，不是它自己的问题",
                    lb * 100.0,
                    pooled_ban_rate * 100.0,
                    summaries
                        .values()
                        .map(|x| x.accounts_seen.max(x.total_bans))
                        .sum::<u64>(),
                    summaries.values().map(|x| x.total_bans).sum::<u64>(),
                ));
            }

            if selection_tier != SelectionTier::Normal {
                reasons.push(format!(
                    "已降权至 {:.0}%（{}），正常档出口用尽前不会轮到它",
                    selection_weight * 100.0,
                    match selection_tier {
                        SelectionTier::Degraded => "降权档",
                        _ => "惩罚档",
                    }
                ));
            }

            let sample_ok =
                s.accounts_seen >= MIN_ACCOUNTS_FOR_VERDICT && s.total_bans >= MIN_BANS_FOR_VERDICT;
            if sample_ok {
                reasons.push(format!(
                    "样本足够：{} 个号里封了 {} 个",
                    s.accounts_seen, s.total_bans
                ));
            } else {
                blockers.push(format!(
                    "样本不足：需要至少 {} 个号且 {} 次封号，当前 {}/{}",
                    MIN_ACCOUNTS_FOR_VERDICT, MIN_BANS_FOR_VERDICT, s.total_bans, s.accounts_seen
                ));
            }

            let absolute_ok = lb >= BAN_RATE_LB_THRESHOLD;
            if absolute_ok {
                reasons.push(format!(
                    "封号率置信下界 {:.0}%，高于 {:.0}% 阈值",
                    lb * 100.0,
                    BAN_RATE_LB_THRESHOLD * 100.0
                ));
            } else {
                blockers.push(format!(
                    "原始封号率 {:.0}% 但置信下界只有 {:.0}%，证据不足",
                    rate * 100.0,
                    lb * 100.0
                ));
            }

            let relative_ok = pool_median_ban_rate <= 0.0
                || rate >= pool_median_ban_rate * RELATIVE_MULTIPLE;
            if relative_ok {
                if pool_median_ban_rate > 0.0 {
                    reasons.push(format!(
                        "封号率 {:.0}% 达到池内中位数 {:.0}% 的 {:.1} 倍",
                        rate * 100.0,
                        pool_median_ban_rate * 100.0,
                        rate / pool_median_ban_rate
                    ));
                }
            } else {
                blockers.push(format!(
                    "全池封号率普遍偏高（中位数 {:.0}%），根因更可能在请求打法而不是这个出口",
                    pool_median_ban_rate * 100.0
                ));
            }

            let batch_ok = s.distinct_batch_days >= MIN_BATCH_DAYS;
            if batch_ok {
                reasons.push(format!("被封的号横跨 {} 个加入批次", s.distinct_batch_days));
            } else {
                blockers.push(
                    "被封的号全部来自同一批，可能是这批料本身脏，不能归咎于出口".to_string(),
                );
            }

            if !capacity_ok {
                blockers.push(format!(
                    "可分配代理仅剩 {} 个，隔离后不足 {} 个，只告警不建议动手",
                    assignable_count, MIN_ASSIGNABLE_AFTER
                ));
            }

            // 秒死通道：号几乎没干活就没了，且明显快于全池，基本可断定出口已被标记
            let low_usage = s
                .median_successes_before_ban
                .is_some_and(|v| v <= LOW_SUCCESS_BEFORE_BAN);
            let fast_burn = s
                .median_survival_secs
                .is_some_and(|v| v < FAST_BURN_SURVIVAL_SECS)
                && s.bans_24h >= FAST_BURN_MIN_BANS_24H
                && low_usage
                && (pool_median_survival <= 0.0
                    || (s.median_survival_secs.unwrap_or(0) as f64) * 4.0 < pool_median_survival);
            if fast_burn {
                reasons.push(format!(
                    "秒死特征：存活中位数仅 {} 秒且死前几乎没成功请求，出口已被上游标记",
                    s.median_survival_secs.unwrap_or(0)
                ));
            }

            let recommend_quarantine =
                capacity_ok && (fast_burn || (sample_ok && absolute_ok && relative_ok && batch_ok));

            // 没超全池基线的一律算 Ok：它的封号只是清扫噪声，标成「存疑」会让运营
            // 把注意力花在无辜的出口上，真正的问题（账号来源）反而被掩盖。
            let level = if recommend_quarantine {
                RiskLevel::QuarantineRecommended
            } else if !above_pool_baseline {
                RiskLevel::Ok
            } else if absolute_ok || rate >= BAN_RATE_LB_THRESHOLD {
                RiskLevel::Suspect
            } else {
                RiskLevel::Watch
            };

            (
                key.clone(),
                ProxyRiskAssessment {
                    level,
                    ban_rate_lower_bound: lb,
                    pool_median_ban_rate,
                    pooled_ban_rate,
                    above_pool_baseline,
                    recommend_quarantine,
                    selection_weight,
                    selection_tier,
                    reasons,
                    blockers,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(id: u64, proxy: Option<&str>, banned_at: &str, added_at: Option<&str>) -> BanObservation {
        BanObservation {
            credential_id: id,
            email: Some(format!("user{}@example.com", id)),
            banned_at: banned_at.to_string(),
            added_at: added_at.map(str::to_string),
            reason: Some("TEMPORARILY_SUSPENDED".to_string()),
            proxy_url: proxy.map(str::to_string),
            successes_before_ban: None,
            requests_before_ban: None,
        }
    }

    /// 造一份汇总，跳过台账直接喂给风险研判
    fn summary(
        total_bans: u64,
        accounts_seen: u64,
        batch_days: u64,
        median_survival: Option<i64>,
        median_successes: Option<u64>,
    ) -> ProxyBanSummary {
        ProxyBanSummary {
            total_bans,
            bans_24h: total_bans,
            bans_7d: total_bans,
            accounts_seen,
            ban_rate: (accounts_seen > 0)
                .then(|| total_bans as f64 / accounts_seen as f64),
            median_survival_secs: median_survival,
            median_successes_before_ban: median_successes,
            distinct_batch_days: batch_days,
            first_ban_at: None,
            last_ban_at: None,
        }
    }

    fn pool(entries: &[(&str, ProxyBanSummary)]) -> BTreeMap<String, ProxyBanSummary> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn normalize_drops_scheme_and_credentials_keeps_port() {
        assert_eq!(
            normalize_proxy_key(Some("socks5://user:pass@1.2.3.4:7139")),
            "1.2.3.4:7139"
        );
        // 换了密码仍是同一个代理
        assert_eq!(
            normalize_proxy_key(Some("socks5://other:secret@1.2.3.4:7139")),
            normalize_proxy_key(Some("http://1.2.3.4:7139"))
        );
        // 不同端口算不同出口
        assert_ne!(
            normalize_proxy_key(Some("socks5://1.2.3.4:7139")),
            normalize_proxy_key(Some("socks5://1.2.3.4:7140"))
        );
        // 密码里带 '@' 也能切对
        assert_eq!(
            normalize_proxy_key(Some("socks5://user:p@ss@1.2.3.4:7139")),
            "1.2.3.4:7139"
        );
        assert_eq!(normalize_proxy_key(None), DIRECT_KEY);
        assert_eq!(normalize_proxy_key(Some("direct")), DIRECT_KEY);
        assert_eq!(normalize_proxy_key(Some("   ")), DIRECT_KEY);
    }

    #[test]
    fn redact_strips_auth_but_keeps_scheme() {
        assert_eq!(
            redact_proxy_url("socks5://user:pass@1.2.3.4:7139"),
            "socks5://1.2.3.4:7139"
        );
        assert_eq!(
            redact_proxy_url("http://1.2.3.4:8080"),
            "http://1.2.3.4:8080"
        );
    }

    #[test]
    fn same_credential_counted_once_per_proxy() {
        let ledger = ProxyBanLedger::new(None);
        let proxy = Some("socks5://a:b@1.2.3.4:7139");
        assert!(ledger.record_ban(obs(1, proxy, "2026-08-15T12:00:00+00:00", None)));
        // 同一凭据重放（并发判死 / 回填）不重复累加
        assert!(!ledger.record_ban(obs(1, proxy, "2026-08-15T12:00:00+00:00", None)));
        // 换了代理认证信息仍视为同一代理
        assert!(!ledger.record_ban(obs(1, Some("socks5://x:y@1.2.3.4:7139"), "2026-08-15T13:00:00+00:00", None)));

        let summary = ledger.summary_for(proxy);
        assert_eq!(summary.total_bans, 1);
        assert_eq!(summary.accounts_seen, 1);
    }

    #[test]
    fn ban_survives_account_deletion() {
        // 台账与凭据生命周期解耦：号删了统计还在，这是这个模块存在的理由
        let ledger = ProxyBanLedger::new(None);
        let proxy = Some("socks5://1.2.3.4:7139");
        for id in 1..=3 {
            ledger.record_ban(obs(id, proxy, "2026-08-15T12:00:00+00:00", None));
        }
        // 模拟凭据被 cleanup 删光：不做任何回调，台账不受影响
        let summary = ledger.summary_for(proxy);
        assert_eq!(summary.total_bans, 3);
    }

    #[test]
    fn survival_and_median_computed_from_added_at() {
        let ledger = ProxyBanLedger::new(None);
        let proxy = Some("socks5://1.2.3.4:7139");
        ledger.record_ban(obs(
            1,
            proxy,
            "2026-08-15T12:00:00+00:00",
            Some("2026-08-15T11:00:00+00:00"),
        ));
        ledger.record_ban(obs(
            2,
            proxy,
            "2026-08-15T12:10:00+00:00",
            Some("2026-08-15T12:00:00+00:00"),
        ));
        ledger.record_ban(obs(
            3,
            proxy,
            "2026-08-15T12:20:00+00:00",
            Some("2026-08-15T12:00:00+00:00"),
        ));
        let summary = ledger.summary_for(proxy);
        assert_eq!(summary.total_bans, 3);
        // 存活时长 3600 / 600 / 1200 → 中位数 1200
        assert_eq!(summary.median_survival_secs, Some(1200));
    }

    #[test]
    fn bindings_form_ban_rate_denominator() {
        let ledger = ProxyBanLedger::new(None);
        let proxy = "socks5://1.2.3.4:7139";
        ledger.observe_bindings((1..=10).map(|id| (Some(proxy.to_string()), id)));
        ledger.record_ban(obs(1, Some(proxy), "2026-08-15T12:00:00+00:00", None));
        ledger.record_ban(obs(2, Some(proxy), "2026-08-15T12:00:00+00:00", None));

        let summary = ledger.summary_for(Some(proxy));
        assert_eq!(summary.accounts_seen, 10);
        assert_eq!(summary.total_bans, 2);
        assert_eq!(summary.ban_rate, Some(0.2));
    }

    #[test]
    fn ban_of_unbound_account_still_extends_denominator() {
        // 号加进来 6 分钟就被封，从没被 observe_bindings 扫到，分母也不能为 0
        let ledger = ProxyBanLedger::new(None);
        let proxy = Some("socks5://1.2.3.4:7139");
        ledger.record_ban(obs(99, proxy, "2026-08-15T12:00:00+00:00", None));
        let summary = ledger.summary_for(proxy);
        assert_eq!(summary.accounts_seen, 1);
        assert_eq!(summary.ban_rate, Some(1.0));
    }

    /// 2026-08-17 线上快照回归：清扫式封号不得降权任何出口。
    ///
    /// 当天全池 34/137 ≈ 25%，是上游按墙钟无差别清扫造成的（8 个号在 32 分钟内死在
    /// 8 个不同出口，存活时长从 36 分钟到 191 分钟）。但降权基准当时取的是「各出口
    /// 置信下界的中位数」，而池子里一堆曝光量小、一次都没摊上封号的出口把中位数压到
    /// 接近 0，于是 4/9 和 4/8 这两个统计上完全正常的出口被降到 0.47 / 0.42 权重，
    /// 界面上还标成「存疑」——运营因此完全无法分辨哪些是误判。
    ///
    /// 改成以全池合计率为基准后，4/9 的置信下界只有 19%，够不上 25% 的基线，不降权。
    #[test]
    fn sweep_noise_does_not_demote_any_exit() {
        let observed: &[(u64, u64)] = &[
            (4, 9),
            (4, 8),
            (3, 12),
            (2, 9),
            (2, 7),
            (2, 4),
            (2, 6),
            (2, 9),
            (2, 5),
            (2, 10),
            (1, 4),
            (1, 4),
            (1, 7),
            (1, 11),
            (1, 1),
            (1, 1),
            (1, 1),
            (1, 3),
            (1, 5),
            (0, 4),
            (0, 3),
            (0, 3),
            (0, 4),
            (0, 4),
            (0, 3),
        ];
        let entries: Vec<(String, ProxyBanSummary)> = observed
            .iter()
            .enumerate()
            .map(|(i, &(bans, seen))| {
                (format!("10.0.0.{}:1080", i + 1), summary(bans, seen, 1, None, None))
            })
            .collect();
        let pool: BTreeMap<String, ProxyBanSummary> = entries.into_iter().collect();

        let assessed = assess_pool_risk(&pool, 25);
        let pooled = assessed.values().next().unwrap().pooled_ban_rate;
        assert!(
            (0.20..0.30).contains(&pooled),
            "全池合计率应落在 25% 附近，实际 {:.3}",
            pooled
        );

        for (key, risk) in &assessed {
            assert_eq!(
                risk.selection_tier,
                SelectionTier::Normal,
                "{key} 不该被降权（权重 {:.2}）：它的封号只是全池清扫噪声",
                risk.selection_weight
            );
            assert_eq!(risk.selection_weight, 1.0, "{key} 权重应为满值");
            assert!(!risk.above_pool_baseline, "{key} 不该被判为高于基线");
            assert_eq!(risk.level, RiskLevel::Ok, "{key} 不该被标成存疑");
            assert!(!risk.recommend_quarantine, "{key} 不该被建议隔离");
        }
    }

    #[test]
    fn sweep_needs_multiple_exits_not_just_multiple_bans() {
        let ledger = ProxyBanLedger::new(None);
        let now = chrono::Utc::now();
        let recent = |mins: i64| (now - chrono::Duration::minutes(mins)).to_rfc3339();

        // 同一个出口连掉 3 个号：那是「出口可能脏」，不是无差别清扫
        for id in 1..=3 {
            ledger.record_ban(obs(id, Some("socks5://1.1.1.1:1080"), &recent(5), None));
        }
        assert_eq!(ledger.detect_sweep(), None);

        // 换个出口再掉一个 -> 跨出口，命中清扫特征
        ledger.record_ban(obs(4, Some("socks5://2.2.2.2:1080"), &recent(3), None));
        let sweep = ledger.detect_sweep().expect("应识别为批量清扫");
        assert_eq!(sweep.bans, 4);
        assert_eq!(sweep.distinct_exits, 2);
        assert_eq!(sweep.credentials, vec![1, 2, 3, 4]);
    }

    #[test]
    fn sweep_ignores_bans_outside_the_window() {
        let ledger = ProxyBanLedger::new(None);
        let now = chrono::Utc::now();
        // 三个号分散在几小时里，各自到寿命，不该报清扫
        ledger.record_ban(obs(1, Some("socks5://1.1.1.1:1080"),
            &(now - chrono::Duration::hours(3)).to_rfc3339(), None));
        ledger.record_ban(obs(2, Some("socks5://2.2.2.2:1080"),
            &(now - chrono::Duration::hours(2)).to_rfc3339(), None));
        ledger.record_ban(obs(3, Some("socks5://3.3.3.3:1080"),
            &(now - chrono::Duration::minutes(1)).to_rfc3339(), None));
        assert_eq!(ledger.detect_sweep(), None);
    }

    #[test]
    fn sweep_reports_survival_spread() {
        // 存活跨度是关键证据：6 分钟的新号和 3 小时的老号一起死，
        // 说明不是每个号各自到寿命，而是被同时端掉
        let ledger = ProxyBanLedger::new(None);
        let now = chrono::Utc::now();
        let banned = (now - chrono::Duration::minutes(2)).to_rfc3339();
        for (id, age_mins, exit) in [
            (1u64, 6i64, "socks5://1.1.1.1:1080"),
            (2, 191, "socks5://2.2.2.2:1080"),
            (3, 66, "socks5://3.3.3.3:1080"),
        ] {
            let added = (now - chrono::Duration::minutes(2 + age_mins)).to_rfc3339();
            ledger.record_ban(obs(id, Some(exit), &banned, Some(&added)));
        }
        let sweep = ledger.detect_sweep().expect("应识别为批量清扫");
        assert_eq!(sweep.distinct_exits, 3);
        assert_eq!(sweep.survival_min_secs, Some(6 * 60));
        assert_eq!(sweep.survival_max_secs, Some(191 * 60));
    }

    #[test]
    fn window_counts_only_recent_bans() {
        let ledger = ProxyBanLedger::new(None);
        let proxy = Some("socks5://1.2.3.4:7139");
        let now = chrono::Utc::now();
        // 一次在窗口内，一次远在窗口之外
        ledger.record_ban(obs(
            1,
            proxy,
            &(now - chrono::Duration::hours(2)).to_rfc3339(),
            None,
        ));
        ledger.record_ban(obs(
            2,
            proxy,
            &(now - chrono::Duration::hours(80)).to_rfc3339(),
            None,
        ));

        let no_floor = |_: &str| None;
        let bans = ledger.bans_in_window(24, &no_floor);
        assert_eq!(bans.get("1.2.3.4:7139").copied(), Some(1));
        // 拉长窗口两次都算上，历史累计从不因窗口而丢失
        let bans = ledger.bans_in_window(168, &no_floor);
        assert_eq!(bans.get("1.2.3.4:7139").copied(), Some(2));
        assert_eq!(ledger.summary_for(proxy).total_bans, 2);
    }

    #[test]
    fn window_floor_ignores_bans_before_manual_reenable() {
        // 出口被放回来之后，之前那些封号不该立刻把它再送回隔离
        let ledger = ProxyBanLedger::new(None);
        let proxy = Some("socks5://1.2.3.4:7139");
        let now = chrono::Utc::now();
        ledger.record_ban(obs(
            1,
            proxy,
            &(now - chrono::Duration::hours(3)).to_rfc3339(),
            None,
        ));
        ledger.record_ban(obs(
            2,
            proxy,
            &(now - chrono::Duration::minutes(30)).to_rfc3339(),
            None,
        ));

        let reset_at = now - chrono::Duration::hours(1);
        let bans = ledger.bans_in_window(24, &|key| {
            (key == "1.2.3.4:7139").then_some(reset_at)
        });
        assert_eq!(bans.get("1.2.3.4:7139").copied(), Some(1));
    }

    #[test]
    fn direct_connection_tracked_separately() {
        let ledger = ProxyBanLedger::new(None);
        ledger.record_ban(obs(1, None, "2026-08-15T12:00:00+00:00", None));
        ledger.record_ban(obs(2, Some("socks5://1.2.3.4:7139"), "2026-08-15T12:00:00+00:00", None));
        let all = ledger.all_summaries();
        assert_eq!(all.get(DIRECT_KEY).unwrap().total_bans, 1);
        assert_eq!(all.get("1.2.3.4:7139").unwrap().total_bans, 1);
    }

    #[test]
    fn events_bounded_but_total_keeps_growing() {
        let ledger = ProxyBanLedger::new(None);
        let proxy = Some("socks5://1.2.3.4:7139");
        for id in 0..(MAX_EVENTS_PER_PROXY as u64 + 50) {
            ledger.record_ban(obs(id, proxy, "2026-08-15T12:00:00+00:00", None));
        }
        let summary = ledger.summary_for(proxy);
        assert_eq!(summary.total_bans, MAX_EVENTS_PER_PROXY as u64 + 50);
        assert_eq!(ledger.events_for(proxy, 10_000).len(), MAX_EVENTS_PER_PROXY);
    }

    #[test]
    fn persists_and_reloads_across_restart() {
        let dir = std::env::temp_dir().join(format!("kiro-ban-ledger-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("proxy_ban_stats.json");
        let proxy = Some("socks5://user:pass@1.2.3.4:7139");
        {
            let ledger = ProxyBanLedger::new(Some(path.clone()));
            ledger.record_ban(obs(1, proxy, "2026-08-15T12:00:00+00:00", None));
        }
        let reloaded = ProxyBanLedger::new(Some(path.clone()));
        let summary = reloaded.summary_for(proxy);
        assert_eq!(summary.total_bans, 1);
        // 落盘内容不得包含代理密码
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("pass@"), "台账不应写入代理密码: {}", raw);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backfill_is_idempotent() {
        let ledger = ProxyBanLedger::new(None);
        let proxy = Some("socks5://1.2.3.4:7139");
        let batch = vec![
            obs(1, proxy, "2026-08-15T12:00:00+00:00", None),
            obs(2, proxy, "2026-08-15T13:00:00+00:00", None),
        ];
        assert_eq!(ledger.backfill_from_credentials(batch.clone()), 2);
        assert_eq!(ledger.backfill_from_credentials(batch), 0);
        assert_eq!(ledger.summary_for(proxy).total_bans, 2);
    }

    #[test]
    fn wilson_lower_bound_penalises_small_samples() {
        // 1/1 原始就是 100%，但下界必须很低，否则一次封号就能判死一个出口
        assert!(wilson_lower_bound(1, 1) < 0.25);
        // 样本变大、比例不变时下界应单调上升
        assert!(wilson_lower_bound(50, 100) > wilson_lower_bound(5, 10));
        assert_eq!(wilson_lower_bound(0, 10), 0.0);
        assert_eq!(wilson_lower_bound(0, 0), 0.0);
    }

    /// 线上真实数据（2026-08-15）：最狠的出口是 3/8，且 8 次封号全在同一天。
    /// 正确答案是「证据不足，不建议隔离」——这条用来钉死判据不会误杀。
    #[test]
    fn production_snapshot_does_not_recommend_quarantine() {
        let summaries = pool(&[
            ("205.179.217.148:7139", summary(3, 8, 1, None, None)),
            ("204.237.153.91:7571", summary(2, 7, 1, None, None)),
            ("207.210.109.94:20000", summary(2, 5, 1, None, None)),
            ("207.145.185.220:7183", summary(1, 7, 1, None, None)),
            ("205.179.215.73:7129", summary(0, 6, 0, None, None)),
            ("204.237.146.233:7443", summary(0, 6, 0, None, None)),
        ]);
        let risk = assess_pool_risk(&summaries, 6);
        for (key, assessment) in &risk {
            assert!(
                !assessment.recommend_quarantine,
                "{} 不应被建议隔离：{:?}",
                key, assessment.reasons
            );
        }
        let worst = &risk["205.179.217.148:7139"];
        assert!(worst.ban_rate_lower_bound < BAN_RATE_LB_THRESHOLD);
        assert!(!worst.blockers.is_empty(), "必须说明为什么没下结论");
    }

    #[test]
    fn recommends_quarantine_when_every_check_passes() {
        let summaries = pool(&[
            // 8 个号封 6 个，横跨 3 个批次，远高于池内中位数
            ("1.1.1.1:1", summary(6, 8, 3, None, None)),
            ("2.2.2.2:2", summary(0, 10, 0, None, None)),
            ("3.3.3.3:3", summary(1, 12, 1, None, None)),
            ("4.4.4.4:4", summary(0, 9, 0, None, None)),
            ("5.5.5.5:5", summary(0, 9, 0, None, None)),
        ]);
        let risk = assess_pool_risk(&summaries, 5);
        assert!(risk["1.1.1.1:1"].recommend_quarantine);
        assert_eq!(risk["1.1.1.1:1"].level, RiskLevel::QuarantineRecommended);
        assert!(!risk["3.3.3.3:3"].recommend_quarantine);
        assert_eq!(risk["2.2.2.2:2"].level, RiskLevel::Ok);
    }

    #[test]
    fn systemic_high_ban_rate_blocks_blaming_one_proxy() {
        // 全池封号率都在 50% 上下：根因在打法，禁用谁都只是把号挪去烧下一个
        let summaries = pool(&[
            ("1.1.1.1:1", summary(5, 9, 3, None, None)),
            ("2.2.2.2:2", summary(5, 10, 3, None, None)),
            ("3.3.3.3:3", summary(6, 11, 3, None, None)),
            ("4.4.4.4:4", summary(5, 10, 3, None, None)),
            ("5.5.5.5:5", summary(4, 9, 3, None, None)),
        ]);
        let risk = assess_pool_risk(&summaries, 5);
        for (key, a) in &risk {
            assert!(!a.recommend_quarantine, "{} 不该被单独归咎", key);
            assert!(
                a.blockers.iter().any(|b| b.contains("全池封号率普遍偏高")),
                "{} 应给出系统性原因说明，实际: {:?}",
                key,
                a.blockers
            );
        }
    }

    #[test]
    fn single_batch_blocks_verdict() {
        let summaries = pool(&[
            ("1.1.1.1:1", summary(6, 8, 1, None, None)), // 同一批
            ("2.2.2.2:2", summary(0, 10, 0, None, None)),
            ("3.3.3.3:3", summary(0, 10, 0, None, None)),
            ("4.4.4.4:4", summary(0, 10, 0, None, None)),
            ("5.5.5.5:5", summary(0, 10, 0, None, None)),
        ]);
        let risk = assess_pool_risk(&summaries, 5);
        assert!(!risk["1.1.1.1:1"].recommend_quarantine);
        assert_eq!(risk["1.1.1.1:1"].level, RiskLevel::Suspect);
        assert!(
            risk["1.1.1.1:1"]
                .blockers
                .iter()
                .any(|b| b.contains("同一批"))
        );
    }

    #[test]
    fn fast_burn_bypasses_batch_check() {
        // 号挂上去 3 分钟就死、死前几乎没成功请求：出口已在黑名单上，
        // 这种情况不必等它横跨多个批次
        let summaries = pool(&[
            ("1.1.1.1:1", summary(4, 5, 1, Some(180), Some(0))),
            ("2.2.2.2:2", summary(1, 10, 1, Some(86400), Some(900))),
            ("3.3.3.3:3", summary(0, 10, 0, Some(86400), None)),
            ("4.4.4.4:4", summary(0, 10, 0, Some(86400), None)),
            ("5.5.5.5:5", summary(0, 10, 0, Some(86400), None)),
        ]);
        let risk = assess_pool_risk(&summaries, 5);
        assert!(risk["1.1.1.1:1"].recommend_quarantine);
        assert!(
            risk["1.1.1.1:1"]
                .reasons
                .iter()
                .any(|r| r.contains("秒死特征"))
        );
    }

    #[test]
    fn heavy_usage_before_ban_is_not_blamed_on_the_exit() {
        // 同样是秒死时长，但死前打了 5000 次成功请求 —— 号是被打死的，换 IP 没用
        let summaries = pool(&[
            ("1.1.1.1:1", summary(4, 5, 1, Some(180), Some(5000))),
            ("2.2.2.2:2", summary(0, 10, 0, Some(86400), None)),
            ("3.3.3.3:3", summary(0, 10, 0, Some(86400), None)),
            ("4.4.4.4:4", summary(0, 10, 0, Some(86400), None)),
            ("5.5.5.5:5", summary(0, 10, 0, Some(86400), None)),
        ]);
        let risk = assess_pool_risk(&summaries, 5);
        assert!(!risk["1.1.1.1:1"].recommend_quarantine);
    }

    #[test]
    fn capacity_floor_blocks_recommendation() {
        // 池子只剩 3 个可分配代理，再隔离就不够用了
        let summaries = pool(&[
            ("1.1.1.1:1", summary(6, 8, 3, None, None)),
            ("2.2.2.2:2", summary(0, 10, 0, None, None)),
            ("3.3.3.3:3", summary(0, 10, 0, None, None)),
        ]);
        let risk = assess_pool_risk(&summaries, 3);
        assert!(!risk["1.1.1.1:1"].recommend_quarantine);
        assert!(
            risk["1.1.1.1:1"]
                .blockers
                .iter()
                .any(|b| b.contains("只告警不建议动手"))
        );
    }

    /// 用户明确要求的性质：所有出口一样烂时不能有人被降权，否则等于凭空
    /// 减少可用容量，而根因（请求打法）一点没解决。
    #[test]
    fn uniform_ban_rate_keeps_everyone_at_full_weight() {
        let summaries = pool(&[
            ("1.1.1.1:1", summary(5, 10, 3, None, None)),
            ("2.2.2.2:2", summary(5, 10, 3, None, None)),
            ("3.3.3.3:3", summary(5, 10, 3, None, None)),
            ("4.4.4.4:4", summary(5, 10, 3, None, None)),
        ]);
        let risk = assess_pool_risk(&summaries, 4);
        for (key, a) in &risk {
            assert_eq!(a.selection_weight, 1.0, "{} 权重应为满值", key);
            assert_eq!(a.selection_tier, SelectionTier::Normal, "{}", key);
        }
    }

    #[test]
    fn outlier_proxy_gets_penalised_while_clean_ones_stay_full() {
        let summaries = pool(&[
            // 12 个号封 10 个，远高于其余出口
            ("bad:1", summary(10, 12, 3, None, None)),
            ("ok1:1", summary(0, 12, 0, None, None)),
            ("ok2:1", summary(1, 20, 1, None, None)),
            ("ok3:1", summary(0, 15, 0, None, None)),
        ]);
        let risk = assess_pool_risk(&summaries, 4);
        assert_eq!(risk["bad:1"].selection_tier, SelectionTier::Penalized);
        assert!(
            risk["bad:1"].selection_weight < 0.2,
            "权重应被压到很低，实际 {}",
            risk["bad:1"].selection_weight
        );
        for key in ["ok1:1", "ok2:1", "ok3:1"] {
            assert_eq!(
                risk[key].selection_tier,
                SelectionTier::Normal,
                "{} 不该被降权",
                key
            );
        }
    }

    #[test]
    fn weight_never_reaches_zero_so_a_proxy_can_recover() {
        // 权重为 0 会让出口彻底断流、统计冻结，换了干净 IP 也翻不了身
        let summaries = pool(&[
            ("bad:1", summary(50, 50, 5, None, None)),
            ("ok:1", summary(0, 50, 0, None, None)),
        ]);
        let risk = assess_pool_risk(&summaries, 2);
        assert!(risk["bad:1"].selection_weight >= MIN_SELECTION_WEIGHT);
        assert!(risk["bad:1"].selection_weight > 0.0);
    }

    #[test]
    fn small_sample_outlier_is_not_heavily_penalised() {
        // 1/1 = 100% 原始封号率，但置信下界很低，不该被打进惩罚档
        let summaries = pool(&[
            ("new:1", summary(1, 1, 1, None, None)),
            ("ok1:1", summary(0, 20, 0, None, None)),
            ("ok2:1", summary(0, 20, 0, None, None)),
        ]);
        let risk = assess_pool_risk(&summaries, 3);
        assert_ne!(
            risk["new:1"].selection_tier,
            SelectionTier::Penalized,
            "单次封号不足以把一个出口打入冷宫，权重 {}",
            risk["new:1"].selection_weight
        );
    }

    #[test]
    fn batch_days_and_median_successes_come_from_events() {
        let ledger = ProxyBanLedger::new(None);
        let proxy = Some("socks5://1.2.3.4:7139");
        for (id, day, successes) in [(1u64, "14", 0u64), (2, "15", 10), (3, "15", 40)] {
            let mut o = obs(
                id,
                proxy,
                &format!("2026-08-{}T12:00:00+00:00", day),
                Some(&format!("2026-08-{}T11:00:00+00:00", day)),
            );
            o.successes_before_ban = Some(successes);
            ledger.record_ban(o);
        }
        let s = ledger.summary_for(proxy);
        assert_eq!(s.distinct_batch_days, 2, "8-14 与 8-15 应算两个批次");
        assert_eq!(s.median_successes_before_ban, Some(10));
    }

    #[test]
    fn reset_clears_single_proxy_only() {
        let ledger = ProxyBanLedger::new(None);
        ledger.record_ban(obs(1, Some("socks5://1.1.1.1:1"), "2026-08-15T12:00:00+00:00", None));
        ledger.record_ban(obs(2, Some("socks5://2.2.2.2:2"), "2026-08-15T12:00:00+00:00", None));
        assert!(ledger.reset(Some("socks5://1.1.1.1:1")));
        assert_eq!(ledger.summary_for(Some("socks5://1.1.1.1:1")).total_bans, 0);
        assert_eq!(ledger.summary_for(Some("socks5://2.2.2.2:2")).total_bans, 1);
    }
}
