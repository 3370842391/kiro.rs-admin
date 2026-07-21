from __future__ import annotations

from dataclasses import dataclass

from .api_key_client import ApiKeyError
from .enterprise_http import EnterpriseHttpClient

# 企业 idc 号 OIDC access_token 刷新(refresh_token 授权)。
#
# 库里存放较久的账号,其 access_token 通常几小时就过期;不刷新直接建 key 会大面积失败。
# 这里用登录时一并存下的 client_id / client_secret / refresh_token,走 AWS SSO OIDC
# `refresh_token` 授权换新 access_token,再交给 api_key_client 建 key。
#
# OIDC base 与登录时同源(绝不另造域名):
#   d-xxx.awsapps.com                     → https://oidc.{region}.amazonaws.com/token
#   ssoins-xxx.portal.{region}.app.aws    → https://oidc.{region}.api.aws/token
# 复用 EnterpriseHttpClient._parse_portal_target(已测过的正则)判定 old / new 门户。

REFRESH_GRANT = "refresh_token"


@dataclass(slots=True, frozen=True)
class RefreshResult:
    access_token: str
    refresh_token: str | None
    expires_in: int | None


def _oidc_token_url(*, start_url: str, region: str) -> str:
    """按登录时同款规则推导 OIDC token 端点;start_url 非法则抛 ApiKeyError。"""
    try:
        target = EnterpriseHttpClient._parse_portal_target(start_url, region)
    except Exception as error:  # noqa: BLE001 - 归一为 ApiKeyError
        raise ApiKeyError(
            "invalid_start_url",
            "refresh_token",
            False,
            "刷新 token 失败:企业 Start URL 无效",
        ) from error
    base = (
        f"https://oidc.{target.region}.api.aws"
        if target.instance_id
        else f"https://oidc.{target.region}.amazonaws.com"
    )
    return base + "/token"


async def refresh_access_token(
    transport,
    *,
    client_id: str,
    client_secret: str,
    refresh_token: str,
    start_url: str,
    region: str,
) -> RefreshResult:
    """用 refresh_token 换新 access_token;缺料或失败抛 ApiKeyError。"""
    if not (client_id or "").strip() or not (client_secret or "").strip():
        raise ApiKeyError(
            "missing_oidc_client",
            "refresh_token",
            False,
            "缺少 client_id / client_secret,无法刷新 token",
        )
    if not (refresh_token or "").strip():
        raise ApiKeyError(
            "missing_refresh_token",
            "refresh_token",
            False,
            "缺少 refresh_token,无法刷新 token",
        )
    url = _oidc_token_url(start_url=start_url, region=region)
    payload = {
        "clientId": client_id,
        "clientSecret": client_secret,
        "refreshToken": refresh_token,
        "grantType": REFRESH_GRANT,
    }
    try:
        response = await transport.request("POST", url, json=payload)
    except ApiKeyError:
        raise
    except Exception as error:  # noqa: BLE001 - 网络层统一归类
        raise ApiKeyError(
            "network_error", "refresh_token", True, "刷新 token 网络请求失败"
        ) from error
    status = response.status_code
    data = response.data if isinstance(response.data, dict) else {}
    if not (200 <= status < 300):
        message = ""
        if isinstance(data, dict):
            message = str(data.get("error") or data.get("message") or "")
        raise ApiKeyError(
            "refresh_failed",
            "refresh_token",
            status >= 500 or status == 429,
            message or f"刷新 token 失败(HTTP {status})",
            status_code=status,
        )
    access = data.get("accessToken")
    if not isinstance(access, str) or not access.strip():
        raise ApiKeyError(
            "refresh_missing_token",
            "refresh_token",
            False,
            "刷新响应未返回 accessToken",
        )
    new_refresh = data.get("refreshToken")
    expires = data.get("expiresIn")
    return RefreshResult(
        access_token=access.strip(),
        refresh_token=(
            new_refresh.strip()
            if isinstance(new_refresh, str) and new_refresh.strip()
            else None
        ),
        expires_in=(
            expires if isinstance(expires, int) and not isinstance(expires, bool) else None
        ),
    )
