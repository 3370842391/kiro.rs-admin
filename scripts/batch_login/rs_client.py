from __future__ import annotations

import asyncio
import re
from dataclasses import dataclass
from typing import Any, Iterable
from urllib.parse import parse_qs, urlsplit, urlunsplit

import httpx


SESSION_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")


def _normalize_base_url(raw_url: str) -> str:
    try:
        parts = urlsplit(raw_url.strip())
        _ = parts.port
    except (AttributeError, TypeError, ValueError) as error:
        raise ValueError("无效的 RS URL") from error
    if (
        parts.scheme not in {"http", "https"}
        or not parts.hostname
        or parts.username is not None
        or parts.password is not None
        or parts.query
        or parts.fragment
    ):
        raise ValueError("RS URL 必须是无凭据、query 和 fragment 的 HTTP(S) 地址")

    path = "/" + "/".join(segment for segment in parts.path.split("/") if segment)
    if path == "/":
        path = ""
    if not path.endswith("/api/admin"):
        path += "/api/admin"
    return urlunsplit((parts.scheme, parts.netloc, path, "", ""))


def _session_path(session_id: str) -> str:
    if not isinstance(session_id, str) or not SESSION_ID_RE.fullmatch(session_id):
        raise ValueError("无效的登录会话 ID")
    return session_id


@dataclass(slots=True)
class RsApiError(Exception):
    code: str
    stage: str
    retryable: bool
    status_code: int
    message: str

    def __str__(self) -> str:
        status = f"HTTP {self.status_code}" if self.status_code else "network"
        return f"{self.code} at {self.stage} ({status}): {self.message}"


def parse_callback_url(raw_url: str) -> dict[str, Any]:
    try:
        parts = urlsplit(raw_url.strip())
    except (AttributeError, TypeError, ValueError) as error:
        raise ValueError("无效的回调 URL") from error

    if parts.scheme not in {"http", "https"} or not parts.netloc:
        raise ValueError("无效的回调 URL")

    query_params = parse_qs(parts.query, keep_blank_values=True)
    fragment_params = parse_qs(parts.fragment, keep_blank_values=True)
    for key, values in fragment_params.items():
        query_params.setdefault(key, values)

    def one(*names: str) -> str | None:
        for name in names:
            value = query_params.get(name, [None])[0]
            if value:
                return value
        return None

    payload: dict[str, Any] = {
        "code": one("code"),
        "state": one("state"),
        "loginOption": one("login_option", "loginOption") or "",
        "path": parts.path,
        "issuerUrl": one("issuer_url", "issuerUrl"),
        "clientId": one("client_id", "clientId"),
        "scopes": one("scopes", "scope"),
        "loginHint": one("login_hint", "loginHint"),
    }
    if not payload["code"] and not (payload["issuerUrl"] and payload["clientId"]):
        raise ValueError("回调 URL 缺少 code 或 external_idp descriptor")
    return {key: value for key, value in payload.items() if value is not None}


class RsClient:
    def __init__(
        self,
        base_url: str,
        admin_key: str,
        *,
        timeout: float = 30,
        transport: httpx.AsyncBaseTransport | None = None,
        retry_delays: Iterable[float] = (0.5, 1.0),
    ):
        self.base_url = _normalize_base_url(base_url)
        self.retry_delays = tuple(retry_delays)[:2]
        self.client = httpx.AsyncClient(
            headers={"x-api-key": admin_key, "accept": "application/json"},
            timeout=timeout,
            transport=transport,
        )

    async def __aenter__(self) -> "RsClient":
        return self

    async def __aexit__(self, *_args: object) -> None:
        await self.client.aclose()

    async def _request(
        self,
        method: str,
        path: str,
        json: dict[str, Any] | None = None,
        *,
        retry_safe: bool | None = None,
    ) -> dict[str, Any]:
        if retry_safe is None:
            retry_safe = method.upper() in {"GET", "HEAD", "OPTIONS", "DELETE"}
        for attempt in range(len(self.retry_delays) + 1):
            try:
                response = await self.client.request(
                    method,
                    self.base_url + path,
                    json=json,
                )
            except httpx.RequestError as error:
                if retry_safe and attempt < len(self.retry_delays):
                    await asyncio.sleep(self.retry_delays[attempt])
                    continue
                raise RsApiError(
                    code="network_error",
                    stage="rs_request",
                    retryable=retry_safe,
                    status_code=0,
                    message="RS 网络请求失败",
                ) from error

            if 200 <= response.status_code < 300:
                if response.status_code == 204:
                    return {}
                try:
                    response_body = response.json()
                except ValueError as error:
                    raise RsApiError(
                        "invalid_rs_response",
                        "rs_response",
                        False,
                        response.status_code,
                        "RS 响应格式无效",
                    ) from error
                if not isinstance(response_body, dict):
                    raise RsApiError(
                        "invalid_rs_response",
                        "rs_response",
                        False,
                        response.status_code,
                        "RS 响应格式无效",
                    )
                return response_body

            if 300 <= response.status_code < 400:
                raise RsApiError(
                    "invalid_rs_response",
                    "rs_response",
                    False,
                    response.status_code,
                    "RS 响应格式无效",
                )

            try:
                response_body = response.json()
            except ValueError:
                response_body = {}
            error_body = response_body.get("error", {}) if isinstance(response_body, dict) else {}
            if not isinstance(error_body, dict):
                error_body = {}

            raw_retryable = error_body.get("retryable")
            declared_retryable = raw_retryable if isinstance(raw_retryable, bool) else None
            should_retry = (
                declared_retryable is True
                or (
                    declared_retryable is None
                    and retry_safe
                    and response.status_code >= 500
                )
            )
            if should_retry and attempt < len(self.retry_delays):
                await asyncio.sleep(self.retry_delays[attempt])
                continue

            code = error_body.get("code")
            if not code:
                if response.status_code in {401, 403}:
                    code = "rs_auth_failed"
                elif response.status_code >= 500:
                    code = "upstream_error"
                else:
                    code = "rs_internal_error"

            raise RsApiError(
                code=str(code),
                stage=str(error_body.get("stage") or "rs_request"),
                retryable=(
                    declared_retryable
                    if declared_retryable is not None
                    else retry_safe and response.status_code >= 500
                ),
                status_code=response.status_code,
                message="RS 请求失败",
            )

        raise AssertionError("unreachable")

    async def preflight(self) -> None:
        await self._request("GET", "/credentials")

    async def start_idc(self, *, region: str, start_url: str, email: str) -> dict[str, Any]:
        return await self._request(
            "POST",
            "/auth/idc/start",
            {"region": region, "startUrl": start_url, "email": email},
        )

    async def poll_idc(self, session_id: str) -> dict[str, Any]:
        return await self._request(
            "POST",
            f"/auth/idc/poll/{_session_path(session_id)}",
            retry_safe=True,
        )

    async def start_social(self, *, email: str) -> dict[str, Any]:
        return await self._request("POST", "/auth/social/start", {"email": email})

    async def complete_social(self, session_id: str, callback_url: str) -> dict[str, Any]:
        callback = parse_callback_url(callback_url)
        return await self._request(
            "POST",
            f"/auth/social/complete/{_session_path(session_id)}",
            callback,
        )

    async def cancel_idc(self, session_id: str) -> dict[str, Any]:
        return await self._request("DELETE", f"/auth/idc/{_session_path(session_id)}")

    async def cancel_social(self, session_id: str) -> dict[str, Any]:
        return await self._request("DELETE", f"/auth/social/{_session_path(session_id)}")
