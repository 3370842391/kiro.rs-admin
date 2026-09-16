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

/// 企业号专项轮询的四个候选端点（同一 IDE 协议族，不含 CLI）。
///
/// 顺序：Legacy Kiro IDE → Kiro Runtime → Legacy Amazon Q → Legacy CodeWhisperer。
pub const ENTERPRISE_ROUND_ROBIN_NODES: &[&str] = &["ide", "runtime", "amazonq", "codewhisperer"];

/// 从 `start` 起按轮询环旋转，下标 `index` 取节点。
pub fn enterprise_node_from_default(start: &str, index: u32) -> &'static str {
    let nodes = enterprise_nodes_rotated(start);
    nodes[(index as usize) % nodes.len()]
}

pub fn enterprise_nodes_rotated(start: &str) -> [&'static str; 4] {
    let start_idx = ENTERPRISE_ROUND_ROBIN_NODES
        .iter()
        .position(|n| *n == start.trim())
        .unwrap_or(0);
    std::array::from_fn(|i| {
        ENTERPRISE_ROUND_ROBIN_NODES[(start_idx + i) % ENTERPRISE_ROUND_ROBIN_NODES.len()]
    })
}

/// 当前桶之后按 ide → runtime → amazonq → codewhisperer 环进一格。
pub fn next_enterprise_node(current: &str) -> &'static str {
    let trimmed = current.trim();
    let idx = ENTERPRISE_ROUND_ROBIN_NODES
        .iter()
        .position(|n| *n == trimmed)
        .unwrap_or(0);
    ENTERPRISE_ROUND_ROBIN_NODES[(idx + 1) % ENTERPRISE_ROUND_ROBIN_NODES.len()]
}

/// 本跳之后按轮询顺序试剩下 3 个桶。
pub fn enterprise_round_robin_hop(primary: &str) -> Vec<String> {
    let trimmed = primary.trim();
    let start = ENTERPRISE_ROUND_ROBIN_NODES
        .iter()
        .position(|n| *n == trimmed)
        .unwrap_or(0);
    (1..ENTERPRISE_ROUND_ROBIN_NODES.len())
        .map(|off| {
            ENTERPRISE_ROUND_ROBIN_NODES[(start + off) % ENTERPRISE_ROUND_ROBIN_NODES.len()]
                .to_string()
        })
        .collect()
}

/// 企业号专项：四个候选端点轮询（旧名保留，避免漏改调用点）。
pub fn enterprise_ide_runtime_hop(primary: &str) -> Vec<String> {
    enterprise_round_robin_hop(primary)
}

/// 本轮四个桶走完后，下一轮从环上下一个端点起手。
pub fn flip_ide_runtime(current: &str) -> &'static str {
    next_enterprise_node(current)
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
    fn enterprise_round_robin_walks_four_ide_buckets() {
        assert_eq!(
            enterprise_round_robin_hop("ide"),
            vec![
                "runtime".to_string(),
                "amazonq".to_string(),
                "codewhisperer".to_string()
            ]
        );
        assert_eq!(
            enterprise_round_robin_hop("runtime"),
            vec![
                "amazonq".to_string(),
                "codewhisperer".to_string(),
                "ide".to_string()
            ]
        );
        assert_eq!(
            enterprise_round_robin_hop("amazonq"),
            vec![
                "codewhisperer".to_string(),
                "ide".to_string(),
                "runtime".to_string()
            ]
        );
        assert_eq!(
            enterprise_round_robin_hop("codewhisperer"),
            vec![
                "ide".to_string(),
                "runtime".to_string(),
                "amazonq".to_string()
            ]
        );
        assert_eq!(next_enterprise_node("ide"), "runtime");
        assert_eq!(next_enterprise_node("runtime"), "amazonq");
        assert_eq!(next_enterprise_node("amazonq"), "codewhisperer");
        assert_eq!(next_enterprise_node("codewhisperer"), "ide");
        assert_eq!(next_enterprise_node("unknown"), "runtime");
        assert_eq!(flip_ide_runtime("codewhisperer"), "ide");
        assert_eq!(
            enterprise_ide_runtime_hop("ide"),
            enterprise_round_robin_hop("ide")
        );
    }

    #[test]
    fn enterprise_start_rotates_from_configured_default() {
        assert_eq!(enterprise_node_from_default("ide", 0), "ide");
        assert_eq!(enterprise_node_from_default("ide", 1), "runtime");
        assert_eq!(enterprise_node_from_default("runtime", 0), "runtime");
        assert_eq!(enterprise_node_from_default("runtime", 1), "amazonq");
        assert_eq!(enterprise_node_from_default("amazonq", 3), "runtime");
        assert_eq!(
            enterprise_nodes_rotated("codewhisperer"),
            ["codewhisperer", "ide", "runtime", "amazonq"]
        );
    }

    #[test]
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
