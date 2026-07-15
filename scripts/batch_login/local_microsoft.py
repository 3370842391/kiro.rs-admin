from __future__ import annotations

import base64
import hashlib
import ipaddress
import json
import secrets
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from urllib.parse import parse_qs, urlencode, urlsplit

import httpx

from .credential_models import CredentialRecord
from .local_idc import LocalAuthError


SIGNIN_URL = "https://app.kiro.dev/signin"
REDIRECT_URI = "http://localhost:3128"
SOCIAL_TOKEN_URL = "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/token"
ALLOWED_IDP_SUFFIXES = (
    ".microsoftonline.com",
    ".microsoftonline.us",
    ".microsoftonline.cn",
)


@dataclass(slots=True, frozen=True)
class MicrosoftSession:
    state: str
    verifier: str = field(repr=False)
    signin_url: str
    region: str


@dataclass(slots=True, frozen=True)
class PortalCallback:
    kind: str
    code: str | None = field(default=None, repr=False)
    issuer_url: str | None = None
    client_id: str | None = None
    scopes: str = ""
    login_hint: str = field(default="", repr=False)


@dataclass(slots=True, frozen=True)
class MicrosoftToken:
    access_token: str = field(repr=False)
    refresh_token: str | None = field(default=None, repr=False)
    expires_in: int | None = None
    profile_arn: str | None = None


@dataclass(slots=True, frozen=True)
class ExternalLeg:
    state: str
    verifier: str = field(repr=False)
    authorize_url: str
    token_endpoint: str
    issuer_url: str
    client_id: str
    scopes: str
    redirect_uri: str


def random_urlsafe(size: int) -> str:
    return base64.urlsafe_b64encode(secrets.token_bytes(size)).rstrip(b"=").decode()


def pkce_challenge(verifier: str) -> str:
    digest = hashlib.sha256(verifier.encode("utf-8")).digest()
    return base64.urlsafe_b64encode(digest).rstrip(b"=").decode()


def validate_external_endpoint(raw_url: str) -> str:
    value = raw_url.strip()
    try:
        parts = urlsplit(value)
        _ = parts.port
    except ValueError as error:
        raise _unsafe_endpoint() from error
    host = (parts.hostname or "").casefold()
    if (
        parts.scheme != "https"
        or not host
        or parts.username is not None
        or parts.password is not None
    ):
        raise _unsafe_endpoint()
    try:
        ipaddress.ip_address(host)
    except ValueError:
        pass
    else:
        raise _unsafe_endpoint()
    if not any(host.endswith(suffix) for suffix in ALLOWED_IDP_SUFFIXES):
        raise _unsafe_endpoint()
    return value


def _unsafe_endpoint() -> LocalAuthError:
    return LocalAuthError(
        "unsafe_idp_endpoint",
        "microsoft_discovery",
        False,
        "外部身份端点不在 Microsoft HTTPS 白名单中",
    )


def _callback_values(raw_url: str) -> dict[str, str]:
    parts = urlsplit(raw_url.strip())
    if (
        parts.scheme != "http"
        or parts.hostname not in {"localhost", "127.0.0.1"}
        or parts.port != 3128
    ):
        raise LocalAuthError(
            "invalid_callback",
            "microsoft_callback",
            False,
            "登录回调地址无效",
        )
    values = parse_qs(parts.query, keep_blank_values=True)
    for key, items in parse_qs(parts.fragment, keep_blank_values=True).items():
        values.setdefault(key, items)
    return {key: items[0] for key, items in values.items() if items}


def parse_portal_callback(raw_url: str, expected_state: str) -> PortalCallback:
    values = _callback_values(raw_url)
    if values.get("state") != expected_state:
        raise LocalAuthError(
            "state_mismatch", "microsoft_callback", False, "OAuth state 不匹配"
        )
    if values.get("error"):
        raise LocalAuthError(
            "access_denied", "microsoft_callback", False, "Microsoft 授权失败"
        )
    if values.get("code"):
        return PortalCallback(kind="social", code=values["code"])
    issuer = values.get("issuer_url") or values.get("issuerUrl")
    client_id = values.get("client_id") or values.get("clientId")
    if issuer and client_id:
        return PortalCallback(
            kind="external_idp",
            issuer_url=issuer,
            client_id=client_id,
            scopes=values.get("scopes") or values.get("scope") or "",
            login_hint=values.get("login_hint") or values.get("loginHint") or "",
        )
    raise LocalAuthError(
        "invalid_callback",
        "microsoft_callback",
        False,
        "登录回调缺少授权信息",
    )


class MicrosoftProtocol:
    def __init__(self, http: httpx.AsyncClient):
        self.http = http

    parse_portal_callback = staticmethod(parse_portal_callback)

    @staticmethod
    def new_session(region: str) -> MicrosoftSession:
        verifier = random_urlsafe(96)
        state = random_urlsafe(32)
        query = urlencode(
            {
                "state": state,
                "code_challenge": pkce_challenge(verifier),
                "code_challenge_method": "S256",
                "redirect_uri": REDIRECT_URI,
                "redirect_from": "KiroIDE",
            }
        )
        return MicrosoftSession(
            state=state,
            verifier=verifier,
            signin_url=f"{SIGNIN_URL}?{query}",
            region=region or "us-east-1",
        )

    async def discover(self, issuer_url: str) -> tuple[str, str]:
        issuer = validate_external_endpoint(issuer_url)
        response = await self._request(
            "GET",
            issuer.rstrip("/") + "/.well-known/openid-configuration",
            stage="microsoft_discovery",
        )
        body = self._json_object(response, "microsoft_discovery")
        authorization = body.get("authorization_endpoint")
        token = body.get("token_endpoint")
        if not isinstance(authorization, str) or not isinstance(token, str):
            raise self._invalid_response("microsoft_discovery")
        return validate_external_endpoint(authorization), validate_external_endpoint(token)

    async def prepare_external(self, callback: PortalCallback) -> ExternalLeg:
        if callback.issuer_url is None or callback.client_id is None:
            raise LocalAuthError(
                "invalid_callback",
                "microsoft_callback",
                False,
                "外部身份描述符不完整",
            )
        authorization, token_endpoint = await self.discover(callback.issuer_url)
        verifier = random_urlsafe(96)
        state = random_urlsafe(32)
        redirect_uri = REDIRECT_URI + "/oauth/callback"
        params = {
            "client_id": callback.client_id,
            "response_type": "code",
            "redirect_uri": redirect_uri,
            "scope": callback.scopes,
            "code_challenge": pkce_challenge(verifier),
            "code_challenge_method": "S256",
            "response_mode": "query",
            "state": state,
        }
        if callback.login_hint:
            params["login_hint"] = callback.login_hint
        return ExternalLeg(
            state=state,
            verifier=verifier,
            authorize_url=authorization + "?" + urlencode(params),
            token_endpoint=token_endpoint,
            issuer_url=callback.issuer_url,
            client_id=callback.client_id,
            scopes=callback.scopes,
            redirect_uri=redirect_uri,
        )

    async def exchange_social(self, code: str, verifier: str) -> MicrosoftToken:
        response = await self._request(
            "POST",
            SOCIAL_TOKEN_URL,
            stage="social_token",
            json={
                "code": code,
                "code_verifier": verifier,
                "redirect_uri": REDIRECT_URI,
            },
        )
        body = self._json_object(response, "social_token")
        return self._camel_token(body, "social_token")

    async def exchange_external(
        self, leg: ExternalLeg, callback_url: str
    ) -> MicrosoftToken:
        values = _callback_values(callback_url)
        if values.get("state") != leg.state:
            raise LocalAuthError(
                "state_mismatch", "external_callback", False, "OAuth state 不匹配"
            )
        if values.get("error") or not values.get("code"):
            raise LocalAuthError(
                "access_denied", "external_callback", False, "Entra 授权失败"
            )
        response = await self._request(
            "POST",
            leg.token_endpoint,
            stage="external_token",
            data={
                "client_id": leg.client_id,
                "grant_type": "authorization_code",
                "code": values["code"],
                "redirect_uri": leg.redirect_uri,
                "code_verifier": leg.verifier,
                "scope": leg.scopes,
            },
        )
        body = self._json_object(response, "external_token")
        access = body.get("access_token")
        refresh = body.get("refresh_token")
        expires = body.get("expires_in")
        return self._validated_token(access, refresh, expires, None, "external_token")

    def social_record(
        self,
        input_email: str,
        region: str,
        token: MicrosoftToken,
        now: datetime,
    ) -> CredentialRecord:
        return CredentialRecord(
            email=email_from_jwt(token.access_token) or input_email,
            auth_method="social",
            provider="Microsoft",
            refresh_token=token.refresh_token,
            access_token=token.access_token,
            profile_arn=token.profile_arn,
            region=region,
            expires_at=_expires_at(now, token.expires_in),
        )

    def external_record(
        self,
        input_email: str,
        region: str,
        leg: ExternalLeg,
        token: MicrosoftToken,
        now: datetime,
    ) -> CredentialRecord:
        return CredentialRecord(
            email=email_from_jwt(token.access_token) or input_email,
            auth_method="external_idp",
            provider="Enterprise",
            refresh_token=token.refresh_token,
            access_token=token.access_token,
            client_id=leg.client_id,
            token_endpoint=leg.token_endpoint,
            issuer_url=leg.issuer_url,
            scopes=leg.scopes,
            region=region,
            expires_at=_expires_at(now, token.expires_in),
        )

    async def _request(self, method: str, url: str, *, stage: str, **kwargs):
        try:
            response = await self.http.request(method, url, **kwargs)
        except httpx.RequestError as error:
            raise LocalAuthError(
                "network_error", stage, True, "Microsoft 认证网络请求失败"
            ) from error
        if not response.is_success:
            raise LocalAuthError(
                "microsoft_request_failed",
                stage,
                response.status_code >= 500,
                "Microsoft 认证请求失败",
            )
        return response

    def _json_object(self, response: httpx.Response, stage: str):
        try:
            body = response.json()
        except ValueError as error:
            raise self._invalid_response(stage) from error
        if not isinstance(body, dict):
            raise self._invalid_response(stage)
        return body

    def _camel_token(self, body, stage):
        return self._validated_token(
            body.get("accessToken"),
            body.get("refreshToken"),
            body.get("expiresIn"),
            body.get("profileArn"),
            stage,
        )

    def _validated_token(self, access, refresh, expires, profile, stage):
        if not isinstance(access, str) or not access:
            raise self._invalid_response(stage)
        if refresh is not None and not isinstance(refresh, str):
            raise self._invalid_response(stage)
        if expires is not None and (
            not isinstance(expires, int) or isinstance(expires, bool)
        ):
            raise self._invalid_response(stage)
        if profile is not None and not isinstance(profile, str):
            raise self._invalid_response(stage)
        return MicrosoftToken(access, refresh, expires, profile)

    @staticmethod
    def _invalid_response(stage):
        return LocalAuthError(
            "invalid_microsoft_response",
            stage,
            False,
            "Microsoft 认证响应格式无效",
        )


def email_from_jwt(token: str) -> str:
    try:
        segment = token.split(".")[1]
        segment += "=" * (-len(segment) % 4)
        claims = json.loads(base64.urlsafe_b64decode(segment).decode("utf-8"))
    except (IndexError, ValueError, UnicodeDecodeError, json.JSONDecodeError):
        return ""
    if not isinstance(claims, dict):
        return ""
    for key in ("preferred_username", "email", "upn", "unique_name", "name"):
        value = claims.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    return ""


def _expires_at(now: datetime, expires_in: int | None) -> str | None:
    if expires_in is None:
        return None
    return (now + timedelta(seconds=expires_in)).isoformat().replace("+00:00", "Z")
