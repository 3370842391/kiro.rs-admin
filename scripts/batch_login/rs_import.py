from __future__ import annotations

import json
import re
from collections.abc import Callable, Iterable
from typing import Any

import httpx

from .redaction import mask_account, redact_text
from .rs_client import RsApiError, _normalize_base_url


_SSE_BOUNDARY_RE = re.compile(r"(?:\r\n|\r|\n)(?:\r\n|\r|\n)")
_SENSITIVE_EVENT_KEYS = {
    "accesstoken",
    "adminkey",
    "apikey",
    "authorization",
    "code",
    "refreshtoken",
    "idtoken",
    "clientsecret",
    "codeverifier",
    "password",
    "state",
    "token",
}


def parse_sse(buffer: str) -> tuple[list[dict[str, Any]], str]:
    events: list[dict[str, Any]] = []
    while match := _SSE_BOUNDARY_RE.search(buffer):
        raw = buffer[: match.start()]
        buffer = buffer[match.end() :]
        data_lines = [
            line[5:].lstrip(" ")
            for line in raw.splitlines()
            if line.startswith("data:")
        ]
        if not data_lines:
            continue
        try:
            value = json.loads("\n".join(data_lines))
        except json.JSONDecodeError as error:
            raise ValueError("RS 导入 SSE JSON 帧无效") from error
        if not isinstance(value, dict):
            raise ValueError("RS 导入 SSE 事件必须是 JSON 对象")
        events.append(value)
    return events, buffer


def _normalized_key(key: str) -> str:
    return key.replace("_", "").replace("-", "").casefold()


def _sanitize_event_value(value: Any, *, key: str = "") -> Any:
    normalized_key = _normalized_key(key)
    if normalized_key in _SENSITIVE_EVENT_KEYS:
        return "<redacted>"
    if isinstance(value, dict):
        return {
            item_key: _sanitize_event_value(item_value, key=str(item_key))
            for item_key, item_value in value.items()
        }
    if isinstance(value, list):
        return [_sanitize_event_value(item) for item in value]
    if isinstance(value, str):
        if normalized_key == "email":
            return mask_account(value)
        return redact_text(value)
    return value


def _http_error(response: httpx.Response, *, stage: str) -> RsApiError:
    status_code = response.status_code
    if status_code in {401, 403}:
        code = "rs_auth_failed"
    elif status_code >= 500:
        code = "upstream_error"
    else:
        code = "rs_import_failed"
    message = "RS 预检失败" if stage == "preflight" else "RS 批量导入失败"
    return RsApiError(
        code=code,
        stage=stage,
        retryable=status_code >= 500 and stage == "preflight",
        status_code=status_code,
        message=redact_text(message),
    )


def _network_error(*, stage: str) -> RsApiError:
    message = "RS 预检网络请求失败" if stage == "preflight" else "RS 批量导入网络请求失败"
    return RsApiError(
        code="network_error",
        stage=stage,
        retryable=stage == "preflight",
        status_code=0,
        message=redact_text(message),
    )


def _protocol_error(message: str) -> RsApiError:
    return RsApiError(
        code="invalid_rs_response",
        stage="batch_import",
        retryable=False,
        status_code=0,
        message=redact_text(message),
    )


class RsImportClient:
    def __init__(
        self,
        base_url: str,
        admin_key: str,
        *,
        transport: httpx.AsyncBaseTransport | None = None,
        timeout: float = 60,
    ):
        self.base_url = _normalize_base_url(base_url)
        self.client = httpx.AsyncClient(
            headers={"x-api-key": admin_key},
            transport=transport,
            timeout=timeout,
        )

    async def __aenter__(self) -> RsImportClient:
        return self

    async def __aexit__(self, *_args: object) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        await self.client.aclose()

    async def preflight(self) -> None:
        try:
            response = await self.client.get(
                self.base_url + "/credentials",
                headers={"accept": "application/json"},
            )
        except httpx.RequestError as error:
            raise _network_error(stage="preflight") from error
        if not 200 <= response.status_code < 300:
            raise _http_error(response, stage="preflight")

    async def batch_import(
        self,
        credentials: Iterable[dict[str, Any]],
        on_event: Callable[[dict[str, Any]], None],
        *,
        verify: bool = True,
        concurrency: int = 8,
    ) -> dict[str, Any]:
        credential_list = list(credentials)
        summary: dict[str, Any] | None = None
        buffer = ""
        try:
            async with self.client.stream(
                "POST",
                self.base_url + "/credentials/batch-import",
                headers={"accept": "text/event-stream"},
                json={
                    "credentials": credential_list,
                    "verify": verify,
                    "concurrency": concurrency,
                },
            ) as response:
                if not 200 <= response.status_code < 300:
                    raise _http_error(response, stage="batch_import")
                async for chunk in response.aiter_text():
                    buffer += chunk
                    try:
                        events, buffer = parse_sse(buffer)
                    except ValueError as error:
                        raise _protocol_error(str(error)) from error
                    for event in events:
                        sanitized = _sanitize_event_value(event)
                        if sanitized.get("status") == "summary":
                            candidate = sanitized.get("summary")
                            if isinstance(candidate, dict):
                                summary = candidate
                        else:
                            on_event(sanitized)
        except RsApiError as error:
            if error.status_code in {404, 405}:
                return await self._legacy_single_import(
                    credential_list,
                    on_event,
                )
            raise
        except httpx.RequestError as error:
            raise _network_error(stage="batch_import") from error

        if summary is None:
            raise _protocol_error("RS 导入响应缺少 summary")
        return summary

    async def _legacy_single_import(
        self,
        credentials: list[dict[str, Any]],
        on_event: Callable[[dict[str, Any]], None],
    ) -> dict[str, Any]:
        summary = {
            "total": len(credentials),
            "imported": 0,
            "verified": 0,
            "duplicate": 0,
            "failed": 0,
            "rolledBack": 0,
        }
        for index, credential in enumerate(credentials):
            event: dict[str, Any] = {
                "index": index,
                "credentialId": None,
                "email": credential.get("email"),
                "compatibilityMode": "legacy-single-add",
            }
            try:
                response = await self.client.post(
                    self.base_url + "/credentials",
                    headers={"accept": "application/json"},
                    json=credential,
                )
            except httpx.RequestError:
                event.update(status="failed", error="RS 单条导入网络请求失败")
                summary["failed"] += 1
            else:
                if response.status_code in {401, 403}:
                    raise _http_error(response, stage="batch_import")
                if 200 <= response.status_code < 300:
                    try:
                        data = response.json()
                    except ValueError:
                        data = {}
                    if isinstance(data, dict):
                        event["credentialId"] = data.get("credentialId")
                        event["email"] = data.get("email") or event["email"]
                    event["status"] = "imported"
                    summary["imported"] += 1
                elif response.status_code == 409:
                    event.update(status="duplicate", error="RS 已存在该凭据")
                    summary["duplicate"] += 1
                else:
                    event.update(
                        status="failed",
                        error=f"RS 单条导入失败（HTTP {response.status_code}）",
                    )
                    summary["failed"] += 1
            on_event(_sanitize_event_value(event))
        return summary
