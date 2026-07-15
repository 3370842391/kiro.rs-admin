from __future__ import annotations

import asyncio
import re
import time
from dataclasses import dataclass, field
from urllib.parse import urlsplit

import httpx


IDC_SCOPES = [
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
]
REGION_RE = re.compile(r"^[a-z0-9-]+$")


@dataclass(slots=True)
class LocalAuthError(Exception):
    code: str
    stage: str
    retryable: bool
    message: str

    def __str__(self) -> str:
        return self.message


@dataclass(slots=True, frozen=True)
class IdcSession:
    region: str
    start_url: str
    client_id: str
    client_secret: str = field(repr=False)
    device_code: str = field(repr=False)
    user_code: str
    verification_url: str
    expires_at: float
    interval: float


@dataclass(slots=True, frozen=True)
class IdcToken:
    access_token: str = field(repr=False)
    refresh_token: str | None = field(default=None, repr=False)
    expires_in: int | None = None


class LocalIdcClient:
    def __init__(
        self,
        http: httpx.AsyncClient,
        *,
        sleep=asyncio.sleep,
        now=time.monotonic,
    ):
        self.http = http
        self.sleep = sleep
        self.now = now

    @staticmethod
    def endpoint(region: str) -> str:
        if not REGION_RE.fullmatch(region):
            raise LocalAuthError(
                "invalid_region", "config", False, "Region 格式无效"
            )
        return f"https://oidc.{region}.amazonaws.com"

    async def start(self, start_url: str, region: str) -> IdcSession:
        start_url = start_url.strip()
        start_parts = urlsplit(start_url)
        if (
            start_parts.scheme != "https"
            or not start_parts.hostname
            or start_parts.username is not None
            or start_parts.password is not None
            or start_parts.fragment
        ):
            raise LocalAuthError(
                "invalid_start_url",
                "config",
                False,
                "Start URL 必须是 HTTPS 地址",
            )
        base = self.endpoint(region)
        registered = await self._post(
            base + "/client/register",
            {
                "clientName": "kiro-rs",
                "clientType": "public",
                "scopes": IDC_SCOPES,
                "grantTypes": [
                    "urn:ietf:params:oauth:grant-type:device_code",
                    "refresh_token",
                ],
                "issuerUrl": start_url,
            },
            stage="idc_register",
            error_code="idc_register_failed",
            message="注册 IdC 客户端失败",
        )
        client_id = registered.get("clientId")
        client_secret = registered.get("clientSecret")
        if not isinstance(client_id, str) or not client_id:
            raise self._invalid_response("idc_register")
        if not isinstance(client_secret, str) or not client_secret:
            raise self._invalid_response("idc_register")

        started = await self._post(
            base + "/device_authorization",
            {
                "clientId": client_id,
                "clientSecret": client_secret,
                "startUrl": start_url,
            },
            stage="idc_start",
            error_code="idc_start_failed",
            message="发起设备授权失败",
        )
        device_code = started.get("deviceCode")
        user_code = started.get("userCode")
        verification_url = started.get("verificationUriComplete") or started.get(
            "verificationUri"
        )
        expires_in = started.get("expiresIn", 600)
        interval = started.get("interval", 5)
        if not isinstance(device_code, str) or not device_code:
            raise self._invalid_response("idc_start")
        if not isinstance(user_code, str) or not user_code:
            raise self._invalid_response("idc_start")
        if not isinstance(verification_url, str) or not verification_url:
            raise self._invalid_response("idc_start")
        if not _is_number(expires_in) or expires_in <= 0:
            raise self._invalid_response("idc_start")
        if not _is_number(interval) or interval < 0:
            raise self._invalid_response("idc_start")
        return IdcSession(
            region=region,
            start_url=start_url,
            client_id=client_id,
            client_secret=client_secret,
            device_code=device_code,
            user_code=user_code,
            verification_url=verification_url,
            expires_at=self.now() + float(expires_in),
            interval=float(interval),
        )

    async def poll(self, session: IdcSession) -> IdcToken:
        interval = max(session.interval, 0.2)
        while self.now() < session.expires_at:
            try:
                response = await self.http.post(
                    self.endpoint(session.region) + "/token",
                    json={
                        "clientId": session.client_id,
                        "clientSecret": session.client_secret,
                        "grantType": "urn:ietf:params:oauth:grant-type:device_code",
                        "deviceCode": session.device_code,
                    },
                )
            except httpx.RequestError as error:
                raise LocalAuthError(
                    "network_error",
                    "idc_poll",
                    True,
                    "IdC token 网络请求失败",
                ) from error

            if response.is_success:
                body = self._json_object(response, "idc_poll")
                access_token = body.get("accessToken")
                refresh_token = body.get("refreshToken")
                expires_in = body.get("expiresIn")
                if not isinstance(access_token, str) or not access_token:
                    raise self._invalid_response("idc_poll")
                if refresh_token is not None and not isinstance(refresh_token, str):
                    raise self._invalid_response("idc_poll")
                if expires_in is not None and (
                    not isinstance(expires_in, int) or isinstance(expires_in, bool)
                ):
                    raise self._invalid_response("idc_poll")
                return IdcToken(access_token, refresh_token, expires_in)

            body = self._json_object(response, "idc_poll", required=False)
            error_code = body.get("error")
            if error_code == "authorization_pending":
                await self.sleep(interval)
                continue
            if error_code == "slow_down":
                interval += 5
                await self.sleep(interval)
                continue
            if error_code == "expired_token":
                raise LocalAuthError(
                    "session_expired",
                    "idc_poll",
                    False,
                    "设备授权已过期",
                )
            if error_code == "access_denied":
                raise LocalAuthError(
                    "access_denied", "idc_poll", False, "用户拒绝授权"
                )
            raise LocalAuthError(
                "idc_token_failed",
                "idc_poll",
                response.status_code >= 500,
                "IdC token 请求失败",
            )
        raise LocalAuthError(
            "session_expired", "idc_poll", False, "设备授权已过期"
        )

    async def _post(
        self,
        url: str,
        payload: dict[str, object],
        *,
        stage: str,
        error_code: str,
        message: str,
    ) -> dict[str, object]:
        try:
            response = await self.http.post(url, json=payload)
        except httpx.RequestError as error:
            raise LocalAuthError(
                "network_error", stage, True, f"{message}：网络请求失败"
            ) from error
        if not response.is_success:
            raise LocalAuthError(
                error_code, stage, response.status_code >= 500, message
            )
        return self._json_object(response, stage)

    def _json_object(
        self,
        response: httpx.Response,
        stage: str,
        *,
        required: bool = True,
    ) -> dict[str, object]:
        try:
            body = response.json()
        except ValueError as error:
            if not required:
                return {}
            raise self._invalid_response(stage) from error
        if not isinstance(body, dict):
            if not required:
                return {}
            raise self._invalid_response(stage)
        return body

    @staticmethod
    def _invalid_response(stage: str) -> LocalAuthError:
        return LocalAuthError(
            "invalid_idc_response", stage, False, "IdC 响应格式无效"
        )


def _is_number(value: object) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)
