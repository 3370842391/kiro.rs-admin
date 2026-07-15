from __future__ import annotations

import asyncio
import base64
import json
import secrets
from dataclasses import dataclass, field
from datetime import datetime, timezone
from email.utils import format_datetime
from typing import Any, Callable
from urllib.parse import parse_qs, quote, urlencode, urlsplit
from uuid import uuid4


IDC_SCOPES = [
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
]
DEVICE_GRANT = "urn:ietf:params:oauth:grant-type:device_code"
DEFAULT_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
    "AppleWebKit/537.36 (KHTML, like Gecko) "
    "Chrome/146.0.0.0 Safari/537.36"
)


@dataclass(slots=True)
class EnterpriseHttpError(Exception):
    code: str
    stage: str
    retryable: bool
    message: str

    def __str__(self) -> str:
        return self.message


@dataclass(slots=True, frozen=True)
class EnterpriseHttpSettings:
    start_url: str
    region: str


@dataclass(slots=True, frozen=True)
class EnterpriseHttpResult:
    directory_id: str
    client_id: str
    client_secret: str = field(repr=False)
    access_token: str = field(repr=False)
    refresh_token: str | None = field(default=None, repr=False)
    expires_in: int | None = None


@dataclass(slots=True, frozen=True)
class HttpResponse:
    status_code: int
    headers: dict[str, str]
    data: object


class CurlCffiTransport:
    """Chrome-impersonating HTTP transport; no browser process is created."""

    def __init__(self, *, timeout: float = 45):
        from curl_cffi.requests import AsyncSession

        self.session = AsyncSession(impersonate="chrome", timeout=timeout)

    @property
    def cookies(self):
        return self.session.cookies

    async def request(self, method: str, url: str, **kwargs) -> HttpResponse:
        response = await self.session.request(method, url, **kwargs)
        content_type = response.headers.get("content-type", "").lower()
        if "json" in content_type:
            try:
                data: object = response.json()
            except ValueError:
                data = response.text
        else:
            try:
                data = response.json()
            except ValueError:
                data = response.text
        return HttpResponse(
            response.status_code,
            {str(key).lower(): str(value) for key, value in response.headers.items()},
            data,
        )

    async def close(self) -> None:
        await self.session.close()


class EnterpriseHttpClient:
    def __init__(
        self,
        transport,
        *,
        vault,
        fingerprint: Callable[..., str] | None = None,
        password_encryptor: Callable[..., str] | None = None,
        app_js_config_loader: Callable[..., object] | None = None,
        event_sink: Callable[[dict[str, Any]], None] | None = None,
    ):
        self.transport = transport
        self.vault = vault
        self._custom_fingerprint = fingerprint is not None
        self.fingerprint = fingerprint or self._default_fingerprint
        self.password_encryptor = password_encryptor or self._default_encryptor
        self.app_js_config_loader = app_js_config_loader
        self.event_sink = event_sink
        self.sleep = asyncio.sleep
        self._workflow_handle = ""
        self._visitor_id = self._new_visitor_id()
        self._account = ""
        self._directory_id = ""
        self._region = ""
        self._fingerprint_identity = None
        self._fingerprint_context = None
        self._fingerprint_config = None
        if not self._custom_fingerprint:
            self._initialize_fingerprint()

    def _emit(self, stage: str) -> None:
        if self.event_sink is not None:
            self.event_sink({"kind": "browser_stage", "stage": stage})

    async def login(
        self,
        account: str,
        password: str,
        settings: EnterpriseHttpSettings,
    ) -> EnterpriseHttpResult:
        self._account = account
        self._region = self._validate_region(settings.region)
        self._directory_id = self._directory_from_start_url(settings.start_url)
        self._workflow_handle = ""
        self._visitor_id = self._new_visitor_id()
        scope = f"{self._region}/{self._directory_id}"
        try:
            unresolved = self.vault.unresolved(account, scope=scope)
        except AttributeError:
            unresolved = None
        login_password = (
            unresolved.password if unresolved is not None else password
        )

        await self._refresh_fingerprint_config()
        client_id, client_secret, device_code, user_code = await self._start_device(
            settings.start_url
        )
        used_fallback = False
        try:
            redirect, password_changed, csrf = await self._signin_attempt(
                account, login_password
            )
        except EnterpriseHttpError as error:
            can_retry_original = (
                unresolved is not None
                and login_password != password
                and error.code in {"http_error", "unsupported_signin_step"}
                and error.stage in {"signin_execute", "password"}
            )
            if not can_retry_original:
                raise
            used_fallback = True
            self._workflow_handle = ""
            self._visitor_id = self._new_visitor_id()
            redirect, password_changed, csrf = await self._signin_attempt(
                account, password
            )
        if unresolved is not None and not password_changed:
            record_id = getattr(unresolved, "record_id", None) or getattr(
                unresolved, "operation_id", None
            )
            if isinstance(record_id, str):
                self._vault_transition(
                    "rejected" if used_fallback else "confirmed",
                    record_id,
                    "candidate_not_active" if used_fallback else None,
                )
        auth_code, state = self._redirect_values(redirect)
        sso_token = await self._exchange_sso(auth_code, state, csrf)
        await self._associate_device(user_code, sso_token)
        token = await self._poll_token(client_id, client_secret, device_code)
        self._emit("complete")
        return EnterpriseHttpResult(
            directory_id=self._directory_id,
            client_id=client_id,
            client_secret=client_secret,
            access_token=self._required_string(token, "accessToken", "oidc_token"),
            refresh_token=self._optional_string(token, "refreshToken", "oidc_token"),
            expires_in=self._optional_int(token, "expiresIn", "oidc_token"),
        )

    async def _signin_attempt(
        self, account: str, password: str
    ) -> tuple[str, bool, str]:
        csrf = await self._portal_init()
        await self._d2c_init()
        await self._workflow_init()
        encryption_context = await self._submit_username(account)
        redirect, password_changed = await self._submit_password(
            account, password, encryption_context
        )
        return redirect, password_changed, csrf

    async def _start_device(self, start_url: str) -> tuple[str, str, str, str]:
        self._emit("oidc_register")
        base = f"https://oidc.{self._region}.amazonaws.com"
        registered = await self._json_request(
            "POST",
            base + "/client/register",
            stage="oidc_register",
            json={
                "clientName": "Amazon Q Developer for command line",
                "clientType": "public",
                "scopes": IDC_SCOPES,
                "grantTypes": [DEVICE_GRANT, "refresh_token"],
                "issuerUrl": start_url,
            },
        )
        client_id = self._required_string(registered, "clientId", "oidc_register")
        client_secret = self._required_string(
            registered, "clientSecret", "oidc_register"
        )
        self._emit("device_authorization")
        started = await self._json_request(
            "POST",
            base + "/device_authorization",
            stage="device_authorization",
            json={
                "clientId": client_id,
                "clientSecret": client_secret,
                "startUrl": start_url,
            },
        )
        return (
            client_id,
            client_secret,
            self._required_string(started, "deviceCode", "device_authorization"),
            self._required_string(started, "userCode", "device_authorization"),
        )

    async def _portal_init(self) -> str:
        self._emit("portal_init")
        self._set_cookie("awsccc", self._awsccc())
        portal = f"https://portal.sso.{self._region}.amazonaws.com/login"
        start = f"https://{self._directory_id}.awsapps.com/start"
        url = portal + "?" + urlencode(
            {
                "directory_id": self._directory_id,
                "redirect_url": start + "/#/",
            }
        )
        data = await self._json_request(
            "GET",
            url,
            stage="portal_init",
            headers={
                "Accept": "application/json, text/plain, */*",
                "Origin": f"https://{self._directory_id}.awsapps.com",
                "Referer": start + "/",
                "User-Agent": self._user_agent(),
            },
        )
        redirect = self._required_string(data, "redirectUrl", "portal_init")
        self._workflow_handle = self._query_value(redirect, "workflowStateHandle")
        csrf = self._required_string(data, "csrfToken", "portal_init")
        self._set_cookie("loginCsrfToken", csrf)
        if not self._workflow_handle:
            raise self._invalid_response("portal_init", "缺少 workflowStateHandle")
        return csrf

    async def _d2c_init(self) -> None:
        login_url = self._login_url()
        old_token = self._get_cookie("awsd2c-token")
        data = await self._json_request(
            "POST",
            "https://vs.aws.amazon.com/token",
            stage="d2c",
            headers={
                "Accept": "*/*",
                "Content-Type": "application/json",
                "Origin": self._signin_base(),
                "Referer": login_url,
                "User-Agent": self._user_agent(),
            },
            json={"token": old_token} if old_token else {},
        )
        token = self._required_string(data, "token", "d2c")
        self._set_cookie("awsd2c-token", token)
        self._set_cookie("awsd2c-token-c", token)
        try:
            middle = token.split(".")[1]
            middle += "=" * (-len(middle) % 4)
            payload = json.loads(base64.urlsafe_b64decode(middle))
            if isinstance(payload.get("vid"), str) and payload["vid"]:
                self._visitor_id = payload["vid"]
        except (IndexError, ValueError, TypeError, json.JSONDecodeError):
            pass

    async def _workflow_init(self) -> None:
        self._emit("workflow_init")
        data = await self._execute(
            "",
            [{"input_type": "FingerPrintRequestInput", "fingerPrint": self._fp("first_load")}],
            action_id=None,
            include_visitor=False,
        )
        self._update_handle(data)
        if data.get("stepId") == "start":
            data = await self._execute(
                "start",
                [{"input_type": "FingerPrintRequestInput", "fingerPrint": self._fp("PageLoad")}],
                action_id=None,
                include_visitor=False,
            )
            self._update_handle(data)

    async def _submit_username(self, account: str) -> dict[str, object]:
        self._emit("username")
        data = await self._execute(
            "get-identity-user",
            [
                {"input_type": "UserRequestInput", "username": account},
                {"input_type": "ApplicationTypeRequestInput", "applicationType": "SSO"},
                {
                    "input_type": "UserEventRequestInput",
                    "directoryId": self._directory_id,
                    "userName": account,
                    "userEvents": [
                        {
                            "input_type": "UserEvent",
                            "eventType": "PAGE_SUBMIT",
                            "pageName": "IDENTIFICATION",
                            "timeSpentOnPage": 5000 + secrets.randbelow(3001),
                        }
                    ],
                },
                {"input_type": "FingerPrintRequestInput", "fingerPrint": self._fp("PageSubmit", account)},
            ],
            action_id="SUBMIT",
        )
        self._update_handle(data)
        response_data = data.get("workflowResponseData")
        if not isinstance(response_data, dict):
            raise self._invalid_response("username", "缺少密码加密上下文")
        context = response_data.get("encryptionContextResponse")
        if not isinstance(context, dict) or not isinstance(context.get("publicKey"), dict):
            raise self._invalid_response("username", "缺少密码加密公钥")
        return context

    async def _submit_password(
        self,
        account: str,
        password: str,
        context: dict[str, object],
    ) -> tuple[str, bool]:
        self._emit("password")
        encrypted = self._encrypt(password, context)
        data = await self._execute(
            "get-password",
            [
                {
                    "input_type": "PasswordRequestInput",
                    "password": encrypted,
                    "passwordString": None,
                    "successfullyEncrypted": "SUCCESSFUL",
                    "errorLog": None,
                },
                {"input_type": "UserPreferencesRequestInput"},
                {
                    "input_type": "UserEventRequestInput",
                    "directoryId": self._directory_id,
                    "userName": account,
                    "userEvents": [
                        {
                            "input_type": "UserEvent",
                            "eventType": "PAGE_SUBMIT",
                            "pageName": "AUTHENTICATION",
                            "timeSpentOnPage": 3000 + secrets.randbelow(3001),
                        }
                    ],
                },
                {"input_type": "UserRequestInput", "username": account},
                {"input_type": "FingerPrintRequestInput", "fingerPrint": self._fp("PageSubmit")},
            ],
            action_id="SUBMIT",
        )
        self._update_handle(data)
        redirect = self._redirect_url(data)
        if redirect:
            return redirect, False
        if data.get("stepId") != "get-new-password-for-change-password":
            raise EnterpriseHttpError(
                "unsupported_signin_step",
                "password",
                False,
                "密码提交后进入未支持的认证步骤",
            )
        return await self._change_password(account, context), True

    async def _change_password(
        self,
        account: str,
        context: dict[str, object],
    ) -> str:
        self._emit("password_reset")
        try:
            attempt = self.vault.prepare(
                account,
                scope=f"{self._region}/{self._directory_id}",
            )
        except Exception as error:
            raise EnterpriseHttpError(
                "password_vault_failed",
                "password_reset",
                False,
                "新密码未能可靠保存，已禁止发送改密请求",
            ) from error
        operation_id = getattr(attempt, "operation_id", None) or getattr(
            attempt, "record_id", None
        )
        new_password = getattr(attempt, "password", None)
        if not isinstance(operation_id, str) or not isinstance(new_password, str):
            raise EnterpriseHttpError(
                "password_vault_failed",
                "password_reset",
                False,
                "密码保险库返回无效记录，已禁止发送改密请求",
            )
        encrypted = self._encrypt(new_password, context)
        payload = self._execute_payload(
            "get-new-password-for-change-password",
            [
                {
                    "input_type": "UpdatePasswordRequestInput",
                    "newPassword": encrypted,
                    "successfullyEncrypted": "SUCCESSFUL",
                    "errorLog": None,
                },
                {"input_type": "UserRequestInput", "username": account},
                {"input_type": "FingerPrintRequestInput", "fingerPrint": self._fp("PageSubmit")},
            ],
            action_id="SUBMIT",
            include_visitor=True,
        )
        try:
            response = await self.transport.request(
                "POST",
                self._execute_url(),
                headers=self._signin_headers(),
                json=payload,
            )
        except Exception as error:
            self._vault_transition("uncertain", operation_id, "network_result_unknown")
            raise EnterpriseHttpError(
                "password_change_uncertain",
                "password_reset",
                False,
                "改密请求结果未知；已保留生成密码，禁止自动更换候选密码",
            ) from error
        if response.status_code != 200:
            self._vault_transition("rejected", operation_id, f"http_{response.status_code}")
            raise EnterpriseHttpError(
                "password_change_rejected",
                "password_reset",
                False,
                "AWS 明确拒绝新密码",
            )
        # HTTP 200 is the password-change commit point.  Persist the confirmed
        # state before any later authorization request can fail.
        self._vault_transition("confirmed", operation_id, None)
        data = self._object(response.data, "password_reset")
        self._update_handle(data)
        redirect = self._redirect_url(data)
        if not redirect:
            data = await self._execute(
                "",
                [
                    {"input_type": "UserRequestInput", "username": account},
                    {"input_type": "FingerPrintRequestInput", "fingerPrint": self._fp("PageLoad")},
                ],
                action_id=None,
            )
            self._update_handle(data)
            redirect = self._redirect_url(data)
        if not redirect:
            raise EnterpriseHttpError(
                "password_change_completed_without_redirect",
                "password_reset",
                False,
                "AWS 已完成改密但未返回授权重定向；新密码已确认保存在保险库",
            )
        return redirect

    async def _exchange_sso(self, auth_code: str, state: str, csrf: str) -> str:
        self._emit("sso_token")
        origin = f"https://{self._directory_id}.awsapps.com"
        url = f"https://portal.sso.{self._region}.amazonaws.com/auth/sso-token"
        headers = {
            "Accept": "application/json, text/plain, */*",
            "Content-Type": "application/x-www-form-urlencoded",
            "Origin": origin,
            "Referer": origin + "/",
            "User-Agent": self._user_agent(),
            "sec-ch-ua": self._sec_ch_ua(),
            "sec-ch-ua-mobile": "?0",
            "sec-ch-ua-platform": '"Windows"',
            "x-amz-sso-csrf-token": csrf,
        }
        form = {
            "authCode": auth_code,
            "state": state,
            "orgId": self._directory_id,
        }
        for attempt in range(5):
            try:
                response = await self.transport.request(
                    "POST", url, headers=headers, data=form
                )
            except Exception as error:
                raise EnterpriseHttpError(
                    "network_error", "sso_token", True, "SSO token 网络请求失败"
                ) from error
            if response.status_code < 200 or response.status_code >= 300:
                raise EnterpriseHttpError(
                    "sso_token_failed",
                    "sso_token",
                    response.status_code >= 500 or response.status_code == 429,
                    f"SSO token 请求失败（HTTP {response.status_code}）",
                )
            data = self._object(response.data, "sso_token")
            token = data.get("token")
            if isinstance(token, str) and token:
                return token
            message = data.get("errorMessage")
            if (
                isinstance(message, str)
                and "not authorized" in message.casefold()
                and attempt < 4
            ):
                await self.sleep(3)
                continue
            break
        raise EnterpriseHttpError(
            "sso_token_failed", "sso_token", False, "企业 SSO token 获取失败"
        )

    async def _associate_device(self, user_code: str, sso_token: str) -> None:
        base = f"https://oidc.{self._region}.amazonaws.com/device_authorization"
        accepted = await self._json_request(
            "POST",
            base + "/accept_user_code",
            stage="device_accept",
            json={"userCode": user_code, "userSessionId": sso_token},
        )
        context = accepted.get("deviceContext")
        if not isinstance(context, (str, dict)) or not context:
            raise self._invalid_response("device_accept", "缺少 deviceContext")
        try:
            response = await self.transport.request(
                "POST",
                base + "/associate_token",
                json={"deviceContext": context, "userSessionId": sso_token},
            )
        except Exception as error:
            raise EnterpriseHttpError(
                "network_error",
                "device_associate",
                True,
                "设备 token 关联网络请求失败",
            ) from error
        if response.status_code < 200 or response.status_code >= 300:
            raise EnterpriseHttpError(
                "device_associate_failed",
                "device_associate",
                response.status_code >= 500 or response.status_code == 429,
                f"设备 token 关联失败（HTTP {response.status_code}）",
            )

    async def _poll_token(
        self,
        client_id: str,
        client_secret: str,
        device_code: str,
    ) -> dict[str, object]:
        url = f"https://oidc.{self._region}.amazonaws.com/token"
        interval = 2.0
        payload = {
            "clientId": client_id,
            "clientSecret": client_secret,
            "deviceCode": device_code,
            "grantType": DEVICE_GRANT,
        }
        for _attempt in range(30):
            try:
                response = await self.transport.request("POST", url, json=payload)
            except Exception as error:
                raise EnterpriseHttpError(
                    "network_error", "oidc_token", True, "OIDC token 网络请求失败"
                ) from error
            data = self._object(response.data, "oidc_token")
            if 200 <= response.status_code < 300:
                return data
            error_code = data.get("error")
            if error_code == "authorization_pending":
                await self.sleep(interval)
                continue
            if error_code == "slow_down":
                interval += 5
                await self.sleep(interval)
                continue
            raise EnterpriseHttpError(
                "oidc_token_failed",
                "oidc_token",
                response.status_code >= 500 or response.status_code == 429,
                f"OIDC token 请求失败（HTTP {response.status_code}）",
            )
        raise EnterpriseHttpError(
            "oidc_token_timeout", "oidc_token", False, "OIDC token 轮询超时"
        )

    async def _execute(
        self,
        step_id: str,
        inputs: list[dict[str, object]],
        *,
        action_id: str | None,
        include_visitor: bool = True,
    ) -> dict[str, object]:
        return await self._json_request(
            "POST",
            self._execute_url(),
            stage="signin_execute",
            headers=self._signin_headers(),
            json=self._execute_payload(
                step_id,
                inputs,
                action_id=action_id,
                include_visitor=include_visitor,
            ),
        )

    def _execute_payload(
        self,
        step_id: str,
        inputs: list[dict[str, object]],
        *,
        action_id: str | None,
        include_visitor: bool,
    ) -> dict[str, object]:
        request_id = str(uuid4())
        payload: dict[str, object] = {
            "stepId": step_id,
            "workflowStateHandle": self._workflow_handle,
            "inputs": inputs,
            "requestId": request_id,
        }
        if action_id is not None:
            payload["actionId"] = action_id
        if include_visitor:
            payload["visitorId"] = self._visitor_id
        return payload

    async def _json_request(
        self,
        method: str,
        url: str,
        *,
        stage: str,
        **kwargs,
    ) -> dict[str, object]:
        try:
            response = await self.transport.request(method, url, **kwargs)
        except Exception as error:
            raise EnterpriseHttpError(
                "network_error", stage, True, f"{stage} 网络请求失败"
            ) from error
        if response.status_code < 200 or response.status_code >= 300:
            raise EnterpriseHttpError(
                "http_error",
                stage,
                response.status_code >= 500 or response.status_code == 429,
                f"{stage} 请求失败（HTTP {response.status_code}）",
            )
        return self._object(response.data, stage)

    def _fp(self, event_type: str, account: str = "") -> str:
        try:
            return self.fingerprint(
                "signin",
                event_type,
                len(account),
                account,
                self._login_url(),
            )
        except TypeError:
            return self.fingerprint("signin", event_type, len(account), account)

    def _encrypt(self, password: str, context: dict[str, object]) -> str:
        public_key = context.get("publicKey")
        if not isinstance(public_key, dict):
            raise self._invalid_response("password", "密码公钥无效")
        issuer = context.get("issuer") or "signin"
        audience = context.get("audience") or "AWSPasswordService"
        region = context.get("region") or self._region
        if not all(isinstance(value, str) for value in (issuer, audience, region)):
            raise self._invalid_response("password", "密码加密上下文无效")
        return self.password_encryptor(
            password,
            public_key,
            issuer,
            audience,
            region,
        )

    @staticmethod
    def _default_encryptor(*args, **kwargs) -> str:
        from .aws_jwe import encrypt_password

        return encrypt_password(*args, **kwargs)

    def _initialize_fingerprint(self) -> None:
        from .aws_fingerprint import (
            FALLBACK_CONFIG,
            new_fingerprint_context,
            random_identity,
        )

        self._fingerprint_identity = random_identity()
        self._fingerprint_context = new_fingerprint_context(
            self._fingerprint_identity
        )
        self._fingerprint_config = FALLBACK_CONFIG

    async def _refresh_fingerprint_config(self) -> None:
        if self._custom_fingerprint:
            return
        from .aws_fingerprint import extract_app_js_config

        try:
            response = await self.transport.request(
                "GET",
                "https://us-east-1.signin.aws/assets/js/app.js",
                headers={"Accept": "*/*", "User-Agent": self._user_agent()},
            )
            if response.status_code == 200 and isinstance(response.data, str):
                loader = self.app_js_config_loader or extract_app_js_config
                self._fingerprint_config = loader(response.data)
        except Exception:
            # The collector ships a known fallback; signin responses remain the
            # authority and will reject an obsolete key without exposing data.
            return

    def _default_fingerprint(
        self,
        page_type: str,
        event_type: str,
        email_length: int,
        email: str,
        location_url: str,
    ) -> str:
        from .aws_fingerprint import generate_fingerprint

        if (
            self._fingerprint_identity is None
            or self._fingerprint_context is None
            or self._fingerprint_config is None
        ):
            raise RuntimeError("AWS fingerprint generator is not initialized")
        return generate_fingerprint(
            identity=self._fingerprint_identity,
            location_url=location_url,
            referrer="https://view.awsapps.com/",
            context=self._fingerprint_context,
            page_type=page_type,
            event_type=event_type,
            time_on_page=0,
            email_length=email_length,
            email=email,
            config=self._fingerprint_config,
        )

    def _vault_transition(
        self, status: str, operation_id: str, reason: str | None
    ) -> None:
        method = getattr(self.vault, f"mark_{status}", None)
        if callable(method):
            if reason is None:
                method(operation_id)
            else:
                method(operation_id, reason)
            return
        transition = getattr(self.vault, "transition", None)
        if not callable(transition):
            raise RuntimeError("password vault does not support transitions")
        try:
            from .password_vault import PasswordStatus

            target = PasswordStatus(status)
        except (ImportError, ValueError):
            target = status
        transition(operation_id, target)

    def _signin_headers(self) -> dict[str, str]:
        return {
            "Accept": "application/json, text/plain, */*",
            "Content-Type": "application/json",
            "Origin": self._signin_base(),
            "Referer": self._login_url(),
            "User-Agent": self._user_agent(),
            "sec-ch-ua": self._sec_ch_ua(),
            "sec-ch-ua-mobile": "?0",
            "sec-ch-ua-platform": '"Windows"',
            "x-amzn-requestid": str(uuid4()),
            "x-amz-date": format_datetime(datetime.now(timezone.utc), usegmt=True),
        }

    def _signin_base(self) -> str:
        return f"https://{self._region}.signin.aws"

    def _user_agent(self) -> str:
        value = getattr(self._fingerprint_identity, "user_agent", None)
        return value if isinstance(value, str) and value else DEFAULT_UA

    def _sec_ch_ua(self) -> str:
        version = getattr(self._fingerprint_identity, "chrome_version", "146")
        major = str(version).split(".", 1)[0]
        return (
            f'"Chromium";v="{major}", "Not/A)Brand";v="24", '
            f'"Google Chrome";v="{major}"'
        )

    def _login_url(self) -> str:
        return (
            f"{self._signin_base()}/platform/{self._directory_id}/login"
            f"?workflowStateHandle={quote(self._workflow_handle)}"
        )

    def _execute_url(self) -> str:
        return f"{self._signin_base()}/platform/{self._directory_id}/api/execute"

    def _update_handle(self, data: dict[str, object]) -> None:
        handle = data.get("workflowStateHandle")
        if isinstance(handle, str) and handle:
            self._workflow_handle = handle

    @staticmethod
    def _redirect_url(data: dict[str, object]) -> str:
        redirect = data.get("redirect")
        if isinstance(redirect, dict) and isinstance(redirect.get("url"), str):
            return redirect["url"]
        return ""

    def _redirect_values(self, redirect: str) -> tuple[str, str]:
        auth_code = self._query_value(redirect, "workflowResultHandle")
        state = self._query_value(redirect, "state")
        if not auth_code or not state:
            raise self._invalid_response("signin_redirect", "授权重定向参数不完整")
        return auth_code, state

    @staticmethod
    def _query_value(url: str, name: str) -> str:
        values = parse_qs(urlsplit(url).query).get(name, [])
        return values[0] if values else ""

    @staticmethod
    def _object(value: object, stage: str) -> dict[str, object]:
        if not isinstance(value, dict):
            raise EnterpriseHttpClient._invalid_response(stage, "响应不是 JSON 对象")
        return value

    @staticmethod
    def _required_string(data: dict[str, object], key: str, stage: str) -> str:
        value = data.get(key)
        if not isinstance(value, str) or not value:
            raise EnterpriseHttpClient._invalid_response(stage, f"缺少 {key}")
        return value

    @staticmethod
    def _optional_string(
        data: dict[str, object], key: str, stage: str
    ) -> str | None:
        value = data.get(key)
        if value is None:
            return None
        if not isinstance(value, str):
            raise EnterpriseHttpClient._invalid_response(stage, f"{key} 类型无效")
        return value

    @staticmethod
    def _optional_int(data: dict[str, object], key: str, stage: str) -> int | None:
        value = data.get(key)
        if value is None:
            return None
        if not isinstance(value, int) or isinstance(value, bool):
            raise EnterpriseHttpClient._invalid_response(stage, f"{key} 类型无效")
        return value

    @staticmethod
    def _invalid_response(stage: str, detail: str) -> EnterpriseHttpError:
        return EnterpriseHttpError(
            "invalid_aws_response", stage, False, f"AWS {stage} 响应无效：{detail}"
        )

    @staticmethod
    def _validate_region(region: str) -> str:
        region = region.strip().lower()
        if not region or any(ch not in "abcdefghijklmnopqrstuvwxyz0123456789-" for ch in region):
            raise EnterpriseHttpError("invalid_region", "config", False, "Region 格式无效")
        return region

    @staticmethod
    def _directory_from_start_url(start_url: str) -> str:
        parts = urlsplit(start_url.strip())
        if parts.scheme != "https" or not parts.hostname:
            raise EnterpriseHttpError(
                "invalid_start_url", "config", False, "企业 Start URL 必须是 HTTPS"
            )
        hostname = parts.hostname.lower()
        suffix = ".awsapps.com"
        if not hostname.endswith(suffix):
            raise EnterpriseHttpError(
                "invalid_start_url", "config", False, "企业 Start URL 域名无效"
            )
        directory_id = hostname[: -len(suffix)]
        if not directory_id.startswith("d-") or "." in directory_id:
            raise EnterpriseHttpError(
                "invalid_start_url", "config", False, "Start URL 缺少企业目录 ID"
            )
        return directory_id

    def _set_cookie(self, name: str, value: str) -> None:
        cookies = getattr(self.transport, "cookies", None)
        if cookies is None:
            return
        setter = getattr(cookies, "set", None)
        if callable(setter):
            setter(name, value)
        elif isinstance(cookies, dict):
            cookies[name] = value

    def _get_cookie(self, name: str) -> str | None:
        cookies = getattr(self.transport, "cookies", None)
        if cookies is None:
            return None
        getter = getattr(cookies, "get", None)
        if callable(getter):
            try:
                value = getter(name)
            except Exception:
                return None
            return value if isinstance(value, str) and value else None
        if isinstance(cookies, dict):
            value = cookies.get(name)
            return value if isinstance(value, str) and value else None
        return None

    @staticmethod
    def _new_visitor_id() -> str:
        value = str(uuid4())
        return value[:14] + "7" + value[15:]

    @staticmethod
    def _awsccc() -> str:
        payload = {
            "e": 1,
            "p": 1,
            "f": 1,
            "a": 1,
            "i": str(uuid4()),
            "v": "1",
        }
        return base64.b64encode(
            json.dumps(payload, separators=(",", ":")).encode()
        ).decode()
