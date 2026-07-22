//! 封号 / 额度 状态分类器（P0）
//!
//! 现状：`provider.rs` 把所有 401/403 混当"凭据失败"做 failover，不区分
//! "账号被封 403"（该号已死）与"跨区 token 兼容性 403"（可回退重试）。
//! 质保机制必须能精确分辨前者——这就是本模块。
//!
//! 数据来源：后台探活调用 `ListAvailableModels`，其失败经 anyhow 冒泡后
//! Display 形如 `"ListAvailableModels HTTP 403: {body}"`。本模块从
//! (status, body) 或错误文本判定账号健康度。
//!
//! ⚠ 封号文案未来可能变。分类器**宽松匹配**封禁关键词，并要求调用方把原始
//! body 落审计；新出现的封禁措辞先记录再补规则，宁可误判成 `TransientAuth`
//! 触发重试，也不要把真封号漏成 `Active`（那会把死号继续卖出去）。

/// 账号健康度
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountHealth {
    /// 200，且（如提供额度信息）额度正常
    Active,
    /// 200 但剩余额度 <= 阈值：号还活着，额度快/已耗尽
    LowQuota,
    /// 403 且响应体命中封禁关键词 → 被封（死号）
    Dead,
    /// 401/403 但属跨区 / token 刷新类，可重试，不算死
    TransientAuth,
    /// 其它状态码（网络/5xx 等），按瞬态处理但记录
    Unknown(u16),
}

impl AccountHealth {
    /// 是否判定为死号（需要移出池 + 触发质保）
    pub fn is_dead(&self) -> bool {
        matches!(self, AccountHealth::Dead)
    }
    /// 号是否仍可用（活着，可交付 / 可继续持有）
    pub fn is_alive(&self) -> bool {
        matches!(self, AccountHealth::Active | AccountHealth::LowQuota)
    }
    /// 稳定字符串，用于持久化 holdings.status
    pub fn as_status_str(&self) -> &'static str {
        match self {
            AccountHealth::Active => "active",
            AccountHealth::LowQuota => "low_quota",
            AccountHealth::Dead => "dead",
            // 瞬态/未知不落库为终态，调用方通常保留原 status；仅用于日志
            AccountHealth::TransientAuth => "transient",
            AccountHealth::Unknown(_) => "unknown",
        }
    }
}

/// 强封禁标记（命中任一即判死，无需再要求其它字样）。你贴的真实响应：
/// `403 Forbidden {"message":"Your User ID is temporarily su[spended]..."}`
/// 注意日志常被截断成 `temporarily su`，也要覆盖。
const BAN_MARKERS: &[&str] = &[
    "suspend",          // suspended / suspension（完整词）
    "temporarily su",   // 截断日志：temporarily su[spended]
    "banned",
    "account is disabled",
    "account has been disabled",
    "access denied because your account",
];

/// 判断 body 是否明确指向封号（命中任一强标记即算）
fn body_indicates_ban(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    BAN_MARKERS.iter().any(|m| b.contains(m))
}

/// 核心分类：由 HTTP 状态码 + 响应体判定健康度。
///
/// - `status == 200` → `Active`（额度维度由 `classify_with_quota` 叠加）
/// - `status == 403` 且 body 命中封禁 → `Dead`
/// - `status == 401 | 403` 其它 → `TransientAuth`（跨区兼容 / token 刷新类）
/// - 其它 → `Unknown(status)`
pub fn classify_account_health(status: u16, body: &str) -> AccountHealth {
    match status {
        200 => AccountHealth::Active,
        403 if body_indicates_ban(body) => AccountHealth::Dead,
        401 | 403 => AccountHealth::TransientAuth,
        s => AccountHealth::Unknown(s),
    }
}

/// 从探活错误文本判定（当只能拿到 anyhow 错误字符串时使用）。
///
/// `get_available_models_for` 失败经 anyhow 冒泡后文本形如：
/// `"ListAvailableModels HTTP 403: {\"message\":\"...suspended...\"}"`。
/// 无结构化 status 时，从文本抽取 `HTTP <code>` 并复用 `classify_account_health`。
pub fn classify_from_error_text(err_text: &str) -> AccountHealth {
    let status = extract_http_status(err_text).unwrap_or(0);
    if status == 0 {
        // 拿不到状态码：可能是网络层错误，按未知瞬态处理
        // 但若文本本身已含封禁字样（少见），仍判死
        if body_indicates_ban(err_text) {
            return AccountHealth::Dead;
        }
        return AccountHealth::Unknown(0);
    }
    classify_account_health(status, err_text)
}

/// 从错误文本里抽第一个 `HTTP <3位状态码>`（大小写不敏感）。
fn extract_http_status(text: &str) -> Option<u16> {
    let lower = text.to_ascii_lowercase();
    let idx = lower.find("http ")?;
    let rest = &text[idx + 5..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() == 3 {
        digits.parse().ok()
    } else {
        None
    }
}

/// 叠加额度维度：号活着（200）时，若剩余额度 <= 阈值则降级为 `LowQuota`。
///
/// `remaining` 为 None 表示未探到额度信息，保持 `Active`。
pub fn classify_with_quota(base: AccountHealth, remaining: Option<i64>, low_threshold: i64) -> AccountHealth {
    match base {
        AccountHealth::Active => match remaining {
            Some(r) if r <= low_threshold => AccountHealth::LowQuota,
            _ => AccountHealth::Active,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_suspended_403_is_dead() {
        // 用户贴的真实封号响应
        let body = r#"{"message":"Your User ID is temporarily suspended from making requests."}"#;
        assert_eq!(classify_account_health(403, body), AccountHealth::Dead);
    }

    #[test]
    fn truncated_suspended_log_is_dead() {
        // 截断日志（你贴的原文就是截断的）也要命中
        let body = r#"403 Forbidden {"message":"Your User ID is temporarily su"#;
        assert_eq!(classify_account_health(403, body), AccountHealth::Dead);
    }

    #[test]
    fn cross_region_403_without_ban_is_transient() {
        // 跨区兼容 403：body 里没有封禁字样 → 可重试，绝不能判死
        let body = r#"{"message":"Invalid token for this region"}"#;
        assert_eq!(classify_account_health(403, body), AccountHealth::TransientAuth);
    }

    #[test]
    fn improperly_formed_400_is_unknown() {
        let body = r#"{"message":"Improperly formed request"}"#;
        assert_eq!(classify_account_health(400, body), AccountHealth::Unknown(400));
    }

    #[test]
    fn plain_401_is_transient() {
        assert_eq!(classify_account_health(401, "unauthorized"), AccountHealth::TransientAuth);
    }

    #[test]
    fn ok_200_is_active() {
        assert_eq!(classify_account_health(200, r#"{"models":[]}"#), AccountHealth::Active);
    }

    #[test]
    fn banned_keyword_is_dead() {
        let body = r#"{"message":"This account has been banned for abuse"}"#;
        assert_eq!(classify_account_health(403, body), AccountHealth::Dead);
    }

    #[test]
    fn error_text_403_suspended_parses_dead() {
        let err = r#"ListAvailableModels HTTP 403: {"message":"Your User ID is temporarily suspended"}"#;
        assert_eq!(classify_from_error_text(err), AccountHealth::Dead);
    }

    #[test]
    fn error_text_403_cross_region_parses_transient() {
        let err = r#"ListAvailableModels HTTP 403: {"message":"Invalid token"}"#;
        assert_eq!(classify_from_error_text(err), AccountHealth::TransientAuth);
    }

    #[test]
    fn error_text_network_no_status_is_unknown() {
        let err = "error sending request: connection reset";
        assert_eq!(classify_from_error_text(err), AccountHealth::Unknown(0));
    }

    #[test]
    fn extract_status_works() {
        assert_eq!(extract_http_status("foo HTTP 403: bar"), Some(403));
        assert_eq!(extract_http_status("HTTP 200 OK"), Some(200));
        assert_eq!(extract_http_status("no status here"), None);
        assert_eq!(extract_http_status("HTTP 40: too short"), None);
    }

    #[test]
    fn quota_downgrade_to_low() {
        assert_eq!(
            classify_with_quota(AccountHealth::Active, Some(3), 5),
            AccountHealth::LowQuota
        );
        assert_eq!(
            classify_with_quota(AccountHealth::Active, Some(100), 5),
            AccountHealth::Active
        );
        assert_eq!(
            classify_with_quota(AccountHealth::Active, None, 5),
            AccountHealth::Active
        );
        // 死号不因额度信息改变
        assert_eq!(
            classify_with_quota(AccountHealth::Dead, Some(0), 5),
            AccountHealth::Dead
        );
    }

    #[test]
    fn health_helpers() {
        assert!(AccountHealth::Dead.is_dead());
        assert!(!AccountHealth::Active.is_dead());
        assert!(AccountHealth::Active.is_alive());
        assert!(AccountHealth::LowQuota.is_alive());
        assert!(!AccountHealth::Dead.is_alive());
        assert_eq!(AccountHealth::LowQuota.as_status_str(), "low_quota");
    }
}
