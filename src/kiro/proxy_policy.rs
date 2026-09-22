use crate::http_client::ProxyConfig;
use crate::kiro::model::credentials::KiroCredentials;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyPurpose {
    Refresh,
    Profile,
    Usage,
    Login,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProxyDecision {
    UseCredentialProxy,
    UseGlobal,
    MissingExclusiveProxy,
}

pub fn decide_auxiliary_proxy(
    credentials: &KiroCredentials,
    _purpose: ProxyPurpose,
    global: Option<&ProxyConfig>,
) -> anyhow::Result<(ProxyDecision, Option<ProxyConfig>)> {
    if credentials.is_enterprise_credential() {
        return Ok((ProxyDecision::UseGlobal, global.cloned()));
    }

    let Some(raw) = credentials.proxy_url.as_deref() else {
        return Ok((ProxyDecision::MissingExclusiveProxy, None));
    };
    let proxy = ProxyConfig::split_candidates(raw)
        .into_iter()
        .find(|candidate| {
            ProxyConfig::is_supported_entry(candidate) && !ProxyConfig::is_direct(candidate)
        })
        .and_then(|candidate| {
            ProxyConfig::from_url_with_auth(
                candidate,
                credentials.proxy_username.as_deref(),
                credentials.proxy_password.as_deref(),
            )
        });

    match proxy {
        Some(proxy) => Ok((ProxyDecision::UseCredentialProxy, Some(proxy))),
        None => Ok((ProxyDecision::MissingExclusiveProxy, None)),
    }
}

pub fn require_auxiliary_proxy(
    credentials: &KiroCredentials,
    purpose: ProxyPurpose,
    global: Option<&ProxyConfig>,
) -> anyhow::Result<Option<ProxyConfig>> {
    let (decision, proxy) = decide_auxiliary_proxy(credentials, purpose, global)?;
    if decision == ProxyDecision::MissingExclusiveProxy {
        anyhow::bail!("MissingExclusiveProxy");
    }
    Ok(proxy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_without_proxy_fails_closed_even_when_global_exists() {
        let credentials = KiroCredentials {
            auth_method: Some("social".into()),
            ..Default::default()
        };
        let global = ProxyConfig::new("http://server:8080");

        let result = decide_auxiliary_proxy(&credentials, ProxyPurpose::Refresh, Some(&global));

        assert_eq!(
            result.unwrap().0,
            ProxyDecision::MissingExclusiveProxy
        );
    }

    #[test]
    fn enterprise_without_credential_proxy_uses_global() {
        let credentials = KiroCredentials {
            auth_method: Some("idc".into()),
            start_url: Some("https://sso.example/start".into()),
            ..Default::default()
        };
        let global = ProxyConfig::new("http://server:8080");

        let result = decide_auxiliary_proxy(&credentials, ProxyPurpose::Usage, Some(&global));

        assert_eq!(result.unwrap(), (ProxyDecision::UseGlobal, Some(global)));
    }

    #[test]
    fn personal_with_proxy_uses_credential_proxy() {
        let credentials = KiroCredentials {
            auth_method: Some("social".into()),
            proxy_url: Some("http://personal:8080".into()),
            ..Default::default()
        };

        let result = decide_auxiliary_proxy(&credentials, ProxyPurpose::Profile, None);

        assert_eq!(
            result.unwrap(),
            (
                ProxyDecision::UseCredentialProxy,
                Some(ProxyConfig::new("http://personal:8080"))
            )
        );
    }
}
