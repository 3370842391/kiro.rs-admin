//! 代理 IP 池管理
//!
//! 独立于凭据管理，存储为 proxy_pool.json
//!
//! 除增删改查外，还提供主动健康检查：周期性（或按需）通过每个代理请求一个
//! 轻量公网探测端点，记录连通性与延迟；连续探测失败达阈值的代理会被自动禁用。

use crate::admin::proxy_ban_stats::{
    ProxyBanLedger, SelectionTier, assess_pool_risk, normalize_proxy_key,
};
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// 健康检查探测端点：返回 204 No Content 的轻量公网地址，不依赖上游 Kiro。
const PROXY_HEALTH_CHECK_URL: &str = "https://www.gstatic.com/generate_204";
/// 单次探测超时（秒）
const PROXY_PROBE_TIMEOUT_SECS: u64 = 8;
/// 连续探测失败阈值：达到后自动禁用（与凭据的 MAX_FAILURES_PER_CREDENTIAL 对齐）
const MAX_PROXY_PROBE_FAILURES: u32 = 3;
/// 风险档位缓存有效期。封号是低频事件，没必要每个请求都重算 Wilson 下界。
const RISK_TIER_TTL: Duration = Duration::from_secs(60);
/// 分配出口时的排序键：档位 → 近 24h 封号 → 封号率(‰) → 累计封号 → 负载 → 延迟。
/// 见 [`ProxyPoolManager::assignment_rank`]。
type AssignmentRank = (u8, u64, u64, u64, usize, u32);

/// 被降权出口保留的探测流量比例。
///
/// 完全断流会让它的统计永远停在被降权那一刻——机场把出口换成干净 IP 之后
/// 也翻不了身。放一点流量进去，它能靠新数据自己爬回正常档。
const RISK_PROBE_RATE: f64 = 0.05;

/// 代理健康状态
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyHealth {
    /// 尚未探测
    #[default]
    Unknown,
    /// 最近一次探测成功
    Healthy,
    /// 最近一次探测失败
    Unhealthy,
}

/// 持久化的代理条目
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyEntry {
    pub id: u64,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 健康状态（健康检查结果）
    #[serde(default)]
    pub health: ProxyHealth,
    /// 最近一次成功探测的延迟（毫秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 最近一次探测时间（RFC3339）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    /// 连续探测失败计数（成功后清零）
    #[serde(default)]
    pub consecutive_failures: u32,
    /// 是否由健康检查自动禁用（区别于用户手动禁用）
    #[serde(default)]
    pub auto_disabled: bool,
    /// 因烧号被隔离的时间（RFC3339）。为 None 说明当前禁用与封号无关。
    ///
    /// 与 `auto_disabled` 并存：那个标记只说明「不是人禁的」，这个说明「为什么」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantined_at: Option<String>,
    /// 隔离原因摘要，直接给运营看
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine_reason: Option<String>,
    /// 隔离守卫的计数起点（RFC3339）。手动重新启用或自动解除时刷成当前时间。
    ///
    /// 没有它，被解除隔离的出口会因为窗口内还留着旧封号记录而立刻再次被隔离，
    /// 运营点「启用」看起来毫无效果。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard_reset_at: Option<String>,
}

impl ProxyEntry {
    fn new(id: u64, url: String, label: Option<String>) -> Self {
        Self {
            id,
            url,
            label,
            enabled: true,
            health: ProxyHealth::Unknown,
            latency_ms: None,
            last_checked_at: None,
            consecutive_failures: 0,
            auto_disabled: false,
            quarantined_at: None,
            quarantine_reason: None,
            guard_reset_at: None,
        }
    }

    /// 隔离守卫应从哪个时刻起统计封号
    pub fn guard_window_start(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        chrono::DateTime::parse_from_rfc3339(self.guard_reset_at.as_deref()?)
            .ok()
            .map(|ts| ts.with_timezone(&chrono::Utc))
    }
}

fn default_true() -> bool {
    true
}

/// 代理分配结果
pub enum GetUrlResult {
    /// 代理存在且已启用，返回 URL
    Ok(String),
    /// 代理不存在
    NotFound,
    /// 代理存在但已被禁用
    Disabled,
}

/// 一次全量健康检查的摘要
#[derive(Debug, Clone, Default)]
pub struct CheckSummary {
    /// 探测成功数
    pub healthy: usize,
    /// 探测失败数
    pub unhealthy: usize,
    /// 本轮新增的自动禁用数
    pub auto_disabled: usize,
    /// 本轮新自动禁用的 URL，供调用方把还活着的号迁走。
    pub disabled_urls: Vec<String>,
}

/// 单个代理探测结果
enum ProbeResult {
    Ok { latency_ms: u32 },
    Err { error: String },
}

pub struct ProxyPoolManager {
    entries: Mutex<Vec<ProxyEntry>>,
    runtime: Mutex<ProxyRuntimeState>,
    // 仅需原子自增，不需要与 entries 联锁；约定独立使用，无锁顺序问题
    next_id: AtomicU64,
    path: Option<PathBuf>,
    /// TLS 后端，构建探测用 HTTP client 时需要
    tls_backend: TlsBackend,
    /// 封号台账（启动时注入）。用于按封号率对出口降权。
    ban_ledger: OnceLock<Arc<ProxyBanLedger>>,
    /// 风险档位缓存，带 TTL
    risk_tiers: Mutex<RiskTierCache>,
}

#[derive(Default)]
struct RiskTierCache {
    tiers: HashMap<String, SelectionTier>,
    refreshed_at: Option<Instant>,
}

#[derive(Default)]
struct ProxyRuntimeState {
    round_robin_cursor: usize,
    in_flight: HashMap<String, usize>,
    sticky_by_credential: HashMap<u64, String>,
}

pub struct ProxyInFlightGuard<'a> {
    manager: &'a ProxyPoolManager,
    url: String,
}

impl Drop for ProxyInFlightGuard<'_> {
    fn drop(&mut self) {
        self.manager.release_in_flight(&self.url);
    }
}

/// 校验代理 URL 的 scheme 是否合法
/// 裸写法（不带 scheme）时补的默认协议。
///
/// 代理商导出的列表只有 `host:port:user:pass`，不含协议。绝大多数是 socks5，
/// 但确实有 http 的——填错会让出口在健康检查里一直失败，所以批量导入界面把它
/// 做成可选项，这里只是缺省值。
pub const DEFAULT_PROXY_SCHEME: &str = "socks5";

/// 支持的 scheme。
pub const PROXY_SCHEMES: [&str; 4] = ["socks5", "socks4", "http", "https"];

/// 脱敏用户输入；同时覆盖代理商常见的 `host:port:user:pass` 裸格式。
pub fn redact_proxy_input(raw: &str) -> String {
    let value = raw.trim();
    if value.contains("://") || value.contains('@') {
        return crate::admin::proxy_ban_stats::redact_proxy_url(value);
    }
    let mut parts = value.splitn(4, ':');
    let host = parts.next().unwrap_or_default();
    let port = parts.next().unwrap_or_default();
    if !host.is_empty() && parts.next().is_some() {
        return format!("{host}:{port}");
    }
    value.to_string()
}

fn redact_proxy_error(raw: &str, error: anyhow::Error) -> anyhow::Error {
    let safe = redact_proxy_input(raw);
    anyhow::anyhow!(error.to_string().replace(raw, &safe))
}

/// 把一条代理配置规范化成带 scheme 的 URL。
///
/// 接受三种写法：
/// - 已带 scheme：`socks5://user:pass@host:port` —— 只校验，原样返回
/// - 代理商导出格式：`host:port:user:pass` —— 各家列表 / Excel 里最常见的形态
/// - 无认证裸写法：`host:port`
///
/// `host:port:user:pass` 里密码可能自带冒号，所以只从左边切三刀，余下全算密码。
/// 用户名密码统一做百分号转义——密码里出现 `@` 或 `:` 时不转义会让 URL 的
/// 分隔符含义错乱，解析出一个完全不同的 host。
pub fn normalize_proxy_entry(raw: &str, scheme: &str) -> anyhow::Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        anyhow::bail!("代理配置为空");
    }
    if value.eq_ignore_ascii_case("direct") {
        return Ok("direct".to_string());
    }
    if value.contains("://") {
        validate_proxy_url(value)?;
        return Ok(value.to_string());
    }

    let scheme = {
        let s = scheme.trim().trim_end_matches("://").to_ascii_lowercase();
        if s.is_empty() {
            DEFAULT_PROXY_SCHEME.to_string()
        } else if PROXY_SCHEMES.contains(&s.as_str()) {
            s
        } else {
            anyhow::bail!(
                "不支持的代理协议: {scheme}（支持 {}）",
                PROXY_SCHEMES.join("/")
            )
        }
    };

    // IPv6 字面量自带冒号，不能按冒号切分
    if value.starts_with('[') {
        let candidate = format!("{scheme}://{value}");
        validate_proxy_url(&candidate)?;
        return Ok(candidate);
    }

    // 先判位置格式 host:port[:user:pass]，再判 user:pass@host:port。
    //
    // 顺序不能反：密码里带 `@` 时（如 `1.2.3.4:8080:user:p@ss`）先看 `@` 会把
    // 整串当成 auth@host，切出来的 host 是 `ss`，端口就没了。
    // 判据用第二段是不是数字端口——位置格式的第二段必然是端口，而 auth 形态的
    // 第二段是密码的一部分（`u:p@1.2.3.4` 这种），不会是纯数字。
    let mut parts = value.splitn(4, ':');
    let first = parts.next().unwrap_or_default().trim();
    let second = parts.next().unwrap_or_default().trim();
    let third = parts.next().map(str::trim);
    let rest = parts.next();
    let looks_positional = !first.is_empty() && second.parse::<u16>().is_ok();

    if looks_positional {
        let candidate = match (third, rest) {
            (Some(user), Some(pass)) if !user.is_empty() => {
                // 只在确实含 URL 分隔符时才转义：绝大多数认证是纯字母数字，
                // 无条件转义会把字面值改掉，跟代理商给的清单肉眼比对时对不上。
                format!(
                    "{scheme}://{}:{}@{first}:{second}",
                    escape_auth(user),
                    escape_auth(pass)
                )
            }
            (Some(_), None) => {
                anyhow::bail!("代理格式无法识别: {raw}（认证格式应为 host:port:用户名:密码）")
            }
            _ => format!("{scheme}://{first}:{second}"),
        };
        validate_proxy_url(&candidate)?;
        return Ok(candidate);
    }

    // user:pass@host:port，只缺协议
    if value.contains('@') {
        let candidate = format!("{scheme}://{value}");
        validate_proxy_url(&candidate)?;
        return Ok(candidate);
    }

    if first.is_empty() {
        anyhow::bail!("代理缺少主机名: {raw}");
    }
    if second.is_empty() {
        anyhow::bail!("代理缺少端口号: {raw}");
    }
    anyhow::bail!("代理端口无效: {raw}（端口应为 1-65535 的数字）")
}

/// 转义认证信息里会破坏 URL 结构的字符。
///
/// 密码里出现 `@` 或 `:` 时，`user:p@ss@host:port` 会被解析成一个完全不同的 host。
/// 只处理这几个分隔符，其余字符保持原样，方便和代理商给的清单肉眼比对。
fn escape_auth(value: &str) -> String {
    if !value.contains([':', '@', '/', '?', '#']) {
        return value.to_string();
    }
    urlencoding::encode(value).into_owned()
}

fn validate_proxy_url(url: &str) -> anyhow::Result<()> {
    let valid_schemes = ["http://", "https://", "socks5://", "socks4://"];
    if !valid_schemes.iter().any(|s| url.starts_with(s)) {
        anyhow::bail!(
            "代理 URL scheme 无效，支持: http/https/socks4/socks5（收到: {}）",
            url
        );
    }
    // 简单检查 host:port 存在
    let after_scheme = valid_schemes
        .iter()
        .find(|s| url.starts_with(*s))
        .map(|s| &url[s.len()..])
        .unwrap_or(url);
    // after_scheme 可能是 user:pass@host:port 或 host:port
    let host_part = after_scheme.rsplit('@').next().unwrap_or(after_scheme);
    if !host_part.contains(':') {
        anyhow::bail!("代理 URL 缺少端口号: {}", url);
    }
    Ok(())
}

impl ProxyPoolManager {
    pub fn new(path: Option<PathBuf>, tls_backend: TlsBackend) -> Self {
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Vec<ProxyEntry>>(&s).ok())
            .unwrap_or_default();

        let next_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;

        Self {
            entries: Mutex::new(entries),
            runtime: Mutex::new(ProxyRuntimeState::default()),
            next_id: AtomicU64::new(next_id),
            path,
            tls_backend,
            ban_ledger: OnceLock::new(),
            risk_tiers: Mutex::new(RiskTierCache::default()),
        }
    }

    /// 注入封号台账，开启按封号率降权。不注入时所有出口一律按正常档处理。
    pub fn set_ban_ledger(&self, ledger: Arc<ProxyBanLedger>) {
        let _ = self.ban_ledger.set(ledger);
        self.risk_tiers.lock().refreshed_at = None;
    }

    /// 取当前风险档位表，过期则重算。
    ///
    /// 必须在拿 `entries` / `runtime` 之前调用：它内部会短暂持有 `entries`
    /// 来数可分配代理，嵌套加锁会死锁。
    fn risk_tiers(&self) -> HashMap<String, SelectionTier> {
        let Some(ledger) = self.ban_ledger.get() else {
            return HashMap::new();
        };
        {
            let cache = self.risk_tiers.lock();
            if cache
                .refreshed_at
                .is_some_and(|at| at.elapsed() < RISK_TIER_TTL)
            {
                return cache.tiers.clone();
            }
        }

        let assignable = self.assignable_urls().len();
        let tiers: HashMap<String, SelectionTier> =
            assess_pool_risk(&ledger.all_summaries(), assignable)
                .into_iter()
                .map(|(key, assessment)| (key, assessment.selection_tier))
                .collect();

        let mut cache = self.risk_tiers.lock();
        cache.tiers = tiers.clone();
        cache.refreshed_at = Some(Instant::now());
        tiers
    }

    /// 该 URL 在本次排序中的档位。
    ///
    /// 被降权的出口有 [`RISK_PROBE_RATE`] 的概率按正常档参与，作为翻身通道。
    fn effective_tier(tiers: &HashMap<String, SelectionTier>, url: &str) -> u8 {
        let tier = tiers
            .get(&normalize_proxy_key(Some(url)))
            .copied()
            .unwrap_or(SelectionTier::Normal);
        if tier != SelectionTier::Normal && fastrand::f64() < RISK_PROBE_RATE {
            return SelectionTier::Normal.rank();
        }
        tier.rank()
    }

    pub fn list(&self) -> Vec<ProxyEntry> {
        self.entries.lock().clone()
    }

    pub fn add(&self, url: String, label: Option<String>) -> anyhow::Result<ProxyEntry> {
        let url = url.trim().to_string();
        if url.is_empty() {
            anyhow::bail!("代理 URL 不能为空");
        }
        // 单条添加同样接受 host:port:user:pass 裸写法：从代理商列表里复制一行粘进来
        // 是最自然的操作，没理由只在批量导入里支持。
        let url = normalize_proxy_entry(&url, DEFAULT_PROXY_SCHEME)
            .map_err(|error| redact_proxy_error(&url, error))?;

        let mut entries = self.entries.lock();

        if entries.iter().any(|e| e.url == url) {
            anyhow::bail!(
                "代理 URL 已存在: {}",
                crate::admin::proxy_ban_stats::redact_proxy_url(&url)
            );
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let entry = ProxyEntry::new(id, url, label);
        entries.push(entry.clone());
        drop(entries);

        self.persist()?;
        Ok(entry)
    }

    /// 批量添加：在单次加锁内完成所有插入，最后统一持久化一次。
    ///
    /// 每行都先过 [`normalize_proxy_entry`]，所以可以直接粘贴代理商导出的
    /// `host:port:user:pass` 列表。`scheme` 是裸写法补的协议，已带 scheme 的行不受影响。
    pub fn batch_add(&self, urls: Vec<String>, scheme: &str) -> (Vec<ProxyEntry>, Vec<String>) {
        let mut added = vec![];
        let mut errors = vec![];

        let mut entries = self.entries.lock();
        for raw in urls {
            let raw = raw.trim().to_string();
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }
            let url = match normalize_proxy_entry(&raw, scheme) {
                Ok(url) => url,
                Err(e) => {
                    errors.push(redact_proxy_error(&raw, e).to_string());
                    continue;
                }
            };
            if entries.iter().any(|e| e.url == url) {
                errors.push(format!(
                    "代理 URL 已存在: {}",
                    crate::admin::proxy_ban_stats::redact_proxy_url(&url)
                ));
                continue;
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let entry = ProxyEntry::new(id, url, None);
            entries.push(entry.clone());
            added.push(entry);
        }
        drop(entries);

        if !added.is_empty() {
            if let Err(e) = self.persist() {
                tracing::warn!("批量添加代理后持久化失败: {}", e);
            }
        }

        (added, errors)
    }

    pub fn delete(&self, id: u64) -> anyhow::Result<()> {
        let mut entries = self.entries.lock();
        let len_before = entries.len();
        let removed_urls: Vec<String> = entries
            .iter()
            .filter(|e| e.id == id)
            .map(|e| e.url.clone())
            .collect();
        entries.retain(|e| e.id != id);
        if entries.len() == len_before {
            anyhow::bail!("代理不存在: {}", id);
        }
        drop(entries);
        self.clear_runtime_for_urls(&removed_urls);
        self.persist()?;
        Ok(())
    }

    /// 批量删除，返回实际删除的 (id, url)。
    ///
    /// 单次加锁 + 一次持久化：逐条调 [`Self::delete`] 会写 N 次盘，几十个 IP
    /// 一起删时既慢又可能写坏（`persist` 是整文件覆写）。
    /// 不存在的 id 静默跳过——调用方多半是照着一份可能过期的列表批量操作。
    pub fn delete_many(&self, ids: &[u64]) -> anyhow::Result<Vec<(u64, String)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: std::collections::HashSet<u64> = ids.iter().copied().collect();
        let mut entries = self.entries.lock();
        let removed: Vec<(u64, String)> = entries
            .iter()
            .filter(|e| wanted.contains(&e.id))
            .map(|e| (e.id, e.url.clone()))
            .collect();
        if removed.is_empty() {
            return Ok(Vec::new());
        }
        entries.retain(|e| !wanted.contains(&e.id));
        drop(entries);

        let urls: Vec<String> = removed.iter().map(|(_, url)| url.clone()).collect();
        self.clear_runtime_for_urls(&urls);
        self.persist()?;
        Ok(removed)
    }

    /// 设置代理启用/禁用状态
    ///
    /// 用户手动启用时清除「健康检查自动禁用」标记与连续失败计数，
    /// 让该代理重新参与健康检查与分配。手动启用同时解除烧号隔离并把守卫的
    /// 计数窗口推到当前时刻，否则窗口里的旧封号会让它下一秒又被隔离回去。
    pub fn set_enabled(&self, id: u64, enabled: bool) -> anyhow::Result<()> {
        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;
        entry.enabled = enabled;
        if enabled {
            entry.auto_disabled = false;
            entry.consecutive_failures = 0;
            entry.quarantined_at = None;
            entry.quarantine_reason = None;
            entry.guard_reset_at = Some(chrono::Utc::now().to_rfc3339());
        } else {
            self.clear_runtime_for_urls(&[entry.url.clone()]);
        }
        drop(entries);
        self.persist()?;
        Ok(())
    }

    /// 因烧号隔离一个出口：停用 + 打隔离标记 + 清掉粘性绑定。
    ///
    /// 返回 false 表示该 URL 不在池子里或已处于停用状态。
    pub fn quarantine(&self, url: &str, reason: String) -> bool {
        let mut applied = false;
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.url == url)
                && entry.enabled
            {
                entry.enabled = false;
                entry.auto_disabled = true;
                entry.quarantined_at = Some(chrono::Utc::now().to_rfc3339());
                entry.quarantine_reason = Some(reason);
                applied = true;
            }
        }
        if applied {
            self.clear_runtime_for_urls(&[url.to_string()]);
            if let Err(error) = self.persist() {
                tracing::warn!(%error, "隔离烧号出口后持久化失败");
            }
        }
        applied
    }

    /// 解除隔离（自动解除通道）。计数窗口起点推到当前，避免旧封号立刻再次触发。
    pub fn release_quarantine(&self, url: &str) -> bool {
        let mut applied = false;
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.url == url)
                && entry.quarantined_at.is_some()
            {
                entry.enabled = true;
                entry.auto_disabled = false;
                entry.consecutive_failures = 0;
                entry.quarantined_at = None;
                entry.quarantine_reason = None;
                entry.guard_reset_at = Some(chrono::Utc::now().to_rfc3339());
                applied = true;
            }
        }
        if applied && let Err(error) = self.persist() {
            tracing::warn!(%error, "解除烧号隔离后持久化失败");
        }
        applied
    }

    /// 获取代理 URL，区分"不存在"和"已禁用"两种情况
    pub fn get_url(&self, id: u64) -> GetUrlResult {
        match self.entries.lock().iter().find(|e| e.id == id) {
            None => GetUrlResult::NotFound,
            Some(e) if !e.enabled => GetUrlResult::Disabled,
            Some(e) => GetUrlResult::Ok(e.url.clone()),
        }
    }

    /// 获取所有「可用于分配」的代理 URL：已启用且非 Unhealthy
    ///
    /// **只反映连通性，不含封号风险。** 用于统计容量（如
    /// [`assess_pool_risk`] 的下限检查）；真要给号**分配**出口时请用
    /// [`Self::assignable_urls_ranked`]。
    pub fn assignable_urls(&self) -> Vec<String> {
        self.entries
            .lock()
            .iter()
            .filter(|e| e.enabled && e.health != ProxyHealth::Unhealthy)
            .map(|e| e.url.clone())
            .collect()
    }

    /// 可分配出口，按「封号风险档位 → 延迟」排序，干净的在前。
    ///
    /// 给号绑定出口时必须用这个。用 [`Self::assignable_urls`] 做轮询分配会出事：
    /// 它只过滤连通性，下标轮询把烧号最多的出口和零封号出口同等对待。
    /// 线上曾出现：新号被轮询分到高封号率出口，十几分钟即死；
    /// 同批绑到零封号出口的号活了下来。
    ///
    /// 这里**不**走 [`RISK_PROBE_RATE`] 那条随机翻身通道——那是给「每请求选候选」
    /// 用的，一次请求落在降权出口上代价很小。绑定是长期的，随机把一个新号
    /// 押在惩罚档出口上不划算。要给出口翻身机会，用面板重置它的台账。
    pub fn assignable_urls_ranked(&self) -> Vec<String> {
        // 必须在拿 entries 之前取：risk_tiers 内部会加 entries 锁
        let risk_tiers = self.risk_tiers();
        let pressure = self.ban_pressures();
        let entries = self.entries.lock();
        let mut ranked: Vec<(AssignmentRank, String)> = entries
            .iter()
            .filter(|e| e.enabled && e.health != ProxyHealth::Unhealthy)
            .map(|e| {
                (
                    Self::assignment_rank(&risk_tiers, &pressure, &e.url, 0, e.latency_ms),
                    e.url.clone(),
                )
            })
            .collect();
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        ranked.into_iter().map(|(_, url)| url).collect()
    }

    /// 各出口的封号压力快照，供分配排序做同档内的次级偏好。
    fn ban_pressures(&self) -> HashMap<String, (u64, u64, u64)> {
        let Some(ledger) = self.ban_ledger.get() else {
            return HashMap::new();
        };
        ledger
            .all_summaries()
            .into_iter()
            .map(|(key, s)| {
                let rate_permille = (s.ban_rate.unwrap_or(0.0) * 1000.0).round() as u64;
                (key, (s.bans_24h, rate_permille, s.total_bans))
            })
            .collect()
    }

    /// 分配排序键：档位 → 近 24h 封号 → 封号率 → 累计封号 → 负载 → 延迟。
    ///
    /// 档位是个**高门槛的统计判断**，只在有证据证明「显著差于全池平均」时才降档。
    /// 有的出口累计烧过不少号，但接过的号更多，置信下界可能刚好压在全池基线上，
    /// 判不出显著性——只按档位排，它会和零封号的出口完全平起平坐。
    ///
    /// 所以同档内还要再比封号压力。这一层不下任何结论、不禁用任何出口，只是：
    /// 别的条件一样时，优先挑烧号更少的那个。近 24h 的权重高于累计，因为出口
    /// 是会换 IP 的，最近的证据更能说明现在的状态。
    fn assignment_rank(
        tiers: &HashMap<String, SelectionTier>,
        pressure: &HashMap<String, (u64, u64, u64)>,
        url: &str,
        load: usize,
        latency_ms: Option<u32>,
    ) -> AssignmentRank {
        let key = normalize_proxy_key(Some(url));
        let tier = tiers
            .get(&key)
            .copied()
            .unwrap_or(SelectionTier::Normal)
            .rank();
        let (bans_24h, rate_permille, total_bans) =
            pressure.get(&key).copied().unwrap_or((0, 0, 0));
        (
            tier,
            bans_24h,
            rate_permille,
            total_bans,
            load,
            latency_ms.unwrap_or(u32::MAX),
        )
    }

    /// 从可分配出口里挑一个替换目标：排除失效出口，**先看封号风险**，再看负载与延迟。
    ///
    /// 号钉死在已禁用/不健康出口上时，候选列表会空。这里给出下一个该绑的 IP，
    /// 调用方负责写回凭据。没有可替换出口时返回 `None`——绝不能因此改成直连。
    ///
    /// 风险档位必须是主键。此前只按 (负载, 延迟) 排，于是「号从一个坏出口被踢出来」
    /// 之后完全可能被自动绑到一个更能烧号的出口上——只要它当时恰好空闲。
    pub fn pick_replacement_url(
        &self,
        exclude: Option<&str>,
        loads: &HashMap<String, usize>,
    ) -> Option<String> {
        // 必须在拿 entries 之前取：risk_tiers 内部会加 entries 锁
        let risk_tiers = self.risk_tiers();
        let pressure = self.ban_pressures();
        let exclude_key = exclude.map(|url| normalize_proxy_key(Some(url)));
        let entries = self.entries.lock();
        let mut targets: Vec<(AssignmentRank, String)> = entries
            .iter()
            .filter(|entry| entry.enabled && entry.health != ProxyHealth::Unhealthy)
            .filter(|entry| {
                exclude_key
                    .as_ref()
                    .is_none_or(|key| normalize_proxy_key(Some(&entry.url)) != *key)
            })
            .map(|entry| {
                let load = loads
                    .get(&normalize_proxy_key(Some(&entry.url)))
                    .copied()
                    .unwrap_or(0);
                (
                    Self::assignment_rank(
                        &risk_tiers,
                        &pressure,
                        &entry.url,
                        load,
                        entry.latency_ms,
                    ),
                    entry.url.clone(),
                )
            })
            .collect();
        targets.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        targets.into_iter().next().map(|(_, url)| url)
    }

    fn clear_runtime_for_urls(&self, urls: &[String]) {
        if urls.is_empty() {
            return;
        }
        let mut runtime = self.runtime.lock();
        runtime
            .sticky_by_credential
            .retain(|_, sticky_url| !urls.iter().any(|url| url == sticky_url));
        for url in urls {
            runtime.in_flight.remove(url);
        }
    }

    fn is_assignable_locked(entries: &[ProxyEntry], url: &str) -> bool {
        match entries.iter().find(|e| e.url == url) {
            Some(e) => e.enabled && e.health != ProxyHealth::Unhealthy,
            None => true,
        }
    }

    fn latency_for_locked(entries: &[ProxyEntry], url: &str) -> u32 {
        entries
            .iter()
            .find(|e| e.url == url)
            .and_then(|e| e.latency_ms)
            .unwrap_or(u32::MAX)
    }

    /// 按代理均衡模式排列候选代理。
    ///
    /// - `round_robin`：进程内轮询游标。
    /// - `least_load`：当前 in-flight 最少优先，延迟作为次序。
    /// - `sticky`：若该凭据已有成功代理且仍可用，优先使用；否则先按 least_load 选，
    ///   成功后由 `report_proxy_success` 绑定。
    ///
    /// 最后统一按封号风险档位做一次**稳定**排序，档位是主键、均衡策略是次键：
    /// 烧号多的出口沉到队尾，只在干净出口用尽时才兜底。全池封号率一致时所有出口
    /// 同档，排序完全等价于原策略。粘性也让位于档位——粘在一个正在烧号的出口上
    /// 正是要避免的情况。
    pub fn order_candidates(
        &self,
        credential_id: u64,
        candidates: Vec<ProxyConfig>,
        mode: &str,
    ) -> Vec<ProxyConfig> {
        // 必须在拿 entries 之前取，risk_tiers 内部会加 entries 锁
        let risk_tiers = self.risk_tiers();
        let entries = self.entries.lock();
        let mut available = Vec::new();
        for candidate in candidates {
            if !Self::is_assignable_locked(&entries, &candidate.url) {
                continue;
            }
            if !available
                .iter()
                .any(|existing: &ProxyConfig| existing == &candidate)
            {
                available.push(candidate);
            }
        }

        if available.len() <= 1 {
            return available;
        }

        let mut ordered = self.order_by_mode(credential_id, available, mode, &entries);
        // 稳定排序：同档内保持上面均衡策略排好的顺序
        ordered.sort_by_key(|proxy| Self::effective_tier(&risk_tiers, &proxy.url));
        ordered
    }

    fn order_by_mode(
        &self,
        credential_id: u64,
        mut available: Vec<ProxyConfig>,
        mode: &str,
        entries: &[ProxyEntry],
    ) -> Vec<ProxyConfig> {
        let mut runtime = self.runtime.lock();
        let load = |url: &str, state: &ProxyRuntimeState| {
            (
                *state.in_flight.get(url).unwrap_or(&0),
                Self::latency_for_locked(&entries, url),
                url.to_string(),
            )
        };

        match mode {
            "round_robin" => {
                let offset = runtime.round_robin_cursor % available.len();
                runtime.round_robin_cursor = runtime.round_robin_cursor.wrapping_add(1);
                available.rotate_left(offset);
                available
            }
            "least_load" => {
                available.sort_by_key(|proxy| load(&proxy.url, &runtime));
                available
            }
            "sticky" => {
                if let Some(sticky_url) = runtime.sticky_by_credential.get(&credential_id).cloned()
                    && let Some(pos) = available.iter().position(|proxy| proxy.url == sticky_url)
                {
                    let sticky = available.remove(pos);
                    available.sort_by_key(|proxy| load(&proxy.url, &runtime));
                    available.insert(0, sticky);
                    return available;
                }
                available.sort_by_key(|proxy| load(&proxy.url, &runtime));
                available
            }
            _ => {
                available.sort_by_key(|proxy| load(&proxy.url, &runtime));
                available
            }
        }
    }

    pub fn in_flight_guard(&self, proxy: &ProxyConfig) -> ProxyInFlightGuard<'_> {
        let url = proxy.url.clone();
        let mut runtime = self.runtime.lock();
        *runtime.in_flight.entry(url.clone()).or_insert(0) += 1;
        ProxyInFlightGuard { manager: self, url }
    }

    fn release_in_flight(&self, url: &str) {
        let mut runtime = self.runtime.lock();
        if let Some(count) = runtime.in_flight.get_mut(url) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                runtime.in_flight.remove(url);
            }
        }
    }

    pub fn report_proxy_success(&self, credential_id: u64, proxy: &ProxyConfig) {
        {
            let mut runtime = self.runtime.lock();
            runtime
                .sticky_by_credential
                .insert(credential_id, proxy.url.clone());
        }

        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.url == proxy.url) {
            entry.health = ProxyHealth::Healthy;
            entry.consecutive_failures = 0;
        }
    }

    /// 记录运行时代理失败。若该 URL 存在于代理池，连续失败达到阈值会自动禁用并持久化。
    ///
    /// 返回本次新自动禁用的 URL，调用方应立刻把还活着的号迁到健康出口。
    pub fn report_proxy_failure(&self, credential_id: u64, proxy: &ProxyConfig) -> Option<String> {
        {
            let mut runtime = self.runtime.lock();
            if runtime
                .sticky_by_credential
                .get(&credential_id)
                .map(|url| url == &proxy.url)
                .unwrap_or(false)
            {
                runtime.sticky_by_credential.remove(&credential_id);
            }
        }

        let mut changed = false;
        let mut disabled_url: Option<String> = None;
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.url == proxy.url) {
                let (_, newly_disabled) = Self::apply_probe_result(
                    entry,
                    &ProbeResult::Err {
                        error: "运行时请求失败".to_string(),
                    },
                );
                changed = true;
                if newly_disabled {
                    disabled_url = Some(entry.url.clone());
                }
            }
        }

        if let Some(url) = &disabled_url {
            self.clear_runtime_for_urls(&[url.clone()]);
        }
        if changed && let Err(e) = self.persist() {
            tracing::warn!("记录运行时代理失败后持久化失败: {}", e);
        }
        disabled_url
    }

    fn persist(&self) -> anyhow::Result<()> {
        let path = match &self.path {
            Some(p) => p,
            None => return Ok(()),
        };
        let entries = self.entries.lock();
        let json = serde_json::to_string_pretty(&*entries)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

// ============ 健康检查 ============

impl ProxyPoolManager {
    /// 探测单个代理 URL 的连通性与延迟。
    ///
    /// 通过该代理请求 `PROXY_HEALTH_CHECK_URL`，成功（HTTP 2xx/3xx）即视为连通，
    /// 返回往返延迟；任何网络错误或非预期状态码视为失败。
    async fn probe_one(&self, url: &str) -> ProbeResult {
        let proxy = ProxyConfig::new(url);
        let client = match build_client(Some(&proxy), PROXY_PROBE_TIMEOUT_SECS, self.tls_backend) {
            Ok(c) => c,
            Err(e) => {
                return ProbeResult::Err {
                    error: format!("构建探测 client 失败: {}", e),
                };
            }
        };

        let started = Instant::now();
        match client.get(PROXY_HEALTH_CHECK_URL).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() || status.is_redirection() {
                    ProbeResult::Ok {
                        latency_ms: started.elapsed().as_millis().min(u32::MAX as u128) as u32,
                    }
                } else {
                    ProbeResult::Err {
                        error: format!("探测端点返回非预期状态: {}", status),
                    }
                }
            }
            Err(e) => ProbeResult::Err {
                error: e.to_string(),
            },
        }
    }

    /// 将一次探测结果回写到指定条目，并按需触发自动禁用。
    ///
    /// 返回 `(变为不健康, 本次新自动禁用)` 供摘要统计。
    fn apply_probe_result(entry: &mut ProxyEntry, result: &ProbeResult) -> (bool, bool) {
        entry.last_checked_at = Some(chrono::Utc::now().to_rfc3339());
        match result {
            ProbeResult::Ok { latency_ms } => {
                entry.health = ProxyHealth::Healthy;
                entry.latency_ms = Some(*latency_ms);
                entry.consecutive_failures = 0;
                (false, false)
            }
            ProbeResult::Err { error } => {
                entry.health = ProxyHealth::Unhealthy;
                entry.latency_ms = None;
                entry.consecutive_failures += 1;
                tracing::warn!(
                    "代理 #{} 探测失败（{}/{}）: {}",
                    entry.id,
                    entry.consecutive_failures,
                    MAX_PROXY_PROBE_FAILURES,
                    error
                );
                let mut newly_disabled = false;
                if entry.consecutive_failures >= MAX_PROXY_PROBE_FAILURES && entry.enabled {
                    entry.enabled = false;
                    entry.auto_disabled = true;
                    newly_disabled = true;
                    tracing::error!(
                        "代理 #{} 连续探测失败 {} 次，已自动禁用",
                        entry.id,
                        entry.consecutive_failures
                    );
                }
                (true, newly_disabled)
            }
        }
    }

    /// 全量健康检查：并发探测所有「已启用」代理，回写结果并持久化一次。
    ///
    /// 仅探测当前 enabled 的条目；用户/自动禁用的条目跳过（手动重新启用会清零计数）。
    pub async fn check_all(&self) -> CheckSummary {
        // 快照待探测的 (id, url)，避免长时间持锁
        let targets: Vec<(u64, String)> = self
            .entries
            .lock()
            .iter()
            .filter(|e| e.enabled)
            .map(|e| (e.id, e.url.clone()))
            .collect();

        if targets.is_empty() {
            return CheckSummary::default();
        }

        let probes = targets
            .iter()
            .map(|(id, url)| async move { (*id, self.probe_one(url).await) });
        let results = futures::future::join_all(probes).await;

        let mut summary = CheckSummary::default();
        let mut disabled_urls = Vec::new();
        {
            let mut entries = self.entries.lock();
            for (id, result) in &results {
                if let Some(entry) = entries.iter_mut().find(|e| e.id == *id) {
                    let (unhealthy, newly_disabled) = Self::apply_probe_result(entry, result);
                    if unhealthy {
                        summary.unhealthy += 1;
                    } else {
                        summary.healthy += 1;
                    }
                    if newly_disabled {
                        summary.auto_disabled += 1;
                        disabled_urls.push(entry.url.clone());
                        summary.disabled_urls.push(entry.url.clone());
                    }
                }
            }
        }
        self.clear_runtime_for_urls(&disabled_urls);

        if let Err(e) = self.persist() {
            tracing::warn!("健康检查后持久化失败: {}", e);
        }
        summary
    }

    /// 单个代理即时探测（供 UI「测试」按钮调用），回写结果并持久化。
    pub async fn check_one(&self, id: u64) -> anyhow::Result<ProxyEntry> {
        let url = self
            .entries
            .lock()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.url.clone())
            .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;

        let result = self.probe_one(&url).await;

        let entry = {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;
            let (_, newly_disabled) = Self::apply_probe_result(entry, &result);
            if newly_disabled {
                self.clear_runtime_for_urls(&[entry.url.clone()]);
            }
            entry.clone()
        };

        self.persist()?;
        Ok(entry)
    }

    /// 临时探测任意代理 URL，不写入代理池、不影响启用状态。
    pub async fn check_url(&self, url: &str) -> anyhow::Result<ProxyEntry> {
        let url = url.trim();
        if url.is_empty() {
            anyhow::bail!("代理 URL 不能为空");
        }
        validate_proxy_url(url)?;

        let result = self.probe_one(url).await;
        let mut entry = ProxyEntry::new(0, url.to_string(), None);
        Self::apply_probe_result(&mut entry, &result);
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(url: &str) -> ProxyEntry {
        ProxyEntry::new(1, url.to_string(), None)
    }

    #[test]
    fn old_json_without_new_fields_deserializes() {
        // 旧格式 JSON 只有 id/url/label/enabled，新字段应由 serde default 补全
        let json = r#"[{"id":1,"url":"socks5://127.0.0.1:1080","enabled":true}]"#;
        let entries: Vec<ProxyEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.health, ProxyHealth::Unknown);
        assert_eq!(e.latency_ms, None);
        assert_eq!(e.consecutive_failures, 0);
        assert!(!e.auto_disabled);
        assert_eq!(e.quarantined_at, None);
        assert_eq!(e.guard_reset_at, None);
    }

    #[test]
    fn quarantine_disables_and_records_reason() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let url = "socks5://127.0.0.1:1080".to_string();
        mgr.add(url.clone(), None).unwrap();

        assert!(mgr.quarantine(&url, "烧了 2 个号".to_string()));
        let e = mgr.list().into_iter().next().unwrap();
        assert!(!e.enabled);
        assert!(e.auto_disabled);
        assert!(e.quarantined_at.is_some());
        assert_eq!(e.quarantine_reason.as_deref(), Some("烧了 2 个号"));
        assert!(mgr.assignable_urls().is_empty());

        // 已停用的出口不会被重复隔离
        assert!(!mgr.quarantine(&url, "再来一次".to_string()));
    }

    #[test]
    fn releasing_quarantine_pushes_guard_window_forward() {
        // 没有这一步，运营点「启用」之后窗口里的旧封号会立刻把出口再隔离回去，
        // 看起来就像按钮没生效
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let url = "socks5://127.0.0.1:1080".to_string();
        let entry = mgr.add(url.clone(), None).unwrap();

        mgr.quarantine(&url, "烧号".to_string());
        assert!(mgr.release_quarantine(&url));
        let e = mgr.list().into_iter().next().unwrap();
        assert!(e.enabled);
        assert!(!e.auto_disabled);
        assert_eq!(e.quarantine_reason, None);
        assert!(e.guard_window_start().is_some());

        // 未处于隔离的出口不需要解除
        assert!(!mgr.release_quarantine(&url));

        // 手动启用同样刷新窗口起点
        mgr.quarantine(&url, "又烧号".to_string());
        mgr.set_enabled(entry.id, true).unwrap();
        let e = mgr.list().into_iter().next().unwrap();
        assert!(e.enabled);
        assert_eq!(e.quarantined_at, None);
        assert!(e.guard_window_start().is_some());
    }

    #[test]
    fn quarantined_proxy_drops_out_of_candidates() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        mgr.add("http://proxy-a:8080".to_string(), None).unwrap();
        mgr.add("http://proxy-b:8080".to_string(), None).unwrap();

        mgr.quarantine("http://proxy-a:8080", "烧号".to_string());
        let ordered = mgr.order_candidates(
            1,
            vec![
                ProxyConfig::new("http://proxy-a:8080"),
                ProxyConfig::new("http://proxy-b:8080"),
            ],
            "least_load",
        );
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].url, "http://proxy-b:8080");
    }

    #[test]
    fn probe_failure_increments_and_auto_disables_at_threshold() {
        let mut entry = make_entry("socks5://127.0.0.1:1080");
        let err = ProbeResult::Err {
            error: "connection refused".to_string(),
        };
        // 前两次失败：计数累加，仍启用
        for n in 1..MAX_PROXY_PROBE_FAILURES {
            let (unhealthy, disabled) = ProxyPoolManager::apply_probe_result(&mut entry, &err);
            assert!(unhealthy);
            assert!(!disabled);
            assert_eq!(entry.consecutive_failures, n);
            assert!(entry.enabled);
            assert!(!entry.auto_disabled);
        }
        // 第 N 次失败：自动禁用
        let (_, disabled) = ProxyPoolManager::apply_probe_result(&mut entry, &err);
        assert!(disabled);
        assert_eq!(entry.consecutive_failures, MAX_PROXY_PROBE_FAILURES);
        assert!(!entry.enabled);
        assert!(entry.auto_disabled);
    }

    #[test]
    fn probe_success_clears_failures_and_marks_healthy() {
        let mut entry = make_entry("socks5://127.0.0.1:1080");
        entry.consecutive_failures = 2;
        entry.health = ProxyHealth::Unhealthy;
        let ok = ProbeResult::Ok { latency_ms: 123 };
        let (unhealthy, disabled) = ProxyPoolManager::apply_probe_result(&mut entry, &ok);
        assert!(!unhealthy);
        assert!(!disabled);
        assert_eq!(entry.consecutive_failures, 0);
        assert_eq!(entry.health, ProxyHealth::Healthy);
        assert_eq!(entry.latency_ms, Some(123));
    }

    #[test]
    fn set_enabled_true_clears_auto_disable_state() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let entry = mgr
            .add("socks5://127.0.0.1:1080".to_string(), None)
            .unwrap();
        // 模拟自动禁用状态
        {
            let mut entries = mgr.entries.lock();
            let e = entries.iter_mut().find(|e| e.id == entry.id).unwrap();
            e.enabled = false;
            e.auto_disabled = true;
            e.consecutive_failures = MAX_PROXY_PROBE_FAILURES;
        }
        mgr.set_enabled(entry.id, true).unwrap();
        let list = mgr.list();
        let e = list.iter().find(|e| e.id == entry.id).unwrap();
        assert!(e.enabled);
        assert!(!e.auto_disabled);
        assert_eq!(e.consecutive_failures, 0);
    }

    #[test]
    fn sticky_mode_reuses_success_proxy_until_failure() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        mgr.add("http://proxy-a:8080".to_string(), None).unwrap();
        mgr.add("http://proxy-b:8080".to_string(), None).unwrap();
        let proxy_a = ProxyConfig::new("http://proxy-a:8080");
        let proxy_b = ProxyConfig::new("http://proxy-b:8080");

        mgr.report_proxy_success(7, &proxy_b);
        let ordered = mgr.order_candidates(7, vec![proxy_a.clone(), proxy_b.clone()], "sticky");
        assert_eq!(
            ordered.first().map(|p| p.url.as_str()),
            Some(proxy_b.url.as_str())
        );

        mgr.report_proxy_failure(7, &proxy_b);
        let ordered = mgr.order_candidates(7, vec![proxy_a.clone(), proxy_b], "sticky");
        assert_eq!(ordered, vec![proxy_a]);
    }

    /// 造一个台账：`bans` 里每项是 (代理 URL, 封号数, 曾绑定账号数)
    fn ledger_with(bans: &[(&str, u64, u64)]) -> Arc<ProxyBanLedger> {
        use crate::admin::proxy_ban_stats::BanObservation;
        let ledger = Arc::new(ProxyBanLedger::new(None));
        let mut next_id = 1u64;
        for (url, banned, seen) in bans {
            let ids: Vec<u64> = (0..*seen)
                .map(|_| {
                    let i = next_id;
                    next_id += 1;
                    i
                })
                .collect();
            ledger.observe_bindings(ids.iter().map(|id| (Some(url.to_string()), *id)));
            for (n, id) in ids.iter().take(*banned as usize).enumerate() {
                ledger.record_ban(BanObservation {
                    credential_id: *id,
                    email: None,
                    // 跨多个批次，避免被「同一批号」检验挡掉
                    banned_at: format!("2026-08-{:02}T12:00:00+00:00", 10 + (n % 5)),
                    added_at: Some(format!("2026-08-{:02}T10:00:00+00:00", 10 + (n % 5))),
                    reason: None,
                    proxy_url: Some(url.to_string()),
                    successes_before_ban: None,
                    requests_before_ban: None,
                });
            }
        }
        ledger
    }

    fn mgr_with_proxies(urls: &[&str], ledger: Arc<ProxyBanLedger>) -> ProxyPoolManager {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        for url in urls {
            mgr.add(url.to_string(), None).unwrap();
        }
        mgr.set_ban_ledger(ledger);
        mgr
    }

    /// 代理商导出的清单可以直接粘进来（host:port:user:pass）。
    #[test]
    fn provider_export_format_is_accepted_verbatim() {
        let cases = [
            (
                "203.0.113.10:1080:user1:pass1",
                "socks5://user1:pass1@203.0.113.10:1080",
            ),
            (
                "203.0.113.11:1081:user2:pass2",
                "socks5://user2:pass2@203.0.113.11:1081",
            ),
            (
                "198.51.100.20:1080:user3:pass3",
                "socks5://user3:pass3@198.51.100.20:1080",
            ),
            (
                "198.51.100.21:1081:user4:pass4",
                "socks5://user4:pass4@198.51.100.21:1081",
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                normalize_proxy_entry(raw, DEFAULT_PROXY_SCHEME).unwrap(),
                expected,
                "输入: {raw}"
            );
        }
    }

    #[test]
    fn bare_forms_and_explicit_scheme_all_normalize() {
        // 无认证
        assert_eq!(
            normalize_proxy_entry("1.2.3.4:8080", "socks5").unwrap(),
            "socks5://1.2.3.4:8080"
        );
        // 指定协议
        assert_eq!(
            normalize_proxy_entry("1.2.3.4:8080:u:p", "http").unwrap(),
            "http://u:p@1.2.3.4:8080"
        );
        // 已带 scheme 的原样返回，不受默认协议影响
        assert_eq!(
            normalize_proxy_entry("http://u:p@1.2.3.4:8080", "socks5").unwrap(),
            "http://u:p@1.2.3.4:8080"
        );
        // 无 scheme 的 user:pass@host:port
        assert_eq!(
            normalize_proxy_entry("u:p@1.2.3.4:8080", "socks5").unwrap(),
            "socks5://u:p@1.2.3.4:8080"
        );
        // direct 直连候选
        assert_eq!(normalize_proxy_entry("DIRECT", "socks5").unwrap(), "direct");
    }

    #[test]
    fn password_containing_separators_is_escaped_not_misparsed() {
        // 密码里带 @ 时不转义会被解析成另一个 host
        let url = normalize_proxy_entry("1.2.3.4:8080:user:p@ss", "socks5").unwrap();
        assert!(
            url.ends_with("@1.2.3.4:8080"),
            "主机部分必须仍是 1.2.3.4:8080，实际 {url}"
        );
        assert!(!url.contains("p@ss"), "密码里的 @ 应被转义，实际 {url}");

        // 密码自带冒号：只从左边切三刀，余下全算密码
        let url = normalize_proxy_entry("1.2.3.4:8080:user:a:b:c", "socks5").unwrap();
        assert!(url.ends_with("@1.2.3.4:8080"), "实际 {url}");
        assert!(url.contains("user:"), "实际 {url}");
    }

    #[test]
    fn malformed_entries_are_rejected_with_actionable_messages() {
        for bad in [
            "1.2.3.4",               // 缺端口
            "1.2.3.4:notaport",      // 端口非数字
            "1.2.3.4:99999",         // 端口越界
            ":8080",                 // 缺主机
            "1.2.3.4:8080:onlyuser", // 只有用户名没有密码
        ] {
            assert!(
                normalize_proxy_entry(bad, "socks5").is_err(),
                "应当拒绝: {bad}"
            );
        }
        assert!(
            normalize_proxy_entry("1.2.3.4:8080", "ftp").is_err(),
            "不支持的协议应报错"
        );
        let (_, errors) = ProxyPoolManager::new(None, TlsBackend::Rustls).batch_add(
            vec!["1.2.3.4:99999:user:pass".to_string()],
            "socks5",
        );
        assert!(!errors[0].contains("user:pass"), "裸格式错误不能回显认证");
    }

    #[test]
    fn batch_add_accepts_provider_export_lines() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let (added, errors) = mgr.batch_add(
            vec![
                "# sample proxies".to_string(),
                "203.0.113.10:1080:user1:pass1".to_string(),
                "203.0.113.11:1081:user2:pass2".to_string(),
                "socks5://already:scheme@9.9.9.9:1080".to_string(),
                "坏数据".to_string(),
            ],
            DEFAULT_PROXY_SCHEME,
        );
        assert_eq!(added.len(), 3, "注释跳过、坏行报错，其余入池");
        assert_eq!(errors.len(), 1);
        assert!(
            added
                .iter()
                .any(|e| e.url == "socks5://user1:pass1@203.0.113.10:1080")
        );
    }

    #[test]
    fn batch_add_dedupes_after_normalization() {
        // 同一个出口用两种写法写两遍，应当只入池一次
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let (added, errors) = mgr.batch_add(
            vec![
                "1.2.3.4:8080:u:p".to_string(),
                "socks5://u:p@1.2.3.4:8080".to_string(),
            ],
            DEFAULT_PROXY_SCHEME,
        );
        assert_eq!(added.len(), 1, "规范化之后是同一条，不该重复入池");
        assert_eq!(errors.len(), 1);
        assert!(!errors[0].contains("u:p@"), "错误消息不能回显代理认证");
    }

    #[test]
    fn delete_many_removes_only_requested_and_ignores_unknown_ids() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let a = mgr.add("http://a:8080".to_string(), None).unwrap();
        let b = mgr.add("http://b:8080".to_string(), None).unwrap();
        let c = mgr.add("http://c:8080".to_string(), None).unwrap();

        // 混入一个不存在的 id：调用方多半照着一份可能过期的列表批量操作，不该整批失败
        let removed = mgr.delete_many(&[a.id, c.id, 99_999]).unwrap();
        let removed_ids: Vec<u64> = removed.iter().map(|(id, _)| *id).collect();
        assert_eq!(removed_ids, vec![a.id, c.id]);

        let left: Vec<u64> = mgr.list().iter().map(|e| e.id).collect();
        assert_eq!(left, vec![b.id]);
    }

    #[test]
    fn delete_many_is_a_noop_for_empty_or_all_unknown() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        mgr.add("http://a:8080".to_string(), None).unwrap();

        assert!(mgr.delete_many(&[]).unwrap().is_empty());
        assert!(mgr.delete_many(&[12_345]).unwrap().is_empty());
        assert_eq!(mgr.list().len(), 1, "不该误删");
    }

    #[test]
    fn delete_many_clears_sticky_binding_of_removed_proxy() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let a = mgr.add("http://a:8080".to_string(), None).unwrap();
        mgr.add("http://b:8080".to_string(), None).unwrap();
        let proxy_a = ProxyConfig::new("http://a:8080");
        mgr.report_proxy_success(42, &proxy_a);

        mgr.delete_many(&[a.id]).unwrap();

        // 粘性还指着已删除的出口的话，该凭据会一直拿到一个池里已经没有的候选
        let ordered = mgr.order_candidates(42, vec![ProxyConfig::new("http://b:8080")], "sticky");
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].url, "http://b:8080");
    }

    /// 回归：分配出口必须先看封号台账。
    ///
    /// 2026-09-01 线上一次导入 7 个号，轮询分配用的是只过滤连通性的
    /// `assignable_urls()`，其中一个被分到 33 个号烧了 9 个的出口上，22 分钟即死；
    /// 同批绑到零封号出口的号活了下来。
    #[test]
    fn ranked_assignment_puts_clean_exits_first() {
        let urls = [
            "http://dirty:8080",
            "http://clean1:8080",
            "http://clean2:8080",
            "http://clean3:8080",
        ];
        let mgr = mgr_with_proxies(
            &urls,
            ledger_with(&[
                ("http://dirty:8080", 9, 33),
                ("http://clean1:8080", 0, 20),
                ("http://clean2:8080", 0, 18),
                ("http://clean3:8080", 1, 25),
            ]),
        );

        let ranked = mgr.assignable_urls_ranked();
        assert_eq!(ranked.len(), urls.len(), "不应丢候选，只是重排");
        assert_eq!(
            ranked.last().map(String::as_str),
            Some("http://dirty:8080"),
            "烧号最多的出口必须排在最后，实际顺序 {ranked:?}"
        );

        // 与旧接口对比：旧接口不看台账，dirty 完全可能排在最前
        let plain = mgr.assignable_urls();
        assert_eq!(plain.len(), ranked.len());

        // 只分配 3 个号时，拿到的应当全是干净出口
        let first_three: Vec<&str> = ranked.iter().take(3).map(String::as_str).collect();
        assert!(
            !first_three.contains(&"http://dirty:8080"),
            "前 3 个不该包含脏出口，实际 {first_three:?}"
        );
    }

    /// 同档内也要挑更干净的。
    ///
    /// 档位是高门槛的统计判断：有的出口累计烧过不少号，但接过的号更多，
    /// 置信下界恰好压在全池基线上，算不出显著性，仍是 Normal 档。只按档位排，
    /// 它就和零封号出口平起平坐——轮询照样可能把新号分给它。
    #[test]
    fn same_tier_still_prefers_the_cleaner_exit() {
        let urls = [
            "http://burned:8080",
            "http://spotless:8080",
            "http://light:8080",
        ];
        let mgr = mgr_with_proxies(
            &urls,
            // 三者封号率相近，都判不出显著性，因此同处 Normal 档
            ledger_with(&[
                ("http://burned:8080", 8, 43),
                ("http://light:8080", 2, 25),
                ("http://spotless:8080", 0, 30),
            ]),
        );

        let tiers = mgr.risk_tiers();
        for url in urls {
            let tier = tiers
                .get(&normalize_proxy_key(Some(url)))
                .copied()
                .unwrap_or(SelectionTier::Normal);
            assert_eq!(tier, SelectionTier::Normal, "{url} 前提是同档");
        }

        let ranked = mgr.assignable_urls_ranked();
        assert_eq!(
            ranked.first().map(String::as_str),
            Some("http://spotless:8080"),
            "同档内应优先零封号出口，实际顺序 {ranked:?}"
        );
        assert_eq!(
            ranked.last().map(String::as_str),
            Some("http://burned:8080"),
            "同档内烧号最多的应排最后，实际顺序 {ranked:?}"
        );
    }

    /// 没有台账时行为不变：所有出口同等对待，只按延迟排。
    #[test]
    fn ranking_without_ledger_keeps_every_exit_equal() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        for url in ["http://a:8080", "http://b:8080", "http://c:8080"] {
            mgr.add(url.to_string(), None).unwrap();
        }
        let ranked = mgr.assignable_urls_ranked();
        assert_eq!(ranked.len(), 3);
        assert_eq!(
            mgr.assignable_urls().len(),
            ranked.len(),
            "未注入台账时不应丢候选"
        );
    }

    /// 排序必须是确定的：分配是长期绑定，不能像每请求选候选那样掺随机探测流量。
    #[test]
    fn ranked_assignment_is_deterministic() {
        let urls = ["http://dirty:8080", "http://clean:8080"];
        let mgr = mgr_with_proxies(
            &urls,
            ledger_with(&[("http://dirty:8080", 9, 33), ("http://clean:8080", 0, 30)]),
        );
        let first = mgr.assignable_urls_ranked();
        for _ in 0..50 {
            assert_eq!(mgr.assignable_urls_ranked(), first, "分配顺序不应随机抖动");
        }
    }

    /// 自动改绑也要看台账：号从坏出口被踢出来，不能又被塞进一个更能烧号的出口。
    #[test]
    fn replacement_prefers_clean_exit_over_idle_dirty_one() {
        let urls = ["http://dirty:8080", "http://clean:8080", "http://old:8080"];
        let mgr = mgr_with_proxies(
            &urls,
            ledger_with(&[
                ("http://dirty:8080", 9, 33),
                ("http://clean:8080", 0, 25),
                ("http://old:8080", 0, 25),
            ]),
        );
        // 故意让干净出口负载更高：旧实现按 (负载, 延迟) 排序会挑空闲的脏出口
        let mut loads = HashMap::new();
        loads.insert(normalize_proxy_key(Some("http://clean:8080")), 5usize);
        loads.insert(normalize_proxy_key(Some("http://dirty:8080")), 0usize);

        let picked = mgr.pick_replacement_url(Some("http://old:8080"), &loads);
        assert_eq!(
            picked.as_deref(),
            Some("http://clean:8080"),
            "风险档位是主键，负载只在同档内比较"
        );
    }

    #[test]
    fn burning_proxy_sinks_to_the_back_of_the_candidate_list() {
        let urls = [
            "http://bad:8080",
            "http://ok1:8080",
            "http://ok2:8080",
            "http://ok3:8080",
        ];
        let mgr = mgr_with_proxies(
            &urls,
            ledger_with(&[
                ("http://bad:8080", 10, 12),
                ("http://ok1:8080", 0, 12),
                ("http://ok2:8080", 0, 12),
                ("http://ok3:8080", 1, 20),
            ]),
        );
        let candidates: Vec<ProxyConfig> = urls.iter().map(|u| ProxyConfig::new(*u)).collect();

        // 降权档保留少量探测流量，所以单次结果有随机性；统计多次看趋势
        let mut bad_first = 0;
        for _ in 0..200 {
            let ordered = mgr.order_candidates(1, candidates.clone(), "least_load");
            if ordered.first().map(|p| p.url.as_str()) == Some("http://bad:8080") {
                bad_first += 1;
            }
        }
        assert!(
            bad_first < 30,
            "烧号出口不该经常排在首位，200 次里出现了 {} 次",
            bad_first
        );
        assert!(bad_first > 0, "应保留探测流量供其翻身，实际完全断流");
    }

    #[test]
    fn uniform_risk_leaves_ordering_to_the_balancing_mode() {
        // 全池一样烂：不该有人被降权，排序必须完全等价于原策略
        let urls = ["http://a:8080", "http://b:8080", "http://c:8080"];
        let mgr = mgr_with_proxies(
            &urls,
            ledger_with(&[
                ("http://a:8080", 5, 10),
                ("http://b:8080", 5, 10),
                ("http://c:8080", 5, 10),
            ]),
        );
        let candidates: Vec<ProxyConfig> = urls.iter().map(|u| ProxyConfig::new(*u)).collect();

        let proxy_a = ProxyConfig::new("http://a:8080");
        let _guard = mgr.in_flight_guard(&proxy_a);
        // least_load 应照常把在途最少的排前面，A 因为在途 1 被排后
        for _ in 0..20 {
            let ordered = mgr.order_candidates(1, candidates.clone(), "least_load");
            assert_ne!(
                ordered.first().map(|p| p.url.as_str()),
                Some("http://a:8080"),
                "同档时应完全由 least_load 决定顺序"
            );
        }
    }

    #[test]
    fn without_ledger_ordering_is_unchanged() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        mgr.add("http://proxy-a:8080".to_string(), None).unwrap();
        mgr.add("http://proxy-b:8080".to_string(), None).unwrap();
        let proxy_a = ProxyConfig::new("http://proxy-a:8080");
        let proxy_b = ProxyConfig::new("http://proxy-b:8080");
        let _guard = mgr.in_flight_guard(&proxy_a);
        let ordered = mgr.order_candidates(1, vec![proxy_a, proxy_b.clone()], "least_load");
        assert_eq!(
            ordered.first().map(|p| p.url.as_str()),
            Some(proxy_b.url.as_str())
        );
    }

    #[test]
    fn least_load_mode_prefers_lower_in_flight_proxy() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        mgr.add("http://proxy-a:8080".to_string(), None).unwrap();
        mgr.add("http://proxy-b:8080".to_string(), None).unwrap();
        let proxy_a = ProxyConfig::new("http://proxy-a:8080");
        let proxy_b = ProxyConfig::new("http://proxy-b:8080");

        let _guard = mgr.in_flight_guard(&proxy_a);
        let ordered = mgr.order_candidates(1, vec![proxy_a.clone(), proxy_b.clone()], "least_load");
        assert_eq!(
            ordered.first().map(|p| p.url.as_str()),
            Some(proxy_b.url.as_str())
        );
    }

    #[test]
    fn pick_replacement_skips_unusable_and_prefers_lower_load() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let dead = mgr.add("http://dead:8080".to_string(), None).unwrap();
        mgr.add("http://busy:8080".to_string(), None).unwrap();
        mgr.add("http://free:8080".to_string(), None).unwrap();
        mgr.set_enabled(dead.id, false).unwrap();

        let mut loads = HashMap::new();
        loads.insert(normalize_proxy_key(Some("http://busy:8080")), 5);
        loads.insert(normalize_proxy_key(Some("http://free:8080")), 1);

        let picked = mgr
            .pick_replacement_url(Some("http://dead:8080"), &loads)
            .unwrap();
        assert_eq!(picked, "http://free:8080");
    }

    #[test]
    fn pick_replacement_returns_none_when_pool_has_nothing_else() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let dead = mgr.add("http://dead:8080".to_string(), None).unwrap();
        mgr.set_enabled(dead.id, false).unwrap();
        assert!(
            mgr.pick_replacement_url(Some("http://dead:8080"), &HashMap::new())
                .is_none()
        );
    }

    #[test]
    fn report_proxy_failure_returns_url_only_when_auto_disabled() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        mgr.add("http://flaky:8080".to_string(), None).unwrap();
        let proxy = ProxyConfig::new("http://flaky:8080");
        assert!(mgr.report_proxy_failure(1, &proxy).is_none());
        assert!(mgr.report_proxy_failure(1, &proxy).is_none());
        assert_eq!(
            mgr.report_proxy_failure(1, &proxy).as_deref(),
            Some("http://flaky:8080")
        );
    }
}
