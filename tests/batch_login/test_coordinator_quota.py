import asyncio
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.account_login_coordinator import AccountLoginCoordinator, QuotaRefreshReport
from batch_login.account_repository import AccountRepository
from batch_login.api_key_refresh import RefreshResult
from batch_login.credential_models import CredentialRecord
from batch_login.gui_settings import GuiSavedSettings
from batch_login.models import AccountEntry, LoginMode
from batch_login.usage_client import UsageError, UsageSnapshot
from batch_login.worker_events import WorkerEvent


class Protector:
    def protect(self, value):
        return b"p:" + value[::-1]

    def unprotect(self, value):
        return value.removeprefix(b"p:")[::-1]


class SettingsStore:
    def __init__(self, settings):
        self.settings = settings

    def load(self):
        return self.settings


class FakeTransport:
    def __init__(self):
        self.closed = False

    async def close(self):
        self.closed = True


NOW = datetime(2026, 7, 19, 12, 0, 0, tzinfo=timezone.utc)


def stamp(delta_seconds):
    return (NOW + timedelta(seconds=delta_seconds)).isoformat().replace("+00:00", "Z")


class _patch:
    def __init__(self, module, name, value):
        self.module, self.name, self.value = module, name, value

    def __enter__(self):
        self.orig = getattr(self.module, self.name)
        setattr(self.module, self.name, self.value)

    def __exit__(self, *exc):
        setattr(self.module, self.name, self.orig)
        return False


class RefreshQuotaTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.repo = AccountRepository(self.root / "accounts.sqlite3", protector=Protector())
        self.account = self.repo.upsert_entries(
            [AccountEntry(1, "acc-1", "pw", "https://d-9067123456.awsapps.com/start")],
            login_mode=LoginMode.ENTERPRISE, region="us-east-1",
        )[0]
        self.settings = GuiSavedSettings(
            credential_path=str(self.root / "c.json"), region="us-east-1",
        )
        self.transports = []

    async def asyncTearDown(self):
        self.temp.cleanup()

    def _coordinator(self, refresher=None):
        def factory():
            t = FakeTransport()
            self.transports.append(t)
            return t

        async def never_refresh(*a, **k):
            raise AssertionError("refresh should not run")

        return AccountLoginCoordinator(
            self.repo, SettingsStore(self.settings),
            api_key_transport_factory=factory,
            token_refresher=refresher or never_refresh,
            now=lambda: NOW,
        )

    def _save_cred(self, **overrides):
        base = dict(
            email="acc-1", auth_method="idc", provider="Enterprise",
            access_token="tok", profile_arn="arn:x", region="us-east-1",
            client_id="cid", client_secret="cs", refresh_token="rt",
            start_url="https://d-9067123456.awsapps.com/start",
        )
        base.update(overrides)
        self.repo.save_credential(self.account.id, CredentialRecord(**base))

    async def test_fetches_and_saves_quota(self):
        self._save_cred(expires_at=stamp(3600))
        coordinator = self._coordinator()
        seen = {}

        async def fake_usage(transport, *, token, profile_arn, region, token_type, **k):
            seen.update(token=token, profile_arn=profile_arn, token_type=token_type)
            return UsageSnapshot(remaining=416.5, total=550, used=133.5,
                                 subscription="Kiro Pro", free_trial=False,
                                 next_reset="2025-08-01T00:00:00Z")

        import batch_login.account_login_coordinator as mod
        events = []
        with _patch(mod, "get_usage_limits", fake_usage):
            report = await coordinator.refresh_quota(
                [self.account.id], progress=lambda _e: None, event_sink=events.append
            )

        self.assertEqual(1, report.updated)
        self.assertEqual(0, report.refreshed)
        self.assertEqual("tok", seen["token"])
        self.assertEqual("arn:x", seen["profile_arn"])
        self.assertIsNone(seen["token_type"])
        stored = self.repo.load_quota(self.account.id)
        self.assertEqual(416.5, stored["remaining"])
        self.assertEqual("Kiro Pro", stored["subscription"])
        self.assertIn("quota_updated", [e.kind for e in events if isinstance(e, WorkerEvent)])
        self.assertTrue(self.transports[0].closed)

    async def test_expired_token_refreshed_before_query(self):
        self._save_cred(expires_at=stamp(-10))
        calls = []

        async def fake_refresh(transport, **k):
            calls.append(k["refresh_token"])
            return RefreshResult(access_token="fresh", refresh_token="rt2", expires_in=3600)

        coordinator = self._coordinator(refresher=fake_refresh)
        tokens = []

        async def fake_usage(transport, *, token, **k):
            tokens.append(token)
            return UsageSnapshot(10, 20, 10, None, False, None)

        import batch_login.account_login_coordinator as mod
        with _patch(mod, "get_usage_limits", fake_usage):
            report = await coordinator.refresh_quota(
                [self.account.id], progress=lambda _e: None, event_sink=lambda _e: None
            )

        self.assertEqual(1, report.refreshed)
        self.assertEqual(1, report.updated)
        self.assertEqual(["rt"], calls)
        self.assertEqual(["fresh"], tokens)
        self.assertEqual("fresh", self.repo.load_credential(self.account.id).access_token)

    async def test_external_idp_sends_tokentype(self):
        self._save_cred(auth_method="external_idp", expires_at=stamp(3600))
        coordinator = self._coordinator()
        seen = {}

        async def fake_usage(transport, *, token, token_type, **k):
            seen["token_type"] = token_type
            return UsageSnapshot(10, 20, 10, None, False, None)

        import batch_login.account_login_coordinator as mod
        with _patch(mod, "get_usage_limits", fake_usage):
            await coordinator.refresh_quota([self.account.id], event_sink=lambda _e: None)
        self.assertEqual("EXTERNAL_IDP", seen["token_type"])

    async def test_missing_profile_arn_resolved_first(self):
        self._save_cred(profile_arn=None, expires_at=stamp(3600))
        coordinator = self._coordinator()

        async def fake_resolve(transport, *, token, region, token_type):
            return "arn:resolved"

        async def fake_usage(transport, *, token, profile_arn, **k):
            self.assertEqual("arn:resolved", profile_arn)
            return UsageSnapshot(1, 2, 1, None, False, None)

        import batch_login.account_login_coordinator as mod
        with _patch(mod, "resolve_profile_arn", fake_resolve), _patch(mod, "get_usage_limits", fake_usage):
            report = await coordinator.refresh_quota([self.account.id], event_sink=lambda _e: None)
        self.assertEqual(1, report.updated)
        # profileArn 回填入库
        self.assertEqual("arn:resolved", self.repo.load_credential(self.account.id).profile_arn)

    async def test_no_credential_skipped(self):
        coordinator = self._coordinator()
        events = []

        async def fake_usage(*a, **k):
            raise AssertionError("should not query without credential")

        import batch_login.account_login_coordinator as mod
        with _patch(mod, "get_usage_limits", fake_usage):
            report = await coordinator.refresh_quota(
                [self.account.id], event_sink=events.append
            )
        self.assertEqual(1, report.skipped)
        self.assertEqual(0, report.updated)
        self.assertIsNone(self.repo.load_quota(self.account.id))

    async def test_usage_error_does_not_abort(self):
        self._save_cred(expires_at=stamp(3600))
        coordinator = self._coordinator()

        async def fake_usage(*a, **k):
            raise UsageError("http_error", "get_usage", False, "boom", status_code=403)

        import batch_login.account_login_coordinator as mod
        with _patch(mod, "get_usage_limits", fake_usage):
            report = await coordinator.refresh_quota([self.account.id], event_sink=lambda _e: None)
        self.assertEqual(1, report.failed)
        self.assertEqual(0, report.updated)
        self.assertTrue(self.transports[0].closed)

    async def test_refresh_quota_honors_concurrency_limit(self):
        extra_accounts = self.repo.upsert_entries(
            [
                AccountEntry(2, "acc-2", "pw", "https://d-9067123456.awsapps.com/start"),
                AccountEntry(3, "acc-3", "pw", "https://d-9067123456.awsapps.com/start"),
                AccountEntry(4, "acc-4", "pw", "https://d-9067123456.awsapps.com/start"),
            ],
            login_mode=LoginMode.ENTERPRISE,
            region="us-east-1",
        )
        accounts = [self.account, *extra_accounts]
        for account in accounts:
            self.repo.save_credential(
                account.id,
                CredentialRecord(
                    email=account.account,
                    auth_method="idc",
                    provider="Enterprise",
                    access_token=f"tok-{account.id}",
                    profile_arn="arn:x",
                    region="us-east-1",
                    expires_at=stamp(3600),
                    start_url=account.start_url,
                ),
            )

        coordinator = self._coordinator()
        active = 0
        max_active = 0

        async def fake_usage(*_args, **_kwargs):
            nonlocal active, max_active
            active += 1
            max_active = max(max_active, active)
            await asyncio.sleep(0.01)
            active -= 1
            return UsageSnapshot(10, 20, 10, None, False, None)

        import batch_login.account_login_coordinator as mod
        with _patch(mod, "get_usage_limits", fake_usage):
            report = await coordinator.refresh_quota(
                [account.id for account in accounts],
                concurrency=2,
                event_sink=lambda _e: None,
            )

        self.assertEqual(4, report.updated)
        self.assertEqual(2, max_active)


if __name__ == "__main__":
    unittest.main()
