import sys
import unittest
from asyncio import CancelledError
from contextlib import asynccontextmanager
from pathlib import Path
from unittest.mock import AsyncMock


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.browser_flows import BrowserFlowError
from batch_login.models import AccountEntry, LoginMode, ResultStatus
from batch_login.runner import BatchLoginRunner, RunnerSettings


class FakeClient:
    def __init__(self):
        self.completed = []
        self.cancelled = []

    async def start_idc(self, **_kwargs):
        return {
            "sessionId": "idc-1",
            "verificationUriComplete": "https://aws/login",
            "pollInterval": 0,
        }

    async def poll_idc(self, _session_id):
        return {"status": "success", "credentialId": 5, "duplicate": False}

    async def start_social(self, **_kwargs):
        return {"sessionId": "social-1", "portalUrl": "https://kiro/signin"}

    async def complete_social(self, session_id, callback):
        self.completed.append((session_id, callback))
        if len(self.completed) == 1:
            return {
                "status": "continue",
                "nextUrl": "https://login.microsoftonline.com/authorize",
            }
        return {"status": "success", "credentialId": 8, "duplicate": True}

    async def cancel_idc(self, session_id):
        self.cancelled.append(("idc", session_id))

    async def cancel_social(self, session_id):
        self.cancelled.append(("social", session_id))


class FakeBrowserSession:
    def __init__(self, fail=False):
        self.fail = fail
        self.callbacks = iter(
            [
                "http://127.0.0.1/signin/callback?login_option=external_idp"
                "&issuer_url=https%3A%2F%2Flogin.microsoftonline.com%2Ft%2Fv2.0"
                "&client_id=c&state=p",
                "http://127.0.0.1/oauth/callback?code=final&state=s",
            ]
        )

    async def complete_enterprise(self, *_args):
        if self.fail:
            raise BrowserFlowError(
                "invalid_credentials",
                "browser_login",
                False,
                "bad password",
            )

    async def capture_callback(self, *_args, **_kwargs):
        return next(self.callbacks)


class FakeBrowserFactory:
    def __init__(self, session):
        self.session = session

    @asynccontextmanager
    async def account_context(self):
        yield self.session


class SequencedBrowserFactory:
    def __init__(self, sessions):
        self.sessions = iter(sessions)

    def account_context(self):
        return FakeBrowserFactory(next(self.sessions)).account_context()


def entry(account="alice", password="secret", line_number=1):
    return AccountEntry(
        line_number=line_number,
        account=account,
        password=password,
    )


def settings():
    return RunnerSettings(
        region="us-east-1",
        start_url="https://example.awsapps.com/start",
    )


class RunnerTests(unittest.IsolatedAsyncioTestCase):
    async def test_microsoft_submits_two_callbacks_in_same_session(self):
        client = FakeClient()
        runner = BatchLoginRunner(
            client,
            FakeBrowserFactory(FakeBrowserSession()),
            checkpoint=None,
        )
        outcome = await runner.run_one(
            LoginMode.MICROSOFT,
            entry("user@example.com", "pw"),
            settings(),
        )
        self.assertEqual(ResultStatus.DUPLICATE, outcome.status)
        self.assertEqual(2, len(client.completed))
        self.assertEqual({"social-1"}, {item[0] for item in client.completed})

    async def test_browser_failure_cancels_server_session(self):
        client = FakeClient()
        runner = BatchLoginRunner(
            client,
            FakeBrowserFactory(FakeBrowserSession(fail=True)),
            checkpoint=None,
        )
        outcome = await runner.run_one(
            LoginMode.ENTERPRISE,
            entry("alice", "wrong"),
            settings(),
        )
        self.assertEqual("invalid_credentials", outcome.code)
        self.assertEqual([("idc", "idc-1")], client.cancelled)

    async def test_batch_continues_after_non_retryable_failure(self):
        client = FakeClient()
        factory = SequencedBrowserFactory(
            [
                FakeBrowserSession(fail=True),
                FakeBrowserSession(fail=False),
            ]
        )
        runner = BatchLoginRunner(client, factory, checkpoint=None)
        outcomes = await runner.run_batch(
            LoginMode.ENTERPRISE,
            [entry("first", "wrong"), entry("second", "right", line_number=2)],
            settings(),
            resume=False,
            run_id="run-1",
        )
        self.assertEqual(
            [ResultStatus.FAILED, ResultStatus.SUCCESS],
            [item.status for item in outcomes],
        )

    async def test_wait_idc_repeats_pending_until_success(self):
        client = FakeClient()
        replies = iter(
            [
                {"status": "pending"},
                {"status": "success", "credentialId": 6, "duplicate": False},
            ]
        )
        client.poll_idc = AsyncMock(side_effect=lambda _session: next(replies))
        runner = BatchLoginRunner(
            client,
            FakeBrowserFactory(FakeBrowserSession()),
            checkpoint=None,
        )
        result = await runner._wait_idc("idc-1", 0)
        self.assertEqual(6, result["credentialId"])
        self.assertEqual(2, client.poll_idc.await_count)

    async def test_expired_idc_is_non_retryable_failure(self):
        client = FakeClient()
        client.poll_idc = AsyncMock(return_value={"status": "expired"})
        runner = BatchLoginRunner(
            client,
            FakeBrowserFactory(FakeBrowserSession()),
            checkpoint=None,
        )
        outcome = await runner.run_one(LoginMode.ENTERPRISE, entry(), settings())
        self.assertEqual("session_expired", outcome.code)
        self.assertFalse(outcome.retryable)

    async def test_invalid_start_response_cancels_created_session(self):
        client = FakeClient()
        client.start_social = AsyncMock(return_value={"sessionId": "social-1"})
        runner = BatchLoginRunner(
            client,
            FakeBrowserFactory(FakeBrowserSession()),
            checkpoint=None,
        )
        outcome = await runner.run_one(
            LoginMode.MICROSOFT,
            entry("user@example.com", "pw"),
            settings(),
        )
        self.assertEqual("invalid_rs_response", outcome.code)
        self.assertEqual([("social", "social-1")], client.cancelled)

    async def test_cancellation_cleans_session_and_is_re_raised(self):
        client = FakeClient()
        browser = FakeBrowserSession()
        browser.complete_enterprise = AsyncMock(side_effect=CancelledError())
        runner = BatchLoginRunner(
            client,
            FakeBrowserFactory(browser),
            checkpoint=None,
        )
        with self.assertRaises(CancelledError):
            await runner.run_one(LoginMode.ENTERPRISE, entry(), settings())
        self.assertEqual([("idc", "idc-1")], client.cancelled)


if __name__ == "__main__":
    unittest.main()
