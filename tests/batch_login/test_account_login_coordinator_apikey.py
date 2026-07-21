import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.account_login_coordinator import (
    AccountLoginCoordinator,
    ApiKeyExtractionReport,
    LoginProgressEvent,
)
from batch_login.account_repository import AccountRepository
from batch_login.api_key_client import ApiKeyError, ApiKeyResult
from batch_login.api_key_refresh import RefreshResult
from batch_login.credential_models import CredentialRecord
from batch_login.gui_settings import GuiSavedSettings
from batch_login.models import AccountEntry, LoginMode
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


class ExtractApiKeysTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.repo = AccountRepository(self.root / "accounts.sqlite3", protector=Protector())
        self.accounts = self.repo.upsert_entries(
            [AccountEntry(1, "codeflow2-1", "pw", "https://d-9067123456.awsapps.com/start")],
            login_mode=LoginMode.ENTERPRISE,
            region="us-east-1",
        )
        self.account_id = self.accounts[0].id
        self.settings = GuiSavedSettings(
            credential_path=str(self.root / "complete.json"),
            oidc_export_directory=str(self.root / "exports"),
            region="us-east-1",
        )
        self.transports = []

    async def asyncTearDown(self):
        self.temp.cleanup()

    def _coordinator(self, *, ensure=None, refresher=None):
        events = []
        progress = []

        def transport_factory():
            transport = FakeTransport()
            self.transports.append(transport)
            return transport

        coordinator = AccountLoginCoordinator(
            self.repo,
            SettingsStore(self.settings),
            api_key_transport_factory=transport_factory,
            token_refresher=refresher or self._never_refresh,
            now=lambda: NOW,
            emit=events.append,
        )
        if ensure is not None:
            coordinator._patched_ensure = ensure
        return coordinator, events, progress

    @staticmethod
    async def _never_refresh(*args, **kwargs):
        raise AssertionError("refresh should not be called")

    def _save_credential(self, **overrides):
        base = dict(
            email="codeflow2-1",
            auth_method="idc",
            provider="Enterprise",
            access_token="stored-access",
            refresh_token="stored-refresh",
            client_id="cid",
            client_secret="csecret",
            start_url="https://d-9067123456.awsapps.com/start",
            region="us-east-1",
        )
        base.update(overrides)
        self.repo.save_credential(self.account_id, CredentialRecord(**base))

    async def test_fresh_token_creates_key_and_writes_back(self):
        self._save_credential(expires_at=stamp(3600))
        coordinator, events, progress = self._coordinator()

        seen = {}

        async def fake_ensure(transport, *, token, label, region, profile_arn, token_type, skip_if_labeled_exists):
            seen.update(token=token, label=label, region=region, token_type=token_type)
            return ApiKeyResult(raw_key="ksk_created", profile_arn="arn:x", reused=False)

        import batch_login.account_login_coordinator as mod
        with unittest_patch(mod, "ensure_api_key", fake_ensure):
            report = await coordinator.extract_api_keys(
                [self.account_id], progress=progress.append, event_sink=events.append
            )

        self.assertEqual(1, report.created)
        self.assertEqual(0, report.refreshed)
        self.assertEqual("stored-access", seen["token"])
        self.assertEqual("codeflow2-1", seen["label"])
        self.assertIsNone(seen["token_type"])
        stored = self.repo.load_credential(self.account_id)
        self.assertEqual("ksk_created", stored.kiro_api_key)
        self.assertEqual("arn:x", stored.profile_arn)
        kinds = [e.kind for e in events if isinstance(e, WorkerEvent)]
        self.assertIn("api_key_created", kinds)
        self.assertIn("api_key_exported", kinds)
        self.assertTrue(self.transports[0].closed)

    async def test_expired_token_refreshes_before_creating(self):
        self._save_credential(expires_at=stamp(-10))
        refresh_calls = []

        async def fake_refresh(transport, *, client_id, client_secret, refresh_token, start_url, region):
            refresh_calls.append(refresh_token)
            return RefreshResult(access_token="fresh-access", refresh_token="fresh-refresh", expires_in=3600)

        coordinator, events, progress = self._coordinator(refresher=fake_refresh)

        tokens_used = []

        async def fake_ensure(transport, *, token, label, region, profile_arn, token_type, skip_if_labeled_exists):
            tokens_used.append(token)
            return ApiKeyResult(raw_key="ksk_after_refresh", profile_arn="arn:x", reused=False)

        import batch_login.account_login_coordinator as mod
        with unittest_patch(mod, "ensure_api_key", fake_ensure):
            report = await coordinator.extract_api_keys(
                [self.account_id], progress=progress.append, event_sink=events.append
            )

        self.assertEqual(1, report.refreshed)
        self.assertEqual(1, report.created)
        self.assertEqual(["stored-refresh"], refresh_calls)
        self.assertEqual(["fresh-access"], tokens_used)
        stored = self.repo.load_credential(self.account_id)
        self.assertEqual("fresh-access", stored.access_token)
        self.assertEqual("fresh-refresh", stored.refresh_token)
        self.assertEqual("ksk_after_refresh", stored.kiro_api_key)
        self.assertIn("api_key_refreshed", [e.kind for e in events if isinstance(e, WorkerEvent)])

    async def test_external_idp_sends_tokentype(self):
        self._save_credential(auth_method="external_idp", expires_at=stamp(3600))
        coordinator, events, progress = self._coordinator()
        seen = {}

        async def fake_ensure(transport, *, token, label, region, profile_arn, token_type, skip_if_labeled_exists):
            seen["token_type"] = token_type
            return ApiKeyResult(raw_key="ksk_x", profile_arn="arn:x", reused=False)

        import batch_login.account_login_coordinator as mod
        with unittest_patch(mod, "ensure_api_key", fake_ensure):
            await coordinator.extract_api_keys([self.account_id], progress=progress.append, event_sink=events.append)

        self.assertEqual("EXTERNAL_IDP", seen["token_type"])

    async def test_no_credential_is_skipped_not_fatal(self):
        coordinator, events, progress = self._coordinator()

        async def fake_ensure(*a, **k):
            raise AssertionError("ensure should not run without credential")

        import batch_login.account_login_coordinator as mod
        with unittest_patch(mod, "ensure_api_key", fake_ensure):
            report = await coordinator.extract_api_keys(
                [self.account_id], progress=progress.append, event_sink=events.append
            )

        self.assertEqual(1, report.skipped)
        self.assertEqual(0, report.created)
        self.assertIsNone(report.export_path)
        failed = [e for e in events if isinstance(e, WorkerEvent) and e.kind == "api_key_failed"]
        self.assertEqual("no_credential", failed[0].payload["code"])

    async def test_api_key_failure_does_not_abort_batch(self):
        self._save_credential(expires_at=stamp(3600))
        coordinator, events, progress = self._coordinator()

        async def fake_ensure(*a, **k):
            raise ApiKeyError("http_error", "create_api_key", False, "boom", status_code=403)

        import batch_login.account_login_coordinator as mod
        with unittest_patch(mod, "ensure_api_key", fake_ensure):
            report = await coordinator.extract_api_keys(
                [self.account_id], progress=progress.append, event_sink=events.append
            )

        self.assertEqual(1, report.failed)
        self.assertEqual(0, report.created)
        self.assertIsNone(self.repo.load_credential(self.account_id).kiro_api_key)
        self.assertTrue(self.transports[0].closed)

    async def test_proxy_enabled_uses_chain_transport_factory(self):
        # settings 打开代理:extract 应改用 ProxyChain 工厂而非默认直连工厂。
        self.settings = GuiSavedSettings(
            credential_path=str(self.root / "complete.json"),
            oidc_export_directory=str(self.root / "exports"),
            region="us-east-1",
            proxy_enabled=True,
            system_proxy="socks5://127.0.0.1:7890",
            home_proxies="socks5://u:p@9.9.9.9:1080",
        )
        self._save_credential(expires_at=stamp(3600))

        default_used = {"count": 0}

        def default_factory():
            default_used["count"] += 1
            return FakeTransport()

        coordinator = AccountLoginCoordinator(
            self.repo,
            SettingsStore(self.settings),
            api_key_transport_factory=default_factory,
            token_refresher=self._never_refresh,
            now=lambda: NOW,
        )

        chain_transports = []

        async def fake_ensure(transport, *, token, **k):
            chain_transports.append(transport)
            return ApiKeyResult(raw_key="ksk_chain", profile_arn="arn:x", reused=False)

        import batch_login.account_login_coordinator as mod
        with unittest_patch(mod, "ensure_api_key", fake_ensure):
            report = await coordinator.extract_api_keys(
                [self.account_id], progress=lambda _e: None, event_sink=lambda _e: None
            )

        self.assertEqual(1, report.created)
        # 默认直连工厂不应被调用(改走链式)
        self.assertEqual(0, default_used["count"])
        # 用的是 ChainedTransport
        from batch_login.proxy_chain import ChainedTransport
        self.assertIsInstance(chain_transports[0], ChainedTransport)

    async def test_home_proxies_override_pins_single_exit(self):
        self.settings = GuiSavedSettings(
            credential_path=str(self.root / "complete.json"),
            oidc_export_directory=str(self.root / "exports"),
            region="us-east-1",
            proxy_enabled=True,
            system_proxy="socks5://127.0.0.1:7890",
            home_proxies="socks5://u:p@1.1.1.1:1080\nsocks5://u:p@2.2.2.2:1080",
        )
        self._save_credential(expires_at=stamp(3600))

        coordinator = AccountLoginCoordinator(
            self.repo,
            SettingsStore(self.settings),
            token_refresher=self._never_refresh,
            now=lambda: NOW,
        )

        seen_transports = []

        async def fake_ensure(transport, *, token, **k):
            seen_transports.append(transport)
            return ApiKeyResult(raw_key="ksk_pinned", profile_arn="arn:x", reused=False)

        import batch_login.account_login_coordinator as mod
        with unittest_patch(mod, "ensure_api_key", fake_ensure):
            await coordinator.extract_api_keys(
                [self.account_id],
                progress=lambda _e: None,
                event_sink=lambda _e: None,
                home_proxies_override="socks5://u:p@2.2.2.2:1080",
            )

        # 覆盖为第二个家宽:transport 的 home 应固定是 2.2.2.2,不走 1.1.1.1
        self.assertEqual("2.2.2.2", seen_transports[0].home.host)

    async def test_login_and_extract_logs_in_only_missing_then_extracts(self):
        # 账号2:已有有效凭据(不需登录);账号1:无凭据(需先登录)
        second = self.repo.upsert_entries(
            [AccountEntry(2, "codeflow2-2", "pw", "https://d-9067123456.awsapps.com/start")],
            login_mode=LoginMode.ENTERPRISE,
            region="us-east-1",
        )[0]
        self.repo.save_credential(
            second.id,
            CredentialRecord(email="codeflow2-2", auth_method="idc", provider="E",
                             access_token="tok", expires_at=stamp(3600)),
        )
        coordinator = AccountLoginCoordinator(
            self.repo, SettingsStore(self.settings), now=lambda: NOW,
        )

        run_calls = []
        extract_calls = []
        phases = []

        async def fake_run(ids, **kwargs):
            run_calls.append(list(ids))
            return None

        async def fake_extract(ids, **kwargs):
            extract_calls.append(list(ids))
            return ApiKeyExtractionReport(len(ids), len(ids), 0, 0, 0, 0, None)

        coordinator.run = fake_run
        coordinator.extract_api_keys = fake_extract

        report = await coordinator.login_and_extract_api_keys(
            [self.account_id, second.id],
            event_sink=lambda e: phases.append(e) if getattr(e, "kind", "") == "api_key_phase" else None,
        )

        # 只有账号1(无凭据)进登录;提取覆盖两个账号
        self.assertEqual([[self.account_id]], run_calls)
        self.assertEqual([[self.account_id, second.id]], extract_calls)
        self.assertEqual(2, report.created)
        # 两个阶段事件都发了
        phase_names = [e.payload.get("phase") for e in phases]
        self.assertIn("login", phase_names)
        self.assertIn("extract", phase_names)

    async def test_login_and_extract_skips_login_when_all_valid(self):
        self._save_credential(expires_at=stamp(3600))
        coordinator = AccountLoginCoordinator(
            self.repo, SettingsStore(self.settings), now=lambda: NOW,
        )
        run_calls = []

        async def fake_run(ids, **kwargs):
            run_calls.append(list(ids))

        async def fake_extract(ids, **kwargs):
            return ApiKeyExtractionReport(len(ids), len(ids), 0, 0, 0, 0, None)

        coordinator.run = fake_run
        coordinator.extract_api_keys = fake_extract

        await coordinator.login_and_extract_api_keys([self.account_id])

        self.assertEqual([], run_calls)  # 已有有效凭据,不登录

    async def test_missing_refresh_material_uses_stored_token(self):
        # 过期但缺 refresh 材料:不刷新,直接用库存 token 尝试。
        self._save_credential(expires_at=stamp(-10), refresh_token=None, client_id=None)

        async def boom_refresh(*a, **k):
            raise AssertionError("must not refresh without material")

        coordinator, events, progress = self._coordinator(refresher=boom_refresh)
        tokens_used = []

        async def fake_ensure(transport, *, token, **k):
            tokens_used.append(token)
            return ApiKeyResult(raw_key="ksk_stored", profile_arn="arn:x", reused=False)

        import batch_login.account_login_coordinator as mod
        with unittest_patch(mod, "ensure_api_key", fake_ensure):
            report = await coordinator.extract_api_keys(
                [self.account_id], progress=progress.append, event_sink=events.append
            )

        self.assertEqual(0, report.refreshed)
        self.assertEqual(1, report.created)
        self.assertEqual(["stored-access"], tokens_used)


class _PipelineRuntime:
    """并发流水线用的假 runtime:open_for_concurrent + login_one。"""

    instances: list = []

    def __init__(self, form, emit):
        self.form = form
        self.emit = emit
        self.logged_in = []
        _PipelineRuntime.instances.append(self)

    async def open_for_concurrent(self):
        return True

    async def login_one(self, entry):
        self.logged_in.append(entry.account)
        return CredentialRecord(
            email=entry.account, auth_method="idc", provider="Enterprise",
            access_token="fresh-" + entry.account, refresh_token="r-" + entry.account,
            start_url=entry.start_url, region="us-east-1",
        )

    async def close(self):
        pass


class PipelineTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.repo = AccountRepository(self.root / "accounts.sqlite3", protector=Protector())
        self.accounts = self.repo.upsert_entries(
            [
                AccountEntry(1, "need-login", "pw1", "https://d-90aa.awsapps.com/start"),
                AccountEntry(2, "reuse-me", "pw2", "https://d-90bb.awsapps.com/start"),
            ],
            login_mode=LoginMode.ENTERPRISE, region="us-east-1",
        )
        self.settings = GuiSavedSettings(
            credential_path=str(self.root / "complete.json"),
            oidc_export_directory=str(self.root / "exports"),
            region="us-east-1",
        )
        _PipelineRuntime.instances = []

    async def asyncTearDown(self):
        self.temp.cleanup()

    async def test_pipeline_logs_in_then_extracts_and_reuses_valid_credential(self):
        # 第二个号已有有效凭据 → 跳过登录直接提取
        self.repo.save_credential(self.accounts[1].id, CredentialRecord(
            email="reuse-me", auth_method="idc", provider="Enterprise",
            access_token="already", refresh_token="already-r",
            start_url="https://d-90bb.awsapps.com/start", region="us-east-1",
        ))

        def transport_factory():
            return FakeTransport()

        coordinator = AccountLoginCoordinator(
            self.repo, SettingsStore(self.settings),
            runtime_factory=lambda f, e: _PipelineRuntime(f, e),
            api_key_transport_factory=transport_factory,
            token_refresher=self._no_refresh, now=lambda: NOW,
        )

        async def fake_ensure(transport, *, token, label, region, profile_arn, token_type, skip_if_labeled_exists):
            return ApiKeyResult(raw_key="ksk_" + label, profile_arn="arn:x", reused=False)

        events, progress = [], []
        import batch_login.account_login_coordinator as mod
        with unittest_patch(mod, "ensure_api_key", fake_ensure):
            report = await coordinator.login_and_extract_pipeline(
                [a.id for a in self.accounts], concurrency=3,
                progress=progress.append, event_sink=events.append,
            )

        # 一个登录、一个复用;两个都提到 key
        self.assertEqual(1, report.logged_in)
        self.assertEqual(1, report.reused)
        self.assertEqual(2, report.keys_created)
        self.assertEqual(0, report.login_failed)
        # 只登录了 need-login,reuse-me 跳过登录
        self.assertEqual(["need-login"], _PipelineRuntime.instances[0].logged_in)
        # 两号凭据都落库、都带 key
        self.assertEqual("ksk_need-login", self.repo.load_credential(self.accounts[0].id).kiro_api_key)
        self.assertEqual("ksk_reuse-me", self.repo.load_credential(self.accounts[1].id).kiro_api_key)
        # 进度事件分 login / apikey 两个阶段
        stages = {e.stage for e in progress if isinstance(e, LoginProgressEvent)}
        self.assertEqual({"login", "apikey"}, stages)

    async def test_pipeline_login_failure_skips_extract_and_counts(self):
        class FailRuntime(_PipelineRuntime):
            async def login_one(self, entry):
                raise RuntimeError("login boom")

        def transport_factory():
            return FakeTransport()

        coordinator = AccountLoginCoordinator(
            self.repo, SettingsStore(self.settings),
            runtime_factory=lambda f, e: FailRuntime(f, e),
            api_key_transport_factory=transport_factory,
            token_refresher=self._no_refresh, now=lambda: NOW,
        )

        async def fake_ensure(transport, **k):
            raise AssertionError("登录失败不应提取")

        import batch_login.account_login_coordinator as mod
        with unittest_patch(mod, "ensure_api_key", fake_ensure):
            report = await coordinator.login_and_extract_pipeline(
                [self.accounts[0].id], concurrency=2,
            )

        self.assertEqual(0, report.logged_in)
        self.assertEqual(1, report.login_failed)
        self.assertEqual(0, report.keys_created)

    @staticmethod
    async def _no_refresh(*args, **kwargs):
        raise AssertionError("refresh should not be called")


# 轻量 patch 上下文(避免依赖 unittest.mock 对 async 的额外配置)。
class unittest_patch:
    def __init__(self, module, name, value):
        self.module = module
        self.name = name
        self.value = value

    def __enter__(self):
        self.original = getattr(self.module, self.name)
        setattr(self.module, self.name, self.value)
        return self.value

    def __exit__(self, *exc):
        setattr(self.module, self.name, self.original)
        return False


if __name__ == "__main__":
    unittest.main()
