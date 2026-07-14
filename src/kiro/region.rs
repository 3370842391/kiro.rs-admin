//! Kiro 数据面区域与主机解析。
//!
//! API Key 只允许显式选择 Kiro 已开放的两个数据面区域；OAuth/SSO 则保留
//! profile ARN 优先和双区兼容回退语义。

use anyhow::bail;

use crate::kiro::model::credentials::{KiroCredentials, region_from_profile_arn};
use crate::model::config::Config;

pub const API_KEY_AUTH_REGION: &str = "us-east-1";
pub const US_EAST_1: &str = "us-east-1";
pub const EU_CENTRAL_1: &str = "eu-central-1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KiroService {
    Rest,
    Ide,
    CodeWhisperer,
    AmazonQ,
    Runtime,
}

pub fn validate_api_region(region: &str) -> anyhow::Result<&str> {
    match region.trim() {
        US_EAST_1 => Ok(US_EAST_1),
        EU_CENTRAL_1 => Ok(EU_CENTRAL_1),
        "" => bail!("API Key 凭据缺少必填字段 apiRegion"),
        other => bail!("不支持的 apiRegion: {other}；仅允许 {US_EAST_1} 或 {EU_CENTRAL_1}"),
    }
}

pub fn data_plane_host(service: KiroService, api_region: &str) -> anyhow::Result<String> {
    let region = api_region.trim();
    if region.is_empty() {
        bail!("数据面区域不能为空");
    }
    if service == KiroService::Rest {
        validate_api_region(region)?;
    }
    let host = match service {
        KiroService::Rest | KiroService::CodeWhisperer if region == US_EAST_1 => {
            "codewhisperer.us-east-1.amazonaws.com".to_string()
        }
        KiroService::Rest
        | KiroService::CodeWhisperer
        | KiroService::Ide
        | KiroService::AmazonQ => format!("q.{region}.amazonaws.com"),
        KiroService::Runtime => format!("runtime.{region}.kiro.dev"),
    };
    Ok(host)
}

pub(crate) fn compatibility_region_candidates(preferred: &str) -> [&'static str; 2] {
    let primary_eu = preferred == EU_CENTRAL_1 || preferred.starts_with("eu-");
    if primary_eu {
        [EU_CENTRAL_1, US_EAST_1]
    } else {
        [US_EAST_1, EU_CENTRAL_1]
    }
}

pub fn rest_region_candidates(
    credentials: &KiroCredentials,
    config: &Config,
) -> anyhow::Result<Vec<String>> {
    if credentials.is_api_key_credential() {
        let region = credentials
            .api_region
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少必填字段 apiRegion"))?;
        return Ok(vec![validate_api_region(region)?.to_string()]);
    }

    let arn_region = credentials
        .effective_profile_arn()
        .and_then(region_from_profile_arn);
    let preferred = arn_region
        .as_deref()
        .or(credentials.api_region.as_deref())
        .unwrap_or_else(|| credentials.effective_auth_region(config));
    Ok(compatibility_region_candidates(preferred)
        .into_iter()
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;

    #[test]
    fn supported_api_regions_are_strict() {
        assert_eq!(validate_api_region("us-east-1").unwrap(), "us-east-1");
        assert_eq!(validate_api_region("eu-central-1").unwrap(), "eu-central-1");
        assert!(validate_api_region("").is_err());
        assert!(validate_api_region("ap-southeast-1").is_err());
    }

    #[test]
    fn host_matrix_never_builds_an_eu_codewhisperer_domain() {
        let cases = [
            (
                KiroService::Rest,
                "us-east-1",
                "codewhisperer.us-east-1.amazonaws.com",
            ),
            (
                KiroService::Rest,
                "eu-central-1",
                "q.eu-central-1.amazonaws.com",
            ),
            (
                KiroService::CodeWhisperer,
                "eu-central-1",
                "q.eu-central-1.amazonaws.com",
            ),
            (KiroService::Ide, "us-east-1", "q.us-east-1.amazonaws.com"),
            (
                KiroService::AmazonQ,
                "eu-central-1",
                "q.eu-central-1.amazonaws.com",
            ),
            (
                KiroService::Runtime,
                "eu-central-1",
                "runtime.eu-central-1.kiro.dev",
            ),
        ];

        for (service, region, expected) in cases {
            assert_eq!(data_plane_host(service, region).unwrap(), expected);
        }
    }

    #[test]
    fn api_key_requires_one_explicit_api_region_without_global_fallback() {
        let mut credentials = KiroCredentials {
            auth_method: Some("api_key".to_string()),
            auth_region: Some("eu-central-1".to_string()),
            ..Default::default()
        };
        let mut config = Config::default();
        config.api_region = Some("us-east-1".to_string());

        assert!(rest_region_candidates(&credentials, &config).is_err());

        credentials.api_region = Some("eu-central-1".to_string());
        assert_eq!(
            rest_region_candidates(&credentials, &config).unwrap(),
            vec!["eu-central-1"]
        );
    }

    #[test]
    fn oauth_keeps_profile_arn_then_explicit_region_then_compatibility_fallback() {
        let mut credentials = KiroCredentials::default();
        credentials.profile_arn =
            Some("arn:aws:codewhisperer:eu-central-1:123456789012:profile/UNIT".to_string());
        credentials.api_region = Some("us-east-1".to_string());
        let config = Config::default();

        assert_eq!(
            rest_region_candidates(&credentials, &config).unwrap(),
            vec!["eu-central-1", "us-east-1"]
        );

        credentials.profile_arn = None;
        assert_eq!(
            rest_region_candidates(&credentials, &config).unwrap(),
            vec!["us-east-1", "eu-central-1"]
        );
    }
}
