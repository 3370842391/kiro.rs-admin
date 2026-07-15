import asyncio
import sys
import unittest
from contextlib import asynccontextmanager
from datetime import datetime, timezone
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.credential_models import CredentialRecord
from batch_login.enterprise_http import EnterpriseHttpResult
from batch_login.local_auth import (
    EnterpriseSettings,
    LocalEnterpriseAuth,
    LocalMicrosoftAuth,
    MicrosoftSettings,
)
from batch_login.local_idc import IdcSession, IdcToken
from batch_login.local_microsoft import MicrosoftToken, PortalCallback
from batch_login.models import AccountEntry

NOW = datetime(2026, 7, 15, tzinfo=timezone.utc)


class FakeIdc:
    async def start(self, start_url, region):
        return IdcSession(
            region,
            start_url,
            "client",
            "secret",
            "device",
            "CODE",
            "https://verify",
            999,
            1,
        )

    async def poll(self, _session):
        return IdcToken("access", "refresh", 3600)


class FakeEnterpriseProtocol:
    async def login(self, account, password, settings):
        self.received = (account, password, settings)
        return EnterpriseHttpResult(
            directory_id="d-123",
            client_id="client",
            client_secret="secret",
            access_token="access",
            refresh_token="refresh",
            expires_in=3600,
        )


class FakePage:
    def __init__(self, owner):
        self.owner = owner

    async def complete_enterprise(
        self,
        url,
        account,
        password,
        user_code=None,
        new_password=None,
    ):
        self.owner.enterprise = (url, account, password, user_code)
        self.owner.new_password = new_password
        if self.owner.block_enterprise:
            await asyncio.Future()

    async def capture_callback(self, _url, _account, _password, *, expected_path):
        self.owner.paths.append(expected_path)
        return self.owner.callbacks.pop(0)


class FakeBrowser:
    def __init__(self, callbacks=None, *, block_enterprise=False):
        self.callbacks = list(callbacks or [])
        self.block_enterprise = block_enterprise
        self.paths = []
        self.contexts = 0
        self.enterprise = None
        self.new_password = None

    @asynccontextmanager
    async def account_context(self):
        self.contexts += 1
        yield FakePage(self)


class FakeMicrosoft:
    def __init__(self, callback_kind="external_idp"):
        self.callback_kind = callback_kind

    def new_session(self, _region):
        return SimpleNamespace(state="s", verifier="v", signin_url="https://signin")

    def parse_portal_callback(self, _url, _state):
        if self.callback_kind == "social":
            return PortalCallback("social", code="portal-code")
        return PortalCallback(
            "external_idp",
            issuer_url="https://login.microsoftonline.com/t",
            client_id="c",
        )

    async def prepare_external(self, _callback):
        return SimpleNamespace(
            authorize_url="https://authorize",
            state="s2",
            client_id="c",
            token_endpoint="https://login.microsoftonline.com/token",
            issuer_url="https://login.microsoftonline.com/t",
            scopes="openid",
        )

    async def exchange_external(self, _leg, _url):
        return MicrosoftToken("access", "refresh", 1800)

    async def exchange_social(self, code, verifier):
        self.social_exchange = (code, verifier)
        return MicrosoftToken("access", "refresh", 1800)

    def external_record(self, email, region, leg, token, _now):
        return CredentialRecord(
            email,
            "external_idp",
            "Enterprise",
            token.refresh_token,
            token.access_token,
            client_id=leg.client_id,
            token_endpoint=leg.token_endpoint,
            issuer_url=leg.issuer_url,
            scopes=leg.scopes,
            region=region,
        )

    def social_record(self, email, region, token, _now):
        return CredentialRecord(
            email,
            "social",
            "Microsoft",
            token.refresh_token,
            token.access_token,
            region=region,
        )


class LocalAuthTests(unittest.IsolatedAsyncioTestCase):
    async def test_enterprise_returns_complete_idc_record(self):
        protocol = FakeEnterpriseProtocol()
        record = await LocalEnterpriseAuth(protocol, now=lambda: NOW).login(
            AccountEntry(1, "admin-user", "password"),
            EnterpriseSettings(
                "https://d-123.awsapps.com/start",
                "us-east-1",
            ),
        )
        self.assertEqual("idc", record.auth_method)
        self.assertEqual("client", record.client_id)
        self.assertEqual("2026-07-15T01:00:00Z", record.expires_at)
        self.assertEqual("admin-user", protocol.received[0])
        self.assertEqual("password", protocol.received[1])
        self.assertEqual("https://d-123.awsapps.com/start", protocol.received[2].start_url)

    async def test_microsoft_external_reuses_one_context(self):
        browser = FakeBrowser([
            "http://localhost:3128?issuer_url=x&client_id=c&state=s",
            "http://localhost:3128/oauth/callback?code=final&state=s2",
        ])
        record = await LocalMicrosoftAuth(FakeMicrosoft(), browser, now=lambda: NOW).login(
            AccountEntry(1, "user@example.com", "password"),
            MicrosoftSettings("us-east-1"),
        )
        self.assertEqual(1, browser.contexts)
        self.assertEqual(["/", "/oauth/callback"], browser.paths)
        self.assertEqual("external_idp", record.auth_method)

    async def test_microsoft_social_uses_portal_callback_without_second_browser_leg(self):
        browser = FakeBrowser(["http://localhost:3128?code=portal-code&state=s"])
        protocol = FakeMicrosoft("social")

        record = await LocalMicrosoftAuth(
            protocol,
            browser,
            now=lambda: NOW,
        ).login(
            AccountEntry(1, "user@example.com", "password"),
            MicrosoftSettings("us-east-1"),
        )

        self.assertEqual("social", record.auth_method)
        self.assertEqual(["/"], browser.paths)
        self.assertEqual(("portal-code", "v"), protocol.social_exchange)

if __name__ == "__main__":
    unittest.main()
