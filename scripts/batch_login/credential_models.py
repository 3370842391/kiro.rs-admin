from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass(slots=True)
class CredentialRecord:
    email: str
    auth_method: str
    provider: str
    refresh_token: str | None = field(default=None, repr=False)
    access_token: str | None = field(default=None, repr=False)
    profile_arn: str | None = None
    expires_at: str | None = None
    client_id: str | None = None
    client_secret: str | None = field(default=None, repr=False)
    start_url: str | None = None
    token_endpoint: str | None = None
    issuer_url: str | None = None
    scopes: str | None = None
    region: str | None = None
    kiro_api_key: str | None = field(default=None, repr=False)
    priority: int = 0
    rpm_limit: int = 10
    source_channel: str = "batch-login-gui"

    def dedupe_key(self) -> tuple[str, str, str]:
        scope = (self.start_url or self.issuer_url or "").casefold().rstrip("/")
        return self.auth_method.casefold(), self.email.casefold(), scope

    def as_add_request(self) -> dict[str, Any]:
        payload = {
            "email": self.email,
            "authMethod": self.auth_method,
            "provider": self.provider,
            "refreshToken": self.refresh_token,
            "accessToken": self.access_token,
            "profileArn": self.profile_arn,
            "expiresAt": self.expires_at,
            "clientId": self.client_id,
            "clientSecret": self.client_secret,
            "startUrl": self.start_url,
            "tokenEndpoint": self.token_endpoint,
            "issuerUrl": self.issuer_url,
            "scopes": self.scopes,
            "region": self.region,
            "kiroApiKey": self.kiro_api_key,
            "priority": self.priority,
            "rpmLimit": self.rpm_limit,
            "sourceChannel": self.source_channel,
        }
        return {key: value for key, value in payload.items() if value is not None}

    @classmethod
    def from_add_request(cls, payload: dict[str, Any]) -> CredentialRecord:
        return cls(
            email=_required_string(payload.get("email"), field_name="email"),
            auth_method=_required_string(
                payload.get("authMethod", "social"), field_name="authMethod"
            ),
            provider=_required_string(
                payload.get("provider", ""), field_name="provider"
            ),
            refresh_token=_optional_string(payload.get("refreshToken")),
            access_token=_optional_string(payload.get("accessToken")),
            profile_arn=_optional_string(payload.get("profileArn")),
            expires_at=_optional_string(payload.get("expiresAt")),
            client_id=_optional_string(payload.get("clientId")),
            client_secret=_optional_string(payload.get("clientSecret")),
            start_url=_optional_string(payload.get("startUrl")),
            token_endpoint=_optional_string(payload.get("tokenEndpoint")),
            issuer_url=_optional_string(payload.get("issuerUrl")),
            scopes=_optional_string(payload.get("scopes")),
            region=_optional_string(payload.get("region")),
            kiro_api_key=_optional_string(payload.get("kiroApiKey")),
            priority=_integer(payload.get("priority"), default=0),
            rpm_limit=_integer(payload.get("rpmLimit"), default=10),
            source_channel=_required_string(
                payload.get("sourceChannel", "batch-login-gui"),
                field_name="sourceChannel",
            ),
        )


def _required_string(value: Any, *, field_name: str) -> str:
    if not isinstance(value, str):
        raise ValueError(f"{field_name} 字段类型无效")
    return value


def _optional_string(value: Any) -> str | None:
    if value is None:
        return None
    if not isinstance(value, str):
        raise ValueError("凭据字段类型无效")
    return value


def _integer(value: Any, *, default: int) -> int:
    if value is None:
        return default
    if not isinstance(value, int) or isinstance(value, bool):
        raise ValueError("凭据整数字段类型无效")
    return value
