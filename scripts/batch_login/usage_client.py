from __future__ import annotations

from dataclasses import dataclass
from urllib.parse import urlencode

# Kiro 门户额度查询(GetUsageLimits REST)。
#
# 参考 Kiro-account-manager 官方格式实测:
#   GET https://q.{region}.amazonaws.com/getUsageLimits
#       ?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST&isEmailRequired=true&profileArn=...
#   头: Authorization: Bearer <token>, Accept: application/json,
#       User-Agent / x-amz-user-agent 模拟 KiroIDE;企业号(external_idp)带 TokenType: EXTERNAL_IDP。
#   主区域 403 时切另一区域兜底(官方仅 us-east-1 / eu-central-1)。
# 响应 usageBreakdownList 里 resourceType==CREDIT 一项:
#   usageLimit(总额度) / currentUsage(已用),剩余 = 总 - 已用;另有 freeTrialInfo、nextDateReset。

_KIRO_VERSION = "0.6.18"
_ACCEPT = "application/json"
_REST_ENDPOINTS = {
    "us-east-1": "https://q.us-east-1.amazonaws.com",
    "eu-central-1": "https://q.eu-central-1.amazonaws.com",
}


@dataclass(slots=True)
class UsageError(Exception):
    code: str
    stage: str
    retryable: bool
    message: str
    status_code: int | None = None

    def __str__(self) -> str:
        return self.message


@dataclass(slots=True, frozen=True)
class UsageSnapshot:
    remaining: float
    total: float
    used: float
    subscription: str | None
    free_trial: bool
    next_reset: str | None

    def display(self) -> str:
        def fmt(value: float) -> str:
            return str(int(value)) if float(value).is_integer() else f"{value:.2f}"
        return f"剩余 {fmt(self.remaining)} / 总 {fmt(self.total)}"


def _rest_base(region: str | None) -> str:
    if not region:
        return _REST_ENDPOINTS["us-east-1"]
    if region in _REST_ENDPOINTS:
        return _REST_ENDPOINTS[region]
    if region.startswith("eu-"):
        return _REST_ENDPOINTS["eu-central-1"]
    return _REST_ENDPOINTS["us-east-1"]


def _fallback_base(region: str | None) -> str:
    primary = _rest_base(region)
    return (
        _REST_ENDPOINTS["us-east-1"]
        if primary == _REST_ENDPOINTS["eu-central-1"]
        else _REST_ENDPOINTS["eu-central-1"]
    )


def _user_agent(machine_id: str | None) -> str:
    suffix = f"KiroIDE-{_KIRO_VERSION}-{machine_id}" if machine_id else f"KiroIDE-{_KIRO_VERSION}"
    return (
        f"aws-sdk-js/1.0.18 ua/2.1 os/windows lang/js md/nodejs#20.16.0 "
        f"api/codewhispererstreaming#1.0.18 m/E {suffix}"
    )


def _amz_user_agent(machine_id: str | None) -> str:
    suffix = f"KiroIDE {_KIRO_VERSION} {machine_id}" if machine_id else f"KiroIDE-{_KIRO_VERSION}"
    return f"aws-sdk-js/1.0.18 {suffix}"


def _bearer(token: str) -> str:
    value = (token or "").strip()
    if not value:
        raise UsageError("missing_token", "usage_auth", False, "缺少 access_token,无法查询额度")
    return value if value.lower().startswith("bearer ") else f"Bearer {value}"


def _headers(*, token: str, token_type: str | None, machine_id: str | None) -> dict[str, str]:
    headers = {
        "Accept": _ACCEPT,
        "Authorization": _bearer(token),
        "User-Agent": _user_agent(machine_id),
        "x-amz-user-agent": _amz_user_agent(machine_id),
    }
    if token_type:
        headers["TokenType"] = token_type
    return headers


def _path(profile_arn: str | None) -> str:
    params = {
        "origin": "AI_EDITOR",
        "resourceType": "AGENTIC_REQUEST",
        "isEmailRequired": "true",
    }
    if profile_arn:
        params["profileArn"] = profile_arn
    return "/getUsageLimits?" + urlencode(params)


def _num(value: object) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def _parse(body: dict[str, object]) -> UsageSnapshot:
    breakdown = body.get("usageBreakdownList")
    credit: dict[str, object] = {}
    if isinstance(breakdown, list):
        for item in breakdown:
            if isinstance(item, dict) and (
                item.get("resourceType") == "CREDIT" or item.get("displayName") == "Credits"
            ):
                credit = item
                break
    base_limit = _num(credit.get("usageLimitWithPrecision")) or _num(credit.get("usageLimit")) or 0.0
    base_used = _num(credit.get("currentUsageWithPrecision")) or _num(credit.get("currentUsage")) or 0.0

    free_trial = False
    ft_limit = ft_used = 0.0
    ft = credit.get("freeTrialInfo")
    if isinstance(ft, dict) and str(ft.get("freeTrialStatus", "")).upper() == "ACTIVE":
        free_trial = True
        ft_limit = _num(ft.get("usageLimitWithPrecision")) or _num(ft.get("usageLimit")) or 0.0
        ft_used = _num(ft.get("currentUsageWithPrecision")) or _num(ft.get("currentUsage")) or 0.0

    bonus_limit = bonus_used = 0.0
    bonuses = credit.get("bonuses")
    if isinstance(bonuses, list):
        for bonus in bonuses:
            if isinstance(bonus, dict) and str(bonus.get("status", "")).upper() == "ACTIVE":
                bonus_limit += _num(bonus.get("usageLimitWithPrecision")) or _num(bonus.get("usageLimit")) or 0.0
                bonus_used += _num(bonus.get("currentUsageWithPrecision")) or _num(bonus.get("currentUsage")) or 0.0

    total = base_limit + ft_limit + bonus_limit
    used = base_used + ft_used + bonus_used
    remaining = max(0.0, total - used)

    subscription = None
    info = body.get("subscriptionInfo")
    if isinstance(info, dict):
        subscription = info.get("subscriptionTitle") or info.get("subscriptionName")

    next_reset = body.get("nextDateReset")
    if isinstance(next_reset, (int, float)) and not isinstance(next_reset, bool):
        from datetime import datetime, timezone

        next_reset = datetime.fromtimestamp(next_reset, tz=timezone.utc).isoformat().replace("+00:00", "Z")
    elif not isinstance(next_reset, str):
        next_reset = None

    return UsageSnapshot(
        remaining=remaining,
        total=total,
        used=used,
        subscription=str(subscription) if subscription else None,
        free_trial=free_trial,
        next_reset=next_reset,
    )


async def get_usage_limits(
    transport,
    *,
    token: str,
    profile_arn: str | None = None,
    region: str | None = None,
    token_type: str | None = None,
    machine_id: str | None = None,
) -> UsageSnapshot:
    """查询账号剩余额度;403 自动切另一区域;失败抛 UsageError。"""
    headers = _headers(token=token, token_type=token_type, machine_id=machine_id)
    path = _path(profile_arn)

    async def call(base: str):
        try:
            return await transport.request("GET", base + path, headers=headers)
        except UsageError:
            raise
        except Exception as error:  # noqa: BLE001 - 网络层统一归类
            raise UsageError("network_error", "get_usage", True, "额度查询网络请求失败") from error

    response = await call(_rest_base(region))
    if response.status_code == 403:
        response = await call(_fallback_base(region))

    status = response.status_code
    if not (200 <= status < 300):
        body = response.data if isinstance(response.data, dict) else {}
        message = str(body.get("message") or body.get("Message") or "") if isinstance(body, dict) else ""
        raise UsageError(
            "http_error",
            "get_usage",
            status >= 500 or status == 429,
            message or f"额度查询失败(HTTP {status})",
            status_code=status,
        )
    if not isinstance(response.data, dict):
        raise UsageError("invalid_response", "get_usage", False, "额度接口响应格式无效")
    return _parse(response.data)
