//! 个人号独占出口：一张号一个 IP，不够就禁用，分到再启用。
//!
//! 多个个人号挤同一出口会被上游连杀。企业号不参与这套互斥。
//! 个人号 `proxyUrl` 只保留一个候选，禁止逗号多 IP 故障转移。

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::admin::proxy_ban_stats::normalize_proxy_key;
use crate::admin::proxy_pool::ProxyPoolManager;
use crate::http_client::ProxyConfig;
use crate::kiro::model::credentials::CredentialDisableReason;
use crate::kiro::token_manager::{
    ExclusivePersonalAccount, ExclusiveProxyPatch, MultiTokenManager,
};

/// 保护“读取占用 → 选择空闲 IP → 一次性落盘”的事务。
///
/// 分配入口既有同步的 Admin handler，也有登录回调和运行时迁移；仅靠凭据快照
/// 会让两个同时到达的请求都看到同一个空闲 IP。锁只覆盖本地计算和落盘，不包住
/// 网络请求，因此不会把上游调用串行化。
static EXCLUSIVE_ALLOCATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn allocation_lock() -> &'static Mutex<()> {
    EXCLUSIVE_ALLOCATION_LOCK.get_or_init(Mutex::default)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExclusiveAssignResult {
    pub assigned: usize,
    pub enabled: usize,
    pub disabled: usize,
    pub stripped_extra: usize,
    pub skipped_enterprise: usize,
    pub proxy_count: usize,
    pub unassigned: usize,
}

fn first_proxy_url(raw: Option<&str>) -> Option<String> {
    let raw = raw.map(str::trim).filter(|value| !value.is_empty())?;
    ProxyConfig::split_candidates(raw)
        .into_iter()
        .find(|candidate| ProxyConfig::is_supported_entry(candidate) && !ProxyConfig::is_direct(candidate))
        .map(|candidate| candidate.to_string())
}

fn is_terminal(reason: Option<CredentialDisableReason>) -> bool {
    reason.is_some_and(CredentialDisableReason::is_terminal)
}

fn occupancy(accounts: &[ExclusivePersonalAccount]) -> HashMap<String, Vec<u64>> {
    let mut used: HashMap<String, Vec<u64>> = HashMap::new();
    for account in accounts {
        if let Some(url) = first_proxy_url(account.proxy_url.as_deref()) {
            used.entry(normalize_proxy_key(Some(&url)))
                .or_default()
                .push(account.id);
        }
    }
    used
}

fn manual_binding(account: &ExclusivePersonalAccount, assignable: &[String]) -> bool {
    account.proxy_manual_binding && account.proxy_url.as_deref().is_some_and(|url| {
        first_proxy_url(Some(url)).is_some_and(|first| assignable.iter().any(|candidate| candidate == &first))
    })
}

/// 给个人号互斥分配可用出口。已占用的独占绑定保留；共享出口只留一张号。
/// IP 不够的个人号写成 `MissingExclusiveProxy` 并禁用；分到的自动启用。
pub fn assign_exclusive_personal_proxies(
    token_manager: &MultiTokenManager,
    pool: &ProxyPoolManager,
    only_ids: Option<&HashSet<u64>>,
) -> ExclusiveAssignResult {
    let _guard = allocation_lock().lock();
    let ranked = pool.assignable_urls_ranked();
    let (accounts, skipped_enterprise) = token_manager.exclusive_personal_accounts();
    let mut used = occupancy(&accounts);
    for ids in used.values_mut() {
        ids.sort_unstable();
    }
    let targets: Vec<ExclusivePersonalAccount> = accounts
        .into_iter()
        .filter(|account| only_ids.is_none_or(|ids| ids.contains(&account.id)))
        .collect();
    let occupied: HashSet<String> = used.keys().cloned().collect();
    let mut free: Vec<String> = ranked
        .iter()
        .filter(|url| !occupied.contains(&normalize_proxy_key(Some(url.as_str()))))
        .cloned()
        .collect();

    let mut patches = Vec::new();
    let mut assigned = 0usize;
    let mut enabled = 0usize;
    let mut disabled = 0usize;
    let mut stripped_extra = 0usize;

    for account in &targets {
        let first = first_proxy_url(account.proxy_url.as_deref());
        if account.proxy_manual_binding
            && !manual_binding(account, &ranked)
        {
            patches.push(ExclusiveProxyPatch {
                id: account.id,
                proxy_url: Some(None),
                disable: Some(true),
            });
            disabled += 1;
            continue;
        }
        if manual_binding(account, &ranked) {
            continue;
        }
        let extras = account
            .proxy_url
            .as_deref()
            .is_some_and(|raw| ProxyConfig::split_candidates(raw).len() > 1);
        let key = first
            .as_deref()
            .map(|url| normalize_proxy_key(Some(url)));
        let exclusive_keeper = key
            .as_ref()
            .and_then(|k| used.get(k))
            .and_then(|ids| ids.first().copied())
            == Some(account.id);

        if exclusive_keeper {
            if extras {
                if let Some(url) = first.clone() {
                    patches.push(ExclusiveProxyPatch {
                        id: account.id,
                        proxy_url: Some(Some(url)),
                        disable: if account.disable_reason
                            == Some(CredentialDisableReason::MissingExclusiveProxy)
                        {
                            Some(false)
                        } else {
                            None
                        },
                    });
                    stripped_extra += 1;
                    if account.disable_reason
                        == Some(CredentialDisableReason::MissingExclusiveProxy)
                    {
                        enabled += 1;
                    }
                }
            } else if account.disable_reason
                == Some(CredentialDisableReason::MissingExclusiveProxy)
            {
                patches.push(ExclusiveProxyPatch {
                    id: account.id,
                    proxy_url: None,
                    disable: Some(false),
                });
                enabled += 1;
            }
            continue;
        }

        if let Some(url) = free.first().cloned() {
            let key = normalize_proxy_key(Some(&url));
            used.entry(key).or_default().push(account.id);
            free.remove(0);
            let should_enable = account.disabled
                && (account.disable_reason
                    == Some(CredentialDisableReason::MissingExclusiveProxy)
                    || account.disable_reason.is_none());
            patches.push(ExclusiveProxyPatch {
                id: account.id,
                proxy_url: Some(Some(url)),
                disable: if should_enable { Some(false) } else { None },
            });
            assigned += 1;
            if should_enable {
                enabled += 1;
            }
            continue;
        }

        if !is_terminal(account.disable_reason) {
            let overwrite_reason = !account.disabled
                || account.disable_reason.is_none()
                || account.disable_reason
                    == Some(CredentialDisableReason::MissingExclusiveProxy);
            patches.push(ExclusiveProxyPatch {
                id: account.id,
                proxy_url: Some(None),
                disable: if overwrite_reason { Some(true) } else { None },
            });
            if overwrite_reason {
                disabled += 1;
            }
        }
    }

    if let Err(error) = token_manager.apply_exclusive_proxy_patches(&patches) {
        tracing::error!(%error, "个人号独占出口落盘失败");
    }

    ExclusiveAssignResult {
        assigned,
        enabled,
        disabled,
        stripped_extra,
        skipped_enterprise,
        proxy_count: ranked.len(),
        unassigned: disabled,
    }
}

/// 个人号失效出口：只改绑到没有其他个人号的 IP；没有空位就禁用，绝不挤过去。
pub fn rebind_personal_exclusive(
    token_manager: &MultiTokenManager,
    pool: &ProxyPoolManager,
    credential_id: u64,
    stale_url: Option<&str>,
) -> Option<String> {
    if token_manager.is_enterprise_credential(credential_id) {
        return crate::admin::proxy_rebind::rebind_credential_to_healthy_proxy(
            token_manager,
            pool,
            credential_id,
            stale_url,
        );
    }
    let _guard = allocation_lock().lock();
    let live = token_manager
        .exclusive_personal_accounts()
        .0
        .into_iter()
        .find(|entry| entry.id == credential_id)?;

    let current = first_proxy_url(live.proxy_url.as_deref()).or_else(|| {
        stale_url.and_then(|url| first_proxy_url(Some(url)))
    });
    if live.proxy_manual_binding
        && manual_binding(&live, &pool.assignable_urls())
        && current.as_deref().is_some_and(|url| {
            pool.assignable_urls().iter().any(|candidate| candidate == url)
        })
    {
        return current;
    }
    if let Some(url) = current.as_deref()
        && pool.assignable_urls().iter().any(|candidate| candidate == url)
        && token_manager
            .exclusive_personal_accounts()
            .0
            .iter()
            .filter(|account| account.id != credential_id)
            .filter_map(|account| first_proxy_url(account.proxy_url.as_deref()))
            .all(|other| normalize_proxy_key(Some(&other)) != normalize_proxy_key(Some(url)))
    {
        return Some(url.to_string());
    }

    let mut loads: HashMap<String, usize> = HashMap::new();
    for account in token_manager.exclusive_personal_accounts().0 {
        if account.id == credential_id {
            continue;
        }
        if let Some(url) = first_proxy_url(account.proxy_url.as_deref()) {
            *loads
                .entry(normalize_proxy_key(Some(&url)))
                .or_default() += 1;
        }
    }
    let new_url = pool
        .pick_replacement_url(current.as_deref(), &loads)
        .filter(|url| loads.get(&normalize_proxy_key(Some(url))).copied().unwrap_or(0) == 0);

    if let Some(new_url) = new_url {
        let _ = token_manager.apply_exclusive_proxy_patches(&[ExclusiveProxyPatch {
            id: credential_id,
            proxy_url: Some(Some(new_url.clone())),
            disable: if live.disable_reason
                == Some(CredentialDisableReason::MissingExclusiveProxy)
            {
                Some(false)
            } else {
                None
            },
        }]);
        tracing::warn!(
            credential_id,
            from = %crate::admin::proxy_ban_stats::redact_proxy_url(current.as_deref().unwrap_or("")),
            to = %crate::admin::proxy_ban_stats::redact_proxy_url(&new_url),
            "个人号出口不可用，已改绑到空闲独立 IP"
        );
        return Some(new_url);
    }

    if !is_terminal(live.disable_reason) {
        let _ = token_manager.apply_exclusive_proxy_patches(&[ExclusiveProxyPatch {
            id: credential_id,
            proxy_url: Some(None),
            disable: Some(true),
        }]);
        tracing::warn!(
            credential_id,
            "个人号出口不可用且没有空闲独立 IP，已禁用等待分配"
        );
    }
    None
}

/// 失效出口上的号：个人号走独占改绑，企业号仍可挤到健康出口。
pub fn migrate_live_off_proxy_exclusive(
    token_manager: &MultiTokenManager,
    pool: &ProxyPoolManager,
    from_url: &str,
) -> usize {
    let from_key = normalize_proxy_key(Some(from_url));
    let survivors: Vec<(u64, bool)> = token_manager
        .snapshot()
        .entries
        .into_iter()
        .filter(|entry| entry.died_at.is_none())
        .filter(|entry| {
            entry
                .proxy_url
                .as_deref()
                .is_some_and(|url| normalize_proxy_key(Some(url)) == from_key)
        })
        .map(|entry| (entry.id, token_manager.is_enterprise_credential(entry.id)))
        .collect();

    let mut migrated = 0usize;
    for (credential_id, enterprise) in survivors {
        let rebound = if enterprise {
            crate::admin::proxy_rebind::rebind_credential_to_healthy_proxy(
                token_manager,
                pool,
                credential_id,
                Some(from_url),
            )
        } else {
            rebind_personal_exclusive(token_manager, pool, credential_id, Some(from_url))
        };
        if rebound.is_some_and(|url| normalize_proxy_key(Some(&url)) != from_key) {
            migrated += 1;
        }
    }
    migrated
}

/// 登录/导入时给个人号挑一个还没被个人号占用的出口。没有空位就返回 None。
pub fn pick_free_personal_proxy(
    token_manager: &MultiTokenManager,
    pool: &ProxyPoolManager,
) -> Option<String> {
    pick_free_personal_proxy_with_reserved(token_manager, pool, &HashSet::new())
}

/// 登录会话尚未写入凭据表时，额外排除已经被其它待完成登录占用的出口。
pub fn pick_free_personal_proxy_with_reserved(
    token_manager: &MultiTokenManager,
    pool: &ProxyPoolManager,
    reserved: &HashSet<String>,
) -> Option<String> {
    let _guard = allocation_lock().lock();
    let used = occupancy(&token_manager.exclusive_personal_accounts().0);
    pool.assignable_urls_ranked()
        .into_iter()
        .find(|url| {
            let key = normalize_proxy_key(Some(url));
            !used.contains_key(&key) && !reserved.contains(&key)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::proxy_pool::ProxyPoolManager;
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::{Config, TlsBackend};
    use std::sync::Arc;

    fn personal(id: u64, proxy: Option<&str>) -> KiroCredentials {
        KiroCredentials {
            id: Some(id),
            email: Some(format!("p{id}@x.com")),
            auth_method: Some("social".to_string()),
            subscription_title: Some("FREE".to_string()),
            proxy_url: proxy.map(str::to_string),
            rpm_limit: 10,
            ..Default::default()
        }
    }

    fn enterprise(id: u64, proxy: Option<&str>) -> KiroCredentials {
        KiroCredentials {
            id: Some(id),
            email: Some(format!("e{id}@x.com")),
            auth_method: Some("idc".to_string()),
            subscription_title: Some("KIRO POWER".to_string()),
            start_url: Some("https://sso.example/".to_string()),
            proxy_url: proxy.map(str::to_string),
            rpm_limit: 10,
            ..Default::default()
        }
    }

    fn setup(credentials: Vec<KiroCredentials>) -> (Arc<MultiTokenManager>, ProxyPoolManager) {
        let manager = Arc::new(
            MultiTokenManager::new(Config::default(), credentials, None, None, true).unwrap(),
        );
        let pool = ProxyPoolManager::new(None, TlsBackend::Rustls);
        (manager, pool)
    }

    fn bound(manager: &MultiTokenManager, id: u64) -> Option<String> {
        manager
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.proxy_url)
    }

    fn disabled(manager: &MultiTokenManager, id: u64) -> bool {
        manager
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.disabled)
            .unwrap_or(false)
    }

    #[test]
    fn exclusive_assign_gives_each_personal_a_unique_ip_and_disables_overflow() {
        let (manager, pool) = setup(vec![
            personal(1, None),
            personal(2, None),
            personal(3, None),
            enterprise(9, Some("http://shared:8080")),
        ]);
        pool.add("http://a:8080".into(), None).unwrap();
        pool.add("http://b:8080".into(), None).unwrap();

        let result = assign_exclusive_personal_proxies(&manager, &pool, None);
        assert_eq!(result.assigned, 2);
        assert_eq!(result.disabled, 1);
        assert_eq!(result.skipped_enterprise, 1);
        let urls = [bound(&manager, 1), bound(&manager, 2), bound(&manager, 3)]
            .into_iter()
            .flatten()
            .collect::<HashSet<_>>();
        assert_eq!(urls.len(), 2);
        assert_eq!(bound(&manager, 9).as_deref(), Some("http://shared:8080"));
        assert_eq!(
            [disabled(&manager, 1), disabled(&manager, 2), disabled(&manager, 3)]
                .into_iter()
                .filter(|d| *d)
                .count(),
            1
        );
    }

    #[test]
    fn exclusive_assign_breaks_shared_ip_and_enables_when_new_ip_arrives() {
        let (manager, pool) = setup(vec![
            personal(1, Some("http://a:8080")),
            personal(2, Some("http://a:8080")),
        ]);
        pool.add("http://a:8080".into(), None).unwrap();

        let first = assign_exclusive_personal_proxies(&manager, &pool, None);
        assert_eq!(first.disabled, 1);
        assert_eq!(bound(&manager, 1).as_deref(), Some("http://a:8080"));
        assert!(disabled(&manager, 2));

        pool.add("http://b:8080".into(), None).unwrap();
        let second = assign_exclusive_personal_proxies(&manager, &pool, None);
        assert_eq!(second.assigned, 1);
        assert_eq!(second.enabled, 1);
        assert!(!disabled(&manager, 2));
        assert_ne!(bound(&manager, 1), bound(&manager, 2));
    }

    #[test]
    fn exclusive_strips_multi_ip_on_personal() {
        let (manager, pool) = setup(vec![personal(
            1,
            Some("http://a:8080, http://b:8080"),
        )]);
        pool.add("http://a:8080".into(), None).unwrap();
        pool.add("http://b:8080".into(), None).unwrap();
        let result = assign_exclusive_personal_proxies(&manager, &pool, None);
        assert_eq!(result.stripped_extra, 1);
        assert_eq!(bound(&manager, 1).as_deref(), Some("http://a:8080"));
    }

    #[test]
    fn personal_rebind_does_not_stack_on_occupied_ip() {
        let (manager, pool) = setup(vec![
            personal(1, Some("http://dead:8080")),
            personal(2, Some("http://ok:8080")),
        ]);
        let dead = pool.add("http://dead:8080".into(), None).unwrap();
        pool.add("http://ok:8080".into(), None).unwrap();
        pool.set_enabled(dead.id, false).unwrap();

        assert!(rebind_personal_exclusive(&manager, &pool, 1, Some("http://dead:8080")).is_none());
        assert!(disabled(&manager, 1));
        assert_eq!(bound(&manager, 2).as_deref(), Some("http://ok:8080"));
    }

    #[test]
    fn exclusive_only_ids_does_not_steal_occupied_ip() {
        let (manager, pool) = setup(vec![
            personal(1, Some("http://a:8080")),
            personal(2, None),
        ]);
        pool.add("http://a:8080".into(), None).unwrap();
        pool.add("http://b:8080".into(), None).unwrap();
        let only = HashSet::from([2u64]);
        let result = assign_exclusive_personal_proxies(&manager, &pool, Some(&only));
        assert_eq!(result.assigned, 1);
        assert_eq!(bound(&manager, 1).as_deref(), Some("http://a:8080"));
        assert_eq!(bound(&manager, 2).as_deref(), Some("http://b:8080"));
    }

    #[test]
    fn exclusive_does_not_overwrite_manual_disable() {
        let mut parked = personal(2, Some("http://a:8080"));
        parked.disabled = true;
        parked.disable_reason = Some(CredentialDisableReason::Manual);
        let (manager, pool) = setup(vec![personal(1, Some("http://a:8080")), parked]);
        pool.add("http://a:8080".into(), None).unwrap();
        let result = assign_exclusive_personal_proxies(&manager, &pool, None);
        assert_eq!(result.disabled, 0);
        assert!(disabled(&manager, 2));
        assert_eq!(bound(&manager, 2), None);
        assert_eq!(
            manager
                .snapshot()
                .entries
                .into_iter()
                .find(|entry| entry.id == 2)
                .and_then(|entry| entry.disabled_reason),
            Some(CredentialDisableReason::Manual.as_label().to_string())
        );
    }

    #[test]
    fn pick_free_skips_occupied_personal_ip() {
        let (manager, pool) = setup(vec![personal(1, Some("http://a:8080"))]);
        pool.add("http://a:8080".into(), None).unwrap();
        pool.add("http://b:8080".into(), None).unwrap();
        assert_eq!(
            pick_free_personal_proxy(&manager, &pool).as_deref(),
            Some("http://b:8080")
        );
    }

    #[test]
    fn pick_free_respects_pending_login_reservation() {
        let (manager, pool) = setup(vec![personal(1, Some("http://a:8080"))]);
        pool.add("http://a:8080".into(), None).unwrap();
        pool.add("http://b:8080".into(), None).unwrap();
        let reserved = HashSet::from([normalize_proxy_key(Some("http://b:8080"))]);

        assert_eq!(
            pick_free_personal_proxy_with_reserved(&manager, &pool, &reserved).as_deref(),
            None
        );
    }

    #[test]
    fn terminal_personal_account_does_not_hold_an_exclusive_ip() {
        let mut dead = personal(1, Some("http://a:8080"));
        dead.disabled = true;
        dead.disable_reason = Some(CredentialDisableReason::QuotaExceeded);
        let (manager, pool) = setup(vec![dead, personal(2, None)]);
        pool.add("http://a:8080".into(), None).unwrap();

        let result = assign_exclusive_personal_proxies(&manager, &pool, None);

        assert_eq!(result.assigned, 1);
        assert_eq!(bound(&manager, 1).as_deref(), Some("http://a:8080"));
        assert_eq!(bound(&manager, 2).as_deref(), Some("http://a:8080"));
    }

    #[test]
    fn manual_shared_binding_survives_reconciliation_and_reload() {
        let mut raw = serde_json::to_value(personal(1, Some("http://a:8080"))).unwrap();
        raw["proxyManualBinding"] = serde_json::json!(true);
        let manual = serde_json::from_value(raw).unwrap();
        let mut manual2 = personal(2, Some("http://a:8080"));
        manual2.proxy_manual_binding = true;
        let (manager, pool) = setup(vec![manual, manual2, personal(3, None)]);
        pool.add("http://a:8080".into(), None).unwrap();
        for _ in 0..2 {
            assign_exclusive_personal_proxies(&manager, &pool, None);
            assert_eq!(bound(&manager, 1).as_deref(), Some("http://a:8080"));
            assert_eq!(bound(&manager, 2).as_deref(), Some("http://a:8080"));
            assert!(disabled(&manager, 3), "自动分配不能挤入手动共享的出口");
        }
        assert_eq!(rebind_personal_exclusive(&manager, &pool, 1, None).as_deref(), Some("http://a:8080"));
        let json = serde_json::to_string(&manager.clone_all_credentials()).unwrap();
        let (reloaded, _) = setup(serde_json::from_str(&json).unwrap());
        assign_exclusive_personal_proxies(&reloaded, &pool, None);
        assert_eq!(bound(&reloaded, 1), bound(&reloaded, 2));
        assert!(!disabled(&reloaded, 2));
    }

    #[test]
    fn manual_binding_never_accepts_direct_or_disabled_proxy() {
        for proxy in ["direct", "http://a:8080"] {
            let mut raw = serde_json::to_value(personal(1, Some(proxy))).unwrap();
            raw["proxyManualBinding"] = serde_json::json!(true);
            let (manager, pool) = setup(vec![serde_json::from_value(raw).unwrap()]);
            let entry = pool.add("http://a:8080".into(), None).unwrap();
            pool.set_enabled(entry.id, false).unwrap();
            assign_exclusive_personal_proxies(&manager, &pool, None);
            let snapshot = manager.snapshot().entries.into_iter().find(|entry| entry.id == 1).unwrap();
            assert!(snapshot.disabled, "proxy={proxy} reason={:?} bound={:?}", snapshot.disabled_reason, snapshot.proxy_url);
            assert!(snapshot.proxy_url.is_none());
        }
    }
}
