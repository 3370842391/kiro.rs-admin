import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.credential_models import CredentialRecord
from batch_login.enterprise_http import HttpResponse
from batch_login.local_runner import LocalBatchRunner
from batch_login.models import AccountEntry, LoginMode
from batch_login.worker_events import LocalRunSettings, ResultMode

ARN = "arn:aws:codewhisperer:us-east-1:389192036452:profile/HG4CUCMEN7QV"


def credential(email="codeflow2-7"):
    return CredentialRecord(
        email=email,
        auth_method="idc",
        provider="Enterprise",
        access_token="access-secret",
        region="us-east-1",
    )


def settings(*, create_api_key=True):
    return LocalRunSettings(
        mode=LoginMode.ENTERPRISE,
        region="us-east-1",
        start_url="https://example.awsapps.com/start",
        headless=True,
        timeout_seconds=10,
        mfa_timeout_seconds=10,
        result_mode=ResultMode.SAVE_ONLY,
        credential_path=Path("unused.json"),
        checkpoint_path=Path("unused.jsonl"),
        create_api_key=create_api_key,
    )


class FakeAuth:
    def __init__(self, result):
        self.result = result

    async def login(self, entry, _settings):
        return self.result


class FakeStore:
    def __init__(self):
        self.saved = []

    def append(self, record):
        self.saved.append(record)
        return True


class FakeCheckpoint:
    def should_run(self, **_kwargs):
        return True

    def append(self, _record):
        pass


class FakeTransport:
    def __init__(self, responses):
        self.responses = list(responses)
        self.closed = False

    async def request(self, method, url, *, headers=None, json=None, **kwargs):
        return self.responses.pop(0)

    async def close(self):
        self.closed = True


def ok(data):
    return HttpResponse(200, {"content-type": "application/json"}, data)


class RunnerApiKeyTests(unittest.IsolatedAsyncioTestCase):
    async def _run(self, *, create_api_key=True, transport=None):
        store = FakeStore()
        events = []
        runner = LocalBatchRunner(
            enterprise=FakeAuth(credential()),
            microsoft=None,
            store=store,
            checkpoint=FakeCheckpoint(),
            emit=events.append,
            api_key_transport_factory=(lambda: transport) if transport else None,
        )
        summary = await runner.run(
            [AccountEntry(1, "codeflow2-7", "pw", "https://example.awsapps.com/start")],
            settings(create_api_key=create_api_key),
        )
        return store, events, summary

    async def test_login_creates_and_persists_api_key(self):
        transport = FakeTransport(
            [ok({"profiles": [{"arn": ARN}]}), ok({"rawKey": "ksk_live_key"})]
        )
        store, events, summary = await self._run(transport=transport)
        self.assertEqual("ksk_live_key", store.saved[0].kiro_api_key)
        self.assertEqual(ARN, store.saved[0].profile_arn)  # 回填
        self.assertEqual(1, summary.api_keys_created)
        self.assertTrue(transport.closed)
        self.assertTrue(any(e.kind == "api_key_created" for e in events))

    async def test_toggle_off_skips_key_creation(self):
        store, events, summary = await self._run(create_api_key=False)
        self.assertIsNone(store.saved[0].kiro_api_key)
        self.assertEqual(0, summary.api_keys_created)

    async def test_api_key_failure_does_not_fail_login(self):
        transport = FakeTransport([ok({"profiles": []})])  # no profile -> ApiKeyError
        store, events, summary = await self._run(transport=transport)
        self.assertEqual(1, summary.succeeded)  # 登录仍算成功
        self.assertIsNone(store.saved[0].kiro_api_key)
        self.assertEqual(1, summary.api_keys_failed)
        self.assertTrue(any(e.kind == "api_key_failed" for e in events))


if __name__ == "__main__":
    unittest.main()
