from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone

from .credential_models import CredentialRecord
from .local_idc import LocalAuthError
from .models import AccountEntry


@dataclass(slots=True, frozen=True)
class EnterpriseSettings:
    start_url: str
    region: str
    new_password: str = field(default="", repr=False)


@dataclass(slots=True, frozen=True)
class MicrosoftSettings:
    region: str = "us-east-1"


class LocalEnterpriseAuth:
    def __init__(self, idc, browser_factory, *, now=lambda: datetime.now(timezone.utc)):
        self.idc = idc
        self.browser_factory = browser_factory
        self.now = now

    async def login(self, entry: AccountEntry, settings: EnterpriseSettings) -> CredentialRecord:
        session = await self.idc.start(settings.start_url, settings.region)
        async with self.browser_factory.account_context() as browser:
            browser_task = asyncio.create_task(
                browser.complete_enterprise(
                    session.verification_url,
                    entry.account,
                    entry.password,
                    session.user_code,
                    new_password=settings.new_password or None,
                )
            )
            token_task = asyncio.create_task(self.idc.poll(session))
            tasks = {browser_task, token_task}
            try:
                done, _ = await asyncio.wait(
                    tasks,
                    return_when=asyncio.FIRST_COMPLETED,
                )
                if token_task in done:
                    token = token_task.result()
                else:
                    browser_task.result()
                    token = await token_task
            finally:
                for task in tasks:
                    if not task.done():
                        task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)
        expires = (
            None
            if token.expires_in is None
            else (self.now() + timedelta(seconds=token.expires_in))
            .isoformat()
            .replace("+00:00", "Z")
        )
        return CredentialRecord(
            email=entry.account,
            auth_method="idc",
            provider="Enterprise",
            refresh_token=token.refresh_token,
            access_token=token.access_token,
            client_id=session.client_id,
            client_secret=session.client_secret,
            start_url=settings.start_url,
            region=settings.region,
            expires_at=expires,
        )


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
