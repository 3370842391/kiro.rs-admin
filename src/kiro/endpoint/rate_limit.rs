//! 普通 429 的端点选择与同端点重试决策。
//!
//! 单号协议（`ide` / `runtime` / 跟随默认）与全局 429 桶策略分开：
//! 前者决定首跳，后者决定 429 后换不换桶。

use crate::model::config::RateLimitBucketMode;

/// 出厂：同一张号、同一个协议最多试 3 次（对齐官方 `max=3`）。
pub const DEFAULT_SAME_ENDPOINT_ATTEMPTS: u32 = 3;

/// 凭据未钉端点时，用全局默认。空白与 `None` 都算未钉。
pub fn resolve_primary_endpoint<'a>(
    configured: Option<&'a str>,
    default_endpoint: &'a str,
) -> &'a str {
    configured
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(default_endpoint)
}

/// 企业号专项：429 后只在 ide / runtime 之间换桶，不进 amazonq / codewhisperer。
pub fn enterprise_ide_runtime_hop(primary: &str) -> Vec<String> {
    match primary.trim() {
        "ide" => vec!["runtime".to_string()],
        "runtime" => vec!["ide".to_string()],
        other => ["ide", "runtime"]
            .into_iter()
            .filter(|name| *name != other)
            .map(str::to_string)
            .collect(),
    }
}

/// 本轮 ide/runtime 对打完后，下一轮从对面端点起手。
pub fn flip_ide_runtime(current: &str) -> &'static str {
    if current.trim() == "runtime" {
        "ide"
    } else {
        "runtime"
    }
}

/// `hop` 才使用解析好的降级链；其它策略显式空链（含面板覆盖）。
pub fn apply_bucket_mode(mode: RateLimitBucketMode, hop_chain: Vec<String>) -> Vec<String> {
    match mode {
        RateLimitBucketMode::Hop => hop_chain,
        RateLimitBucketMode::SameEndpoint | RateLimitBucketMode::None => Vec::new(),
    }
}

/// 当前这次同端点尝试失败后，要不要还在这张号上再打同一协议。
///
/// `attempts_used` 含刚刚失败的那一次，从 1 起。账号级风控永不连打。
pub fn stay_on_same_endpoint(
    mode: RateLimitBucketMode,
    attempts_used: u32,
    max_attempts: u32,
    account_throttled: bool,
) -> bool {
    if account_throttled || max_attempts <= 1 {
        return false;
    }
    match mode {
        RateLimitBucketMode::SameEndpoint => attempts_used < max_attempts,
        RateLimitBucketMode::Hop | RateLimitBucketMode::None => false,
    }
}

/// 官方 `x-kiro-attempt`：`1;max=3`。
pub fn kiro_attempt_header(attempt: u32, max_attempts: u32) -> String {
    let (attempt, max) = clamp_attempt(attempt, max_attempts);
    format!("{attempt};max={max}")
}

/// AWS SDK 的 `amz-sdk-request`：`attempt=1; max=3`。
///
/// 必须与 [`kiro_attempt_header`] 取同一对 (attempt, max)。此前这个头被写死成
/// `attempt=1`，而 `x-kiro-attempt` 是动态的——同一次重试里两个描述重试进度的头
/// 互相矛盾（第 2 跳时一个说 2、一个说 1），真实 SDK 不会这样。
pub fn amz_sdk_request_header(attempt: u32, max_attempts: u32) -> String {
    let (attempt, max) = clamp_attempt(attempt, max_attempts);
    format!("attempt={attempt}; max={max}")
}

fn clamp_attempt(attempt: u32, max_attempts: u32) -> (u32, u32) {
    let max = max_attempts.max(1);
    (attempt.clamp(1, max), max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_blank_endpoint_follows_default() {
        assert_eq!(resolve_primary_endpoint(None, "ide"), "ide");
        assert_eq!(resolve_primary_endpoint(Some(""), "runtime"), "runtime");
        assert_eq!(resolve_primary_endpoint(Some("  "), "ide"), "ide");
        assert_eq!(
            resolve_primary_endpoint(Some("runtime"), "ide"),
            "runtime"
        );
        assert_eq!(resolve_primary_endpoint(Some(" ide "), "runtime"), "ide");
    }

    #[test]
    fn enterprise_hop_only_swaps_ide_and_runtime() {
        assert_eq!(enterprise_ide_runtime_hop("ide"), vec!["runtime".to_string()]);
        assert_eq!(enterprise_ide_runtime_hop("runtime"), vec!["ide".to_string()]);
        assert_eq!(
            enterprise_ide_runtime_hop("amazonq"),
            vec!["ide".to_string(), "runtime".to_string()]
        );
        assert_eq!(flip_ide_runtime("ide"), "runtime");
        assert_eq!(flip_ide_runtime("runtime"), "ide");
    }

    fn same_endpoint_and_none_drop_hop_chain() {
        let hop = vec!["runtime".into(), "codewhisperer".into()];
        assert_eq!(
            apply_bucket_mode(RateLimitBucketMode::Hop, hop.clone()),
            hop
        );
        assert!(apply_bucket_mode(RateLimitBucketMode::SameEndpoint, hop.clone()).is_empty());
        assert!(apply_bucket_mode(RateLimitBucketMode::None, hop).is_empty());
    }

    #[test]
    fn same_endpoint_stays_until_third_failure() {
        assert!(stay_on_same_endpoint(
            RateLimitBucketMode::SameEndpoint,
            1,
            3,
            false
        ));
        assert!(stay_on_same_endpoint(
            RateLimitBucketMode::SameEndpoint,
            2,
            3,
            false
        ));
        assert!(!stay_on_same_endpoint(
            RateLimitBucketMode::SameEndpoint,
            3,
            3,
            false
        ));
    }

    #[test]
    fn account_throttle_or_other_modes_do_not_stay() {
        assert!(!stay_on_same_endpoint(
            RateLimitBucketMode::SameEndpoint,
            1,
            3,
            true
        ));
        assert!(!stay_on_same_endpoint(RateLimitBucketMode::Hop, 1, 3, false));
        assert!(!stay_on_same_endpoint(RateLimitBucketMode::None, 1, 3, false));
    }

    #[test]
    fn attempt_header_matches_official() {
        assert_eq!(kiro_attempt_header(1, 3), "1;max=3");
        assert_eq!(kiro_attempt_header(2, 3), "2;max=3");
        assert_eq!(kiro_attempt_header(3, 3), "3;max=3");
        assert_eq!(kiro_attempt_header(0, 3), "1;max=3");
        assert_eq!(kiro_attempt_header(9, 3), "3;max=3");
    }

    #[test]
    fn amz_sdk_request_tracks_the_same_attempt_as_kiro_header() {
        for (attempt, max) in [(1, 3), (2, 3), (3, 3), (0, 3), (9, 3), (1, 1)] {
            let kiro = kiro_attempt_header(attempt, max);
            let amz = amz_sdk_request_header(attempt, max);
            let (n, m) = kiro.split_once(";max=").expect("形如 2;max=3");
            assert_eq!(amz, format!("attempt={n}; max={m}"), "两个头必须同源");
        }
        assert_eq!(amz_sdk_request_header(2, 3), "attempt=2; max=3");
    }
}
