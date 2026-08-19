//! 出口失效后把活号改绑到健康 IP。
//!
//! 号的 `proxyUrl` 是钉死的单候选。出口被自动禁用或不健康后，候选列表会被滤空；
//! 直连又被禁止，请求就会 0ms 报「没有可用代理候选」。这里只做一件事：换成负载
//! 最低的可分配出口，绝不退回直连。

use std::collections::HashMap;

use crate::admin::proxy_ban_stats::normalize_proxy_key;
use crate::admin::proxy_pool::ProxyPoolManager;
use crate::kiro::token_manager::MultiTokenManager;

/// 每个出口当前绑了多少个还活着的号。
pub fn live_proxy_loads(token_manager: &MultiTokenManager) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for credential in token_manager.snapshot().entries {
        if credential.died_at.is_some() {
            continue;
        }
        if let Some(url) = credential.proxy_url.as_deref() {
            *counts.entry(normalize_proxy_key(Some(url))).or_default() += 1;
        }
    }
    counts
}

/// 当前凭据若已绑在可分配出口上，原样返回；否则改绑到负载最低的健康出口。
///
/// `stale_url` 是这次请求快照里的绑定，可能已经过期。先看磁盘上的活绑定，
/// 避免刚被批量迁走过的号又被改绑第二次。
pub fn rebind_credential_to_healthy_proxy(
    token_manager: &MultiTokenManager,
    pool: &ProxyPoolManager,
    credential_id: u64,
    stale_url: Option<&str>,
) -> Option<String> {
    let live = token_manager
        .snapshot()
        .entries
        .into_iter()
        .find(|entry| entry.id == credential_id)?;
    if live.died_at.is_some() {
        return None;
    }
    let current = live.proxy_url.as_deref().or(stale_url);
    if let Some(url) = current
        && pool.assignable_urls().iter().any(|candidate| candidate == url)
    {
        return Some(url.to_string());
    }

    let mut loads = live_proxy_loads(token_manager);
    if let Some(url) = current {
        let key = normalize_proxy_key(Some(url));
        if let Some(count) = loads.get_mut(&key) {
            *count = count.saturating_sub(1);
        }
    }
    let new_url = pool.pick_replacement_url(current, &loads)?;
    token_manager
        .update_credential(
            credential_id,
            None,
            None,
            Some(Some(new_url.clone())),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .ok()?;
    tracing::warn!(
        credential_id,
        email = live.email.as_deref().unwrap_or("-"),
        from = %crate::admin::proxy_ban_stats::redact_proxy_url(current.unwrap_or("")),
        to = %crate::admin::proxy_ban_stats::redact_proxy_url(&new_url),
        "绑定出口不可用，账号已改绑到健康出口"
    );
    Some(new_url)
}

/// 把某个失效出口上还活着的号全部迁走。死号不动。
pub fn migrate_live_off_proxy(
    token_manager: &MultiTokenManager,
    pool: &ProxyPoolManager,
    from_url: &str,
) -> usize {
    let from_key = normalize_proxy_key(Some(from_url));
    let survivors: Vec<u64> = token_manager
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
        .map(|entry| entry.id)
        .collect();

    let mut migrated = 0usize;
    for credential_id in survivors {
        if rebind_credential_to_healthy_proxy(token_manager, pool, credential_id, Some(from_url))
            .is_some_and(|url| normalize_proxy_key(Some(&url)) != from_key)
        {
            migrated += 1;
        }
    }
    migrated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::proxy_pool::ProxyPoolManager;
    use crate::http_client::ProxyConfig;
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::kiro::token_manager::MultiTokenManager;
    use crate::model::config::{Config, TlsBackend};
    use std::sync::Arc;

    fn cred(id: u64, email: &str, proxy: &str) -> KiroCredentials {
        KiroCredentials {
            id: Some(id),
            email: Some(email.to_string()),
            proxy_url: Some(proxy.to_string()),
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

    fn bound_proxy(manager: &MultiTokenManager, id: u64) -> Option<String> {
        manager
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.proxy_url)
    }

    #[test]
    fn rebind_moves_account_off_disabled_proxy() {
        let (manager, pool) = setup(vec![cred(1, "a@x.com", "http://dead:8080")]);
        let dead = pool.add("http://dead:8080".into(), None).unwrap();
        pool.add("http://ok:8080".into(), None).unwrap();
        pool.set_enabled(dead.id, false).unwrap();

        let new_url =
            rebind_credential_to_healthy_proxy(&manager, &pool, 1, Some("http://dead:8080"))
                .unwrap();
        assert_eq!(new_url, "http://ok:8080");
        assert_eq!(bound_proxy(&manager, 1).as_deref(), Some("http://ok:8080"));
    }

    #[test]
    fn rebind_does_not_invent_direct_when_pool_is_empty() {
        let (manager, pool) = setup(vec![cred(1, "a@x.com", "http://dead:8080")]);
        let dead = pool.add("http://dead:8080".into(), None).unwrap();
        pool.set_enabled(dead.id, false).unwrap();

        assert!(
            rebind_credential_to_healthy_proxy(&manager, &pool, 1, Some("http://dead:8080"))
                .is_none()
        );
        assert_eq!(bound_proxy(&manager, 1).as_deref(), Some("http://dead:8080"));
    }

    #[test]
    fn migrate_moves_live_accounts_and_leaves_dead_ones() {
        let (manager, pool) = setup(vec![
            cred(1, "live@x.com", "http://dead:8080"),
            cred(2, "dead@x.com", "http://dead:8080"),
            cred(3, "other@x.com", "http://ok:8080"),
        ]);
        let dead = pool.add("http://dead:8080".into(), None).unwrap();
        pool.add("http://ok:8080".into(), None).unwrap();
        pool.set_enabled(dead.id, false).unwrap();
        manager
            .mark_credential_dead(2, Some("http://dead:8080"), Some("test"))
            .unwrap();

        let moved = migrate_live_off_proxy(&manager, &pool, "http://dead:8080");
        assert_eq!(moved, 1);
        assert_eq!(bound_proxy(&manager, 1).as_deref(), Some("http://ok:8080"));
        assert_eq!(bound_proxy(&manager, 2).as_deref(), Some("http://dead:8080"));
        assert_eq!(bound_proxy(&manager, 3).as_deref(), Some("http://ok:8080"));
    }

    #[test]
    fn three_runtime_failures_disable_and_can_migrate() {
        let (manager, pool) = setup(vec![cred(1, "a@x.com", "http://flaky:8080")]);
        pool.add("http://flaky:8080".into(), None).unwrap();
        pool.add("http://ok:8080".into(), None).unwrap();
        let proxy = ProxyConfig::new("http://flaky:8080");
        assert!(pool.report_proxy_failure(1, &proxy).is_none());
        assert!(pool.report_proxy_failure(1, &proxy).is_none());
        assert_eq!(
            pool.report_proxy_failure(1, &proxy).as_deref(),
            Some("http://flaky:8080")
        );
        assert_eq!(migrate_live_off_proxy(&manager, &pool, "http://flaky:8080"), 1);
        assert_eq!(bound_proxy(&manager, 1).as_deref(), Some("http://ok:8080"));
    }
}
