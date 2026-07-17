from __future__ import annotations

from dataclasses import dataclass

# Kiro 门户 API Key(ksk_)创建 / 列举客户端。
#
# 实测(2026-07-16)结论:
# - 鉴权只需 `authorization: Bearer <token>`,cookie / csrf / x-kiro-* 均可省。
# - token 即 device-code / IdC 登录流拿到的 access_token(格式 aoaAAAA...:<DER-ECDSA>)。
# - 两步链路(均 AWS-JSON-1.0):
#     1. q.{region}.amazonaws.com  AmazonCodeWhispererService.ListAvailableProfiles → profileArn
#     2. management.{region}.kiro.dev  KiroControlPlaneBearerService.CreateApiKey → rawKey(仅创建时返回一次)
# - 企业 SSO(external_idp)调 ListAvailableProfiles 必须带 `tokentype: EXTERNAL_IDP`,
#   否则上游静默返回空 profile 列表;idc / social 不需要该头。

CONTENT_TYPE = "application/x-amz-json-1.0"
_PROFILE_TARGET = "AmazonCodeWhispererService.ListAvailableProfiles"
_CREATE_TARGET = "KiroControlPlaneBearerService.CreateApiKey"
_LIST_TARGET = "KiroControlPlaneBearerService.ListApiKeys"
DEFAULT_REGION = "us-east-1"


@dataclass(slots=True)
class ApiKeyError(Exception):
    code: str
    stage: str
    retryable: bool
    message: str
    status_code: int | None = None

    def __str__(self) -> str:
        return self.message


def _q_url(region: str) -> str:
    return f"https://q.{region}.amazonaws.com/"


def _management_url(region: str) -> str:
    return f"https://management.{region}.kiro.dev/"


def _bearer(token: str) -> str:
    value = (token or "").strip()
    if not value:
        raise ApiKeyError(
            "missing_token", "api_key_auth", False, "缺少 access_token,无法创建 API Key"
        )
    return value if value.lower().startswith("bearer ") else f"Bearer {value}"


def _headers(*, token: str, target: str, token_type: str | None) -> dict[str, str]:
    headers = {
        "content-type": CONTENT_TYPE,
        "authorization": _bearer(token),
        "x-amz-target": target,
    }
    if token_type:
        headers["tokentype"] = token_type
    return headers


def _as_object(data: object, stage: str) -> dict[str, object]:
    if isinstance(data, dict):
        return data
    raise ApiKeyError(
        "invalid_response", stage, False, "API Key 接口响应格式无效"
    )


def _raise_for_status(response, stage: str) -> dict[str, object]:
    status = response.status_code
    body = response.data if isinstance(response.data, dict) else {}
    if 200 <= status < 300:
        return _as_object(response.data, stage)
    message = ""
    if isinstance(body, dict):
        message = str(body.get("message") or body.get("Message") or "")
    raise ApiKeyError(
        "http_error",
        stage,
        status >= 500 or status == 429,
        message or f"{stage} 请求失败(HTTP {status})",
        status_code=status,
    )


async def resolve_profile_arn(
    transport,
    *,
    token: str,
    region: str = DEFAULT_REGION,
    token_type: str | None = None,
) -> str | None:
    """打 ListAvailableProfiles 取第一个真实 profileArn(取不到返回 None)。"""
    try:
        response = await transport.request(
            "POST",
            _q_url(region),
            headers=_headers(
                token=token, target=_PROFILE_TARGET, token_type=token_type
            ),
            json={"maxResults": 10},
        )
    except ApiKeyError:
        raise
    except Exception as error:  # noqa: BLE001 - 网络层统一归类
        raise ApiKeyError(
            "network_error", "list_profiles", True, "ListAvailableProfiles 网络请求失败"
        ) from error
    body = _raise_for_status(response, "list_profiles")
    profiles = body.get("profiles")
    if not isinstance(profiles, list):
        return None
    for item in profiles:
        if not isinstance(item, dict):
            continue
        arn = item.get("arn")
        if isinstance(arn, str) and arn.strip():
            return arn.strip()
    return None


async def list_api_keys(
    transport,
    *,
    token: str,
    profile_arn: str,
    region: str = DEFAULT_REGION,
) -> list[dict[str, object]]:
    """列举现有 key(仅含 keyId/keyPrefix/label,无 rawKey)。"""
    try:
        response = await transport.request(
            "POST",
            _management_url(region),
            headers=_headers(token=token, target=_LIST_TARGET, token_type=None),
            json={"profileArn": profile_arn},
        )
    except ApiKeyError:
        raise
    except Exception as error:  # noqa: BLE001
        raise ApiKeyError(
            "network_error", "list_api_keys", True, "ListApiKeys 网络请求失败"
        ) from error
    body = _raise_for_status(response, "list_api_keys")
    keys = body.get("keys")
    return [item for item in keys if isinstance(item, dict)] if isinstance(keys, list) else []


async def create_api_key(
    transport,
    *,
    token: str,
    profile_arn: str,
    label: str,
    region: str = DEFAULT_REGION,
) -> str:
    """创建一把 key,返回完整 rawKey(ksk_...);拿不到 rawKey 视为失败。"""
    try:
        response = await transport.request(
            "POST",
            _management_url(region),
            headers=_headers(token=token, target=_CREATE_TARGET, token_type=None),
            json={"profileArn": profile_arn, "label": label},
        )
    except ApiKeyError:
        raise
    except Exception as error:  # noqa: BLE001
        raise ApiKeyError(
            "network_error", "create_api_key", True, "CreateApiKey 网络请求失败"
        ) from error
    body = _raise_for_status(response, "create_api_key")
    raw = body.get("rawKey")
    if not isinstance(raw, str) or not raw.strip():
        raise ApiKeyError(
            "missing_raw_key",
            "create_api_key",
            False,
            "CreateApiKey 未返回 rawKey",
        )
    return raw.strip()


@dataclass(slots=True, frozen=True)
class ApiKeyResult:
    raw_key: str | None
    profile_arn: str | None
    reused: bool = False


async def ensure_api_key(
    transport,
    *,
    token: str,
    label: str,
    region: str = DEFAULT_REGION,
    profile_arn: str | None = None,
    token_type: str | None = None,
    skip_if_labeled_exists: bool = False,
) -> ApiKeyResult:
    """编排:缺 profileArn 先解析;可选按 label 判重;否则创建并返回 rawKey。

    skip_if_labeled_exists=True 且同 label 已存在时,返回 raw_key=None、reused=True
    (rawKey 无法二次获取,调用方需保留库中旧值或标注需手动)。
    """
    resolved = profile_arn
    if not resolved:
        resolved = await resolve_profile_arn(
            transport, token=token, region=region, token_type=token_type
        )
    if not resolved:
        raise ApiKeyError(
            "no_profile_arn",
            "list_profiles",
            False,
            "未能解析出 profileArn,无法创建 API Key",
        )
    if skip_if_labeled_exists:
        existing = await list_api_keys(
            transport, token=token, profile_arn=resolved, region=region
        )
        if any(str(item.get("label", "")) == label for item in existing):
            return ApiKeyResult(raw_key=None, profile_arn=resolved, reused=True)
    raw = await create_api_key(
        transport,
        token=token,
        profile_arn=resolved,
        label=label,
        region=region,
    )
    return ApiKeyResult(raw_key=raw, profile_arn=resolved, reused=False)
