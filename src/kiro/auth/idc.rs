//! AWS SSO OIDC 设备授权登录流程
//!
//! 实现三步流程：
//! 1. 注册 OIDC 客户端（register_client）
//! 2. 发起设备授权，获取用户验证码（start_device_authorization）
//! 3. 轮询令牌端点，等待用户完成授权（poll_token）

use anyhow::Context;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::model::token_refresh::{
    CreateTokenRequest, CreateTokenResponse, OidcErrorResponse, RegisterClientRequest,
    RegisterClientResponse, StartDeviceAuthorizationRequest, StartDeviceAuthorizationResponse,
};
use crate::model::config::Config;

/// 设备授权轮询结果
#[derive(Debug)]
pub enum PollResult {
    /// 用户尚未完成授权，继续等待
    Pending,
    /// 授权成功，返回 token
    Success(CreateTokenResponse),
    /// 设备码已过期，需重新发起
    Expired,
    /// 其他错误
    Error(anyhow::Error),
}

/// AWS Builder ID / IAM Identity Center 的默认 Start URL
pub const BUILDER_ID_START_URL: &str = "https://view.awsapps.com/start";

/// 仅 IPv4 的 Access Portal 别名 → 双栈门户。
///
/// `https://<alias>.awsapps.com/start` 能完成 SSO，但 Kiro 数据面会把随后的
/// bearer token 判成 invalid。必须改用 `https://<dir>.portal.<region>.app.aws`。
const IPV4_PORTAL_ALIASES: &[(&str, &str)] = &[
    (
        "jarvisclaw.awsapps.com",
        "https://ssoins-821071a5e59b0789.portal.ap-southeast-1.app.aws",
    ),
];

/// 规范化后的 IdC issuer：真正拿去注册 OIDC 的 Start URL + 刷新区。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedIdcIssuer {
    pub start_url: String,
    pub auth_region: String,
    pub rewritten_from_ipv4: bool,
}

/// 把登录框里的 Start URL / 区域收成 OIDC 真正该用的值。
///
/// - 双栈门户 `*.portal.<region>.app.aws`：区域以主机名为准（避免 UI 默认 us-east-1）
/// - 已知的仅 IPv4 `*.awsapps.com`：改写成双栈
/// - Builder ID / 其它 URL：原样保留，区域用调用方填的
pub fn normalize_idc_issuer(
    start_url: &str,
    requested_region: &str,
) -> Result<NormalizedIdcIssuer, String> {
    let requested = requested_region.trim();
    if requested.is_empty() {
        return Err("SSO 区域不能为空".to_string());
    }

    let trimmed = start_url.trim();
    let raw = if trimmed.is_empty() {
        BUILDER_ID_START_URL.to_string()
    } else if trimmed.contains("://") {
        trimmed.trim_end_matches('/').to_string()
    } else {
        format!("https://{}", trimmed.trim_end_matches('/'))
    };

    let host = host_from_url(&raw).ok_or_else(|| {
        format!("SSO Start URL 无效：{raw}。企业号请填双栈门户，例如 https://xxxx.portal.ap-southeast-1.app.aws")
    })?;

    if let Some(region) = auth_region_from_portal_host(&host) {
        return Ok(NormalizedIdcIssuer {
            start_url: canonicalize_https_host_url(&raw, &host),
            auth_region: region,
            rewritten_from_ipv4: false,
        });
    }

    if is_ipv4_access_portal(&host)
        && let Some(dual) = ipv4_alias_target(&host)
    {
        let dual_host = host_from_url(dual).unwrap_or_default();
        let region = auth_region_from_portal_host(&dual_host)
            .ok_or_else(|| format!("内部门户别名缺少区域: {dual}"))?;
        return Ok(NormalizedIdcIssuer {
            start_url: dual.trim_end_matches('/').to_string(),
            auth_region: region,
            rewritten_from_ipv4: true,
        });
    }

    Ok(NormalizedIdcIssuer {
        start_url: raw,
        auth_region: requested.to_string(),
        rewritten_from_ipv4: false,
    })
}

fn host_from_url(raw: &str) -> Option<String> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?.trim();
    if host.is_empty() || host.contains('@') {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

fn canonicalize_https_host_url(raw: &str, host: &str) -> String {
    let after_host = raw
        .split_once("://")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once('/').map(|(_, path)| path))
        .unwrap_or("");
    let path = after_host
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    if path.is_empty() {
        format!("https://{host}")
    } else {
        format!("https://{host}/{path}")
    }
}

fn is_ipv4_access_portal(host: &str) -> bool {
    host.ends_with(".awsapps.com") && host != "view.awsapps.com"
}

fn ipv4_alias_target(host: &str) -> Option<&'static str> {
    IPV4_PORTAL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == host)
        .map(|(_, dual)| *dual)
}

/// `ssoins-xxx.portal.ap-southeast-1.app.aws` → `ap-southeast-1`
pub fn auth_region_from_portal_host(host: &str) -> Option<String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let rest = host.strip_suffix(".app.aws")?;
    let (_, region) = rest.rsplit_once(".portal.")?;
    if region.is_empty() || !region.contains('-') {
        return None;
    }
    Some(region.to_string())
}


/// Kiro IDE 使用的 OIDC 作用域
const KIRO_SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];

fn oidc_endpoint(region: &str) -> String {
    format!("https://oidc.{}.amazonaws.com", region)
}

/// 注册 OIDC 客户端
///
/// 每次发起设备授权前调用，获得 clientId 和 clientSecret。
/// 注册结果有过期时间（通常数天），但此处每次重新注册以保持简单。
/// `start_url` 作为 issuerUrl 一并提交：Builder ID 为默认 Start URL，
/// 企业 IAM Identity Center 为组织自己的 Start URL。
pub async fn register_client(
    region: &str,
    start_url: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<RegisterClientResponse> {
    let url = format!("{}/client/register", oidc_endpoint(region));
    let client = build_client(proxy, 30, config.tls_backend)?;

    let body = RegisterClientRequest {
        client_name: "kiro-rs".to_string(),
        client_type: "public".to_string(),
        scopes: KIRO_SCOPES.iter().map(|s| s.to_string()).collect(),
        grant_types: vec![
            "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            "refresh_token".to_string(),
        ],
        issuer_url: start_url.to_string(),
    };

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .json(&body)
        .send()
        .await
        .context("注册 OIDC 客户端请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("注册 OIDC 客户端失败 {}: {}", status, body_text);
    }

    resp.json::<RegisterClientResponse>()
        .await
        .context("解析注册响应失败")
}

/// 发起设备授权，返回供用户访问的验证码和 URL
pub async fn start_device_authorization(
    region: &str,
    start_url: &str,
    client_id: &str,
    client_secret: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<StartDeviceAuthorizationResponse> {
    let url = format!("{}/device_authorization", oidc_endpoint(region));
    let client = build_client(proxy, 30, config.tls_backend)?;

    let body = StartDeviceAuthorizationRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        start_url: start_url.to_string(),
    };

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .json(&body)
        .send()
        .await
        .context("发起设备授权请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("发起设备授权失败 {}: {}", status, body_text);
    }

    resp.json::<StartDeviceAuthorizationResponse>()
        .await
        .context("解析设备授权响应失败")
}

/// 轮询一次令牌端点
///
/// 返回 `PollResult`，由调用方决定是否继续轮询。
pub async fn poll_token(
    region: &str,
    client_id: &str,
    client_secret: &str,
    device_code: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> PollResult {
    let url = format!("{}/token", oidc_endpoint(region));
    let client = match build_client(proxy, 30, config.tls_backend) {
        Ok(c) => c,
        Err(e) => return PollResult::Error(e),
    };

    let body = CreateTokenRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        grant_type: "urn:ietf:params:oauth:grant-type:device_code".to_string(),
        device_code: device_code.to_string(),
    };

    let resp = match client
        .post(&url)
        .header("content-type", "application/json")
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return PollResult::Error(e.into()),
    };

    let status = resp.status();

    if status.is_success() {
        return match resp.json::<CreateTokenResponse>().await {
            Ok(token) => PollResult::Success(token),
            Err(e) => PollResult::Error(e.into()),
        };
    }

    let body_text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return PollResult::Error(e.into()),
    };

    // 解析标准 OIDC 错误码
    if let Ok(err_resp) = serde_json::from_str::<OidcErrorResponse>(&body_text) {
        match err_resp.error.as_str() {
            "authorization_pending" => return PollResult::Pending,
            "slow_down" => return PollResult::Pending,
            "expired_token" => return PollResult::Expired,
            "access_denied" => return PollResult::Error(anyhow::anyhow!("用户拒绝了授权请求")),
            _ => {}
        }
    }

    PollResult::Error(anyhow::anyhow!("轮询令牌失败 {}: {}", status, body_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_stack_portal_overrides_ui_default_region() {
        let issuer = normalize_idc_issuer(
            "https://ssoins-821071a5e59b0789.portal.ap-southeast-1.app.aws/",
            "us-east-1",
        )
        .unwrap();
        assert_eq!(
            issuer.start_url,
            "https://ssoins-821071a5e59b0789.portal.ap-southeast-1.app.aws"
        );
        assert_eq!(issuer.auth_region, "ap-southeast-1");
        assert!(!issuer.rewritten_from_ipv4);
    }

    #[test]
    fn known_ipv4_alias_rewrites_to_dual_stack() {
        let issuer = normalize_idc_issuer("https://jarvisclaw.awsapps.com/start", "us-east-1")
            .unwrap();
        assert_eq!(
            issuer.start_url,
            "https://ssoins-821071a5e59b0789.portal.ap-southeast-1.app.aws"
        );
        assert_eq!(issuer.auth_region, "ap-southeast-1");
        assert!(issuer.rewritten_from_ipv4);
    }

    #[test]
    fn builder_id_keeps_requested_region() {
        let issuer = normalize_idc_issuer("", "us-east-1").unwrap();
        assert_eq!(issuer.start_url, BUILDER_ID_START_URL);
        assert_eq!(issuer.auth_region, "us-east-1");
        assert!(!issuer.rewritten_from_ipv4);
    }

    #[test]
    fn unknown_awsapps_alias_stays_but_uses_requested_region() {
        let issuer = normalize_idc_issuer("https://other-org.awsapps.com/start", "eu-central-1")
            .unwrap();
        assert_eq!(issuer.start_url, "https://other-org.awsapps.com/start");
        assert_eq!(issuer.auth_region, "eu-central-1");
        assert!(!issuer.rewritten_from_ipv4);
    }

    #[test]
    fn empty_region_is_rejected() {
        assert!(normalize_idc_issuer("https://view.awsapps.com/start", "  ").is_err());
    }
}
