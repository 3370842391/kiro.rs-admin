import asyncio
import json
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.browser_flows import BrowserFlowError
from batch_login.credential_models import CredentialRecord
from batch_login.credential_store import CredentialStoreError
from batch_login.local_auth import LocalAuthError
from batch_login.local_runner import LocalBatchRunner
from batch_login.models import AccountEntry, LoginMode
from batch_login.worker_events import LocalRunSettings, ResultMode


def credential(email="admin-user"):
    return CredentialRecord(
        email=email,
        auth_method="idc",
        provider="Enterprise",
        refresh_token="refresh-secret",
        access_token="access-secret",
        client_id="client",
        client_secret="client-secret",
        start_url="https://example.awsapps.com/start",
        region="us-east-1",
    )


def settings_for(result_mode, *, resume=False):
    return LocalRunSettings(
        mode=LoginMode.ENTERPRISE,
        region="us-east-1",
        start_url="https://example.awsapps.com/start",
        headless=True,
        timeout_seconds=10,
        mfa_timeout_seconds=10,
        result_mode=result_mode,
        credential_path=Path("unused-credentials.json"),
        checkpoint_path=Path("unused-checkpoint.jsonl"),
        resume=resume,
    )


class FakeAuth:
    def __init__(self, results):
        self.results = list(results)
        self.accounts = []

    async def login(self, entry, _settings):
        self.accounts.append(entry.account)
        value = self.results.pop(0)
        if isinstance(value, BaseException):
            raise value
        return value


class FakeStore:
    def __init__(self, calls=None, *, error=None):
        self.calls = calls if calls is not None else []
        self.error = error

    def append(self, _record):
        self.calls.append("store.append")
        if self.error is not None:
            raise self.error
        return True


class FakeCheckpoint:
    def __init__(self, *, should_run=True):
        self.run = should_run
        self.records = []
        self.import_records = []

    def should_run(self, **_kwargs):
        return self.run

    def append(self, record):
        self.records.append(record)

    def append_import_result(self, previous, **kwargs):
        self.import_records.append((previous, kwargs))


class FakeImporter:
    def __init__(self, calls=None):
        self.calls = calls if calls is not None else []

    async def batch_import(self, credentials, on_event):
        self.calls.append("import.start")
        self.credentials = credentials
        on_event({"index": 0, "status": "verified", "credentialId": 9})
        return {
            "total": 1,
            "imported": 0,
            "verified": 1,
            "duplicate": 0,
            "failed": 0,
            "rolledBack": 0,
        }


class LocalRunnerTests(unittest.IsolatedAsyncioTestCase):
    async def test_credentials_are_saved_before_import_starts(self):
        calls = []
        checkpoint = FakeCheckpoint()
        importer = FakeImporter(calls)
        auth = FakeAuth([credential()])
        events = []
        runner = LocalBatchRunner(
            enterprise=auth,
            microsoft=auth,
            store=FakeStore(calls),
            checkpoint=checkpoint,
            importer=importer,
            emit=events.append,
        )

        summary = await runner.run(
            [AccountEntry(1, "admin-user", "one-time-password")],
            settings_for(ResultMode.SAVE_AND_IMPORT),
        )

        self.assertLess(calls.index("store.append"), calls.index("import.start"))
        self.assertEqual(1, summary.succeeded)
        self.assertEqual(1, summary.imported)
        self.assertEqual("refresh-secret", importer.credentials[0]["refreshToken"])
        self.assertEqual("verified", checkpoint.import_records[0][1]["import_status"])
        self.assertEqual("batch_finished", events[-1].kind)
        started = next(event for event in events if event.kind == "account_started")
        self.assertEqual("ad***", started.payload["accountMasked"])
        self.assertNotIn("admin-user", json.dumps(started.payload))

    async def test_one_account_failure_does_not_stop_next_account(self):
        auth = FakeAuth(
            [
                LocalAuthError(
                    "invalid_credentials",
                    "browser_login",
                    False,
                    "登录失败",
                ),
                credential("good-user"),
            ]
        )
        checkpoint = FakeCheckpoint()
        runner = LocalBatchRunner(
            enterprise=auth,
            microsoft=auth,
            store=FakeStore(),
            checkpoint=checkpoint,
        )

        summary = await runner.run(
            [
                AccountEntry(1, "bad-user", "bad-password"),
                AccountEntry(2, "good-user", "good-password"),
            ],
            settings_for(ResultMode.SAVE_ONLY),
        )

        self.assertEqual(1, summary.failed)
        self.assertEqual(1, summary.succeeded)
        self.assertEqual(["failed", "success"], [r.status for r in checkpoint.records])

    async def test_manual_browser_error_is_recorded_for_resume(self):
        error = BrowserFlowError("captcha_required", "mfa", False, "等待人工验证")
        auth = FakeAuth([error])
        checkpoint = FakeCheckpoint()
        runner = LocalBatchRunner(
            enterprise=auth,
            microsoft=auth,
            store=FakeStore(),
            checkpoint=checkpoint,
        )

        summary = await runner.run(
            [AccountEntry(1, "admin-user", "one-time-password")],
            settings_for(ResultMode.SAVE_ONLY),
        )

        self.assertEqual(1, summary.manual_required)
        self.assertEqual("manual_required", checkpoint.records[0].status)

    async def test_store_failure_is_fatal_and_never_imports(self):
        calls = []
        auth = FakeAuth([credential(), credential("second-user")])
        runner = LocalBatchRunner(
            enterprise=auth,
            microsoft=auth,
            store=FakeStore(calls, error=CredentialStoreError("写入失败")),
            checkpoint=FakeCheckpoint(),
            importer=FakeImporter(calls),
        )

        with self.assertRaises(CredentialStoreError):
            await runner.run(
                [
                    AccountEntry(1, "admin-user", "one-time-password"),
                    AccountEntry(2, "second-user", "one-time-password"),
                ],
                settings_for(ResultMode.SAVE_AND_IMPORT),
            )

        self.assertEqual(["admin-user"], auth.accounts)
        self.assertNotIn("import.start", calls)

    async def test_cancel_is_rethrown_and_emits_safe_event(self):
        auth = FakeAuth([asyncio.CancelledError()])
        events = []
        runner = LocalBatchRunner(
            enterprise=auth,
            microsoft=auth,
            store=FakeStore(),
            checkpoint=FakeCheckpoint(),
            emit=events.append,
        )

        with self.assertRaises(asyncio.CancelledError):
            await runner.run(
                [AccountEntry(1, "admin-user", "one-time-password")],
                settings_for(ResultMode.SAVE_ONLY),
            )

        self.assertEqual("batch_cancelled", events[-1].kind)
        serialized = json.dumps(events[-1].payload)
        self.assertNotIn("admin-user", serialized)
        self.assertNotIn("one-time-password", serialized)

    async def test_resume_skips_previously_saved_account(self):
        auth = FakeAuth([credential()])
        runner = LocalBatchRunner(
            enterprise=auth,
            microsoft=auth,
            store=FakeStore(),
            checkpoint=FakeCheckpoint(should_run=False),
        )

        summary = await runner.run(
            [AccountEntry(1, "admin-user", "one-time-password")],
            settings_for(ResultMode.SAVE_ONLY, resume=True),
        )

        self.assertEqual(0, summary.succeeded)
        self.assertEqual([], auth.accounts)


if __name__ == "__main__":
    unittest.main()
