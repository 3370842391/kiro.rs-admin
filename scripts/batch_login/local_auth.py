from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import Callable

from .credential_models import CredentialRecord
from .enterprise_http import EnterpriseHttpSettings
from .local_idc import LocalAuthError
from .models import AccountEntry


@dataclass(slots=True, frozen=True)
class EnterpriseSettings:
    start_url: str
    region: str


@dataclass(slots=True, frozen=True)
class MicrosoftSettings:
    region: str = "us-east-1"


class LocalEnterpriseAuth:
    def __init__(self, protocol, *, now=lambda: datetime.now(timezone.utc)):
        self.protocol = protocol
        self.now = now

    async def login(self, entry: AccountEntry, settings: EnterpriseSettings) -> CredentialRecord:
        result = await self.protocol.login(
            entry.account,
            entry.password,
            EnterpriseHttpSettings(settings.start_url, settings.region),
        )
        expires = (
            None
            if result.expires_in is None
            else (self.now() + timedelta(seconds=result.expires_in))
            .isoformat()
            .replace("+00:00", "Z")
        )
        return CredentialRecord(
            email=entry.account,
            auth_method="idc",
            provider="Enterprise",
            refresh_token=result.refresh_token,
            access_token=result.access_token,
            client_id=result.client_id,
            client_secret=result.client_secret,
            start_url=settings.start_url,
            region=settings.region,
            expires_at=expires,
        )


class IsolatedEnterpriseAuth:
    """Creates a fresh HTTP transport and protocol for every enterprise account."""

    def __init__(
        self,
        transport_factory: Callable[[], object],
        protocol_factory: Callable[[object], object],
        *,
        now=lambda: datetime.now(timezone.utc),
    ):
        self.transport_factory = transport_factory
        self.protocol_factory = protocol_factory
        self.now = now

    async def login(
        self,
        entry: AccountEntry,
        settings: EnterpriseSettings,
    ) -> CredentialRecord:
        transport = self.transport_factory()
        try:
            protocol = self.protocol_factory(transport)
            return await LocalEnterpriseAuth(protocol, now=self.now).login(
                entry,
                settings,
            )
        finally:
            await transport.close()


class LocalMicrosoftAuth:
    def __init__(self, protocol, browser_factory, *, now=lambda: datetime.now(timezone.utc)):
        self.protocol = protocol
        self.browser_factory = browser_factory
        self.now = now

    async def login(self, entry: AccountEntry, settings: MicrosoftSettings) -> CredentialRecord:
        session = self.protocol.new_session(settings.region)
        async with self.browser_factory.account_context() as browser:
            first = await browser.capture_callback(
                session.signin_url, entry.account, entry.password, expected_path="/"
            )
            callback = self.protocol.parse_portal_callback(first, session.state)
            if callback.kind == "social":
                if callback.code is None:
                    raise LocalAuthError(
                        "invalid_callback",
                        "microsoft_callback",
                        False,
                        "登录回调缺少授权码",
                    )
                token = await self.protocol.exchange_social(callback.code, session.verifier)
                return self.protocol.social_record(
                    entry.account,
                    settings.region,
                    token,
                    self.now(),
                )
            leg = await self.protocol.prepare_external(callback)
            final = await browser.capture_callback(
                leg.authorize_url, entry.account, entry.password,
                expected_path="/oauth/callback",
            )
            token = await self.protocol.exchange_external(leg, final)
            return self.protocol.external_record(
                entry.account,
                settings.region,
                leg,
                token,
                self.now(),
            )
