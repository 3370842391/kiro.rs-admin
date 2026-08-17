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
        validate_proxy_url(&url)?;

        let mut entries = self.entries.lock();

        if entries.iter().any(|e| e.url == url) {
            anyhow::bail!("代理 URL 已存在: {}", url);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let entry = ProxyEntry::new(id, url, label);
        entries.push(entry.clone());
        drop(entries);

        self.persist()?;
        Ok(entry)
    }

    /// 批量添加：在单次加锁内完成所有插入，最后统一持久化一次
    pub fn batch_add(&self, urls: Vec<String>) -> (Vec<ProxyEntry>, Vec<String>) {
        let mut added = vec![];
        let mut errors = vec![];

        let mut entries = self.entries.lock();
        for url in urls {
            let url = url.trim().to_string();
            if url.is_empty() || url.starts_with('#') {
                continue;
            }
            if let Err(e) = validate_proxy_url(&url) {
                errors.push(e.to_string());
                continue;
            }
            if entries.iter().any(|e| e.url == url) {
                errors.push(format!("代理 URL 已存在: {}", url));
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
    pub fn assignable_urls(&self) -> Vec<String> {
        self.entries
            .lock()
            .iter()
            .filter(|e| e.enabled && e.health != ProxyHealth::Unhealthy)
            .map(|e| e.url.clone())
            .collect()
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
    pub fn report_proxy_failure(&self, credential_id: u64, proxy: &ProxyConfig) {
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

        if let Some(url) = disabled_url {
            self.clear_runtime_for_urls(&[url]);
        }
        if changed && let Err(e) = self.persist() {
            tracing::warn!("记录运行时代理失败后持久化失败: {}", e);
        }
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
            let ids: Vec<u64> = (0..*seen).map(|_| { let i = next_id; next_id += 1; i }).collect();
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
        assert_eq!(ordered.first().map(|p| p.url.as_str()), Some(proxy_b.url.as_str()));
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
}
