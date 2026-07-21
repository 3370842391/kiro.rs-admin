import sys
import tempfile
import unittest
from dataclasses import asdict
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.account_login_coordinator import (
    AccountLoginCoordinator,
    LoginProgressEvent,
)
from batch_login.account_repository import AccountRepository, LifecycleStatus
from batch_login.credential_models import CredentialRecord
from batch_login.credential_store import CredentialStore
from batch_login.gui_settings import GuiSavedSettings
from batch_login.models import AccountEntry, LoginMode
from batch_login.password_vault import PasswordStatus
from batch_login.worker_events import WorkerEvent


class Protector:
    def protect(self, value): return b"p:" + value[::-1]
    def unprotect(self, value): return value.removeprefix(b"p:")[::-1]


class SettingsStore:
    def __init__(self, settings): self.settings = settings
    def load(self): return self.settings


class Exporter:
    def __init__(self): self.records = None
    def export(self, records, **kwargs):
        self.records = list(records)
        return type("Report", (), {"record_count": len(self.records), "merged_path": Path("out.json"), "account_paths": ()})()


class Runtime:
    """并发 run() 用的假 runtime:open_for_concurrent + login_one。

    calls 记录每次 login_one 的账号(顺序不保证,并发);测试用 set 比对。
    """
    def __init__(self, form, emit, calls): self.form=form; self.calls=calls
    async def open_for_concurrent(self): return True
    async def login_one(self, entry):
        self.calls.append(entry.account)
        return CredentialRecord(
            email=entry.account, auth_method="idc", provider="Enterprise",
            refresh_token="refresh-" + entry.account, start_url=entry.start_url,
            region="us-east-1",
        )
    async def close(self): pass


class CoordinatorTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.repo = AccountRepository(self.root / "accounts.sqlite3", protector=Protector())
        self.accounts = self.repo.upsert_entries(
            [AccountEntry(1, "first", "one", "https://portal/one"), AccountEntry(2, "second", "two", "https://portal/two")],
            login_mode=LoginMode.ENTERPRISE, region="us-east-1",
        )
        self.settings = GuiSavedSettings(
            credential_path=str(self.root / "complete.json"),
            password_vault_path=str(self.root / "passwords.sqlite3"),
            oidc_export_directory=str(self.root / "exports"),
            admin_key="admin-key-secret",
        )

    async def asyncTearDown(self): self.temp.cleanup()

    async def test_valid_credential_is_reused_without_login(self):
        self.repo.save_credential(self.accounts[0].id, CredentialRecord(
            email="first", auth_method="idc", provider="Enterprise", refresh_token="existing"
        ))
        calls=[]; exporter=Exporter()
        coordinator = AccountLoginCoordinator(self.repo, SettingsStore(self.settings), exporter=exporter, runtime_factory=lambda f,e: Runtime(f,e,calls))

        report = await coordinator.run([self.accounts[0].id])

        self.assertEqual([], calls)
        self.assertEqual("existing", exporter.records[0].refresh_token)
        self.assertEqual(1, report.exported)

    async def test_missing_credentials_login_and_sync_before_export(self):
        calls=[]; exporter=Exporter()
        coordinator = AccountLoginCoordinator(self.repo, SettingsStore(self.settings), exporter=exporter, runtime_factory=lambda f,e: Runtime(f,e,calls))

        report = await coordinator.run([item.id for item in self.accounts])

        self.assertEqual({"first", "second"}, set(calls))  # 并发,顺序不保证
        self.assertEqual(2, report.logged_in)
        self.assertEqual(2, report.exported)
        self.assertEqual("refresh-first", self.repo.load_credential(self.accounts[0].id).refresh_token)

    async def test_login_only_stores_credentials_without_exporting_files(self):
        calls=[]; exporter=Exporter()
        coordinator = AccountLoginCoordinator(self.repo, SettingsStore(self.settings), exporter=exporter, runtime_factory=lambda f,e: Runtime(f,e,calls))

        report = await coordinator.run(
            [item.id for item in self.accounts], export_files=False
        )

        # 登录发生、凭据落库,但不写任何文件、exported 记 0
        self.assertEqual({"first", "second"}, set(calls))  # 并发,顺序不保证
        self.assertEqual(2, report.logged_in)
        self.assertEqual(0, report.exported)
        self.assertIsNone(exporter.records)
        self.assertEqual(
            "refresh-first", self.repo.load_credential(self.accounts[0].id).refresh_token
        )

    async def test_concurrent_login_persists_each_account_and_reports_success(self):
        """并发 login_one:每号登完立刻存库 + 报 success(terminal)。"""
        events = []
        coordinator = AccountLoginCoordinator(
            self.repo, SettingsStore(self.settings),
            exporter=Exporter(), runtime_factory=lambda f, e: Runtime(f, e, []),
        )

        report = await coordinator.run(
            [item.id for item in self.accounts],
            progress=events.append,
            event_sink=lambda _e: None,
            export_files=False,
        )

        self.assertEqual(2, report.logged_in)
        self.assertEqual("refresh-first", self.repo.load_credential(self.accounts[0].id).refresh_token)
        self.assertEqual("refresh-second", self.repo.load_credential(self.accounts[1].id).refresh_token)
        success = [e for e in events if isinstance(e, LoginProgressEvent) and e.status == "success"]
        self.assertEqual(2, len(success))

    async def test_cancellation_keeps_done_account_and_marks_rest_cancelled(self):
        """终止:已登成功的号保留凭据,其余标记 cancelled,run() 抛 CancelledError。"""
        import asyncio

        gate = asyncio.Event()

        class CancelRuntime:
            def __init__(self, form, emit):
                self.form = form
            async def open_for_concurrent(self): return True
            async def login_one(self, entry):
                if entry.account == "first":
                    gate.set()
                    return CredentialRecord(
                        email="first", auth_method="idc", provider="Enterprise",
                        refresh_token="refresh-first", start_url=entry.start_url, region="us-east-1",
                    )
                # 第二个号一直挂着,等外部取消
                await asyncio.sleep(3600)
                raise AssertionError("不应到达")
            async def close(self): pass

        coordinator = AccountLoginCoordinator(
            self.repo, SettingsStore(self.settings),
            exporter=Exporter(), runtime_factory=lambda f, e: CancelRuntime(f, e),
        )

        task = asyncio.ensure_future(
            coordinator.run([item.id for item in self.accounts], concurrency=2, export_files=False)
        )
        await gate.wait()          # 等第一个号登完落库
        await asyncio.sleep(0)     # 让 save/notify 跑完
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task

        # 第一个号凭据保留;第二个号被标记(cancelled),未写入凭据
        self.assertEqual("refresh-first", self.repo.load_credential(self.accounts[0].id).refresh_token)
        self.assertIsNone(self.repo.load_credential(self.accounts[1].id))
        self.assertIsNone(self.repo.load_credential(self.accounts[1].id))

    async def test_progress_covers_reused_success_and_failed_accounts_once(self):
        third = self.repo.upsert_entries(
            [
                AccountEntry(
                    3,
                    "third-secret-account",
                    "never-log-this-password",
                    "https://portal/three",
                )
            ],
            login_mode=LoginMode.ENTERPRISE,
            region="us-east-1",
        )[0]
        self.repo.save_credential(
            self.accounts[0].id,
            CredentialRecord(
                email="first",
                auth_method="idc",
                provider="Enterprise",
                refresh_token="existing-refresh-secret",
                access_token="existing-access-secret",
                client_secret="existing-client-secret",
            ),
        )

        class PartialRuntime:
            def __init__(self, form, _emit):
                self.form = form
            async def open_for_concurrent(self): return True
            async def login_one(self, entry):
                # second 成功,third 失败(抛异常)
                if entry.account == "second":
                    return CredentialRecord(
                        email=entry.account, auth_method="idc", provider="Enterprise",
                        refresh_token="new-refresh-secret",
                        start_url=entry.start_url, region="us-east-1",
                    )
                raise RuntimeError("password=never-log-this-password login failed")
            async def close(self):
                pass

        progress: list[LoginProgressEvent] = []
        coordinator = AccountLoginCoordinator(
            self.repo,
            SettingsStore(self.settings),
            exporter=Exporter(),
            runtime_factory=PartialRuntime,
        )

        report = await coordinator.run(
            [self.accounts[0].id, self.accounts[1].id, third.id],
            progress=progress.append,
        )

        statuses = {
            account_id: [
                event.status
                for event in progress
                if event.account_id == account_id
            ]
            for account_id in (
                self.accounts[0].id,
                self.accounts[1].id,
                third.id,
            )
        }
        # 第一个号:有效凭据复用(不经登录);顺序确定
        self.assertEqual(["waiting", "reused"], statuses[self.accounts[0].id])
        # 并发下 second/third 的 running/终态穿插,只断言集合与终态
        self.assertEqual({"waiting", "running", "success"}, set(statuses[self.accounts[1].id]))
        self.assertEqual({"waiting", "running", "failed"}, set(statuses[third.id]))
        terminal = [
            event
            for event in progress
            if event.status in {"reused", "success", "failed"}
        ]
        # completed 计数单调递增到 3(并发下终态顺序不定)
        self.assertEqual([1, 2, 3], sorted(event.completed for event in terminal))
        self.assertTrue(all(event.total == 3 for event in progress))
        third_terminal = next(e for e in terminal if e.account_id == third.id)
        self.assertEqual("login_failed", third_terminal.code)
        self.assertEqual(1, report.reused)
        self.assertEqual(1, report.logged_in)
        self.assertEqual(1, report.failed)

        serialized = repr([asdict(event) for event in progress])
        for secret in (
            "third-secret-account",
            "never-log-this-password",
            "existing-refresh-secret",
            "existing-access-secret",
            "existing-client-secret",
            "new-refresh-secret",
            "admin-key-secret",
        ):
            self.assertNotIn(secret, serialized)

    async def test_runtime_batch_failure_continues_with_next_login_mode(self):
        microsoft = self.repo.upsert_entries(
            [
                AccountEntry(
                    3,
                    "microsoft-user@example.com",
                    "microsoft-password-secret",
                )
            ],
            login_mode=LoginMode.MICROSOFT,
            region="us-east-1",
        )[0]
        opened = []

        class ModeRuntime:
            def __init__(self, form, _emit):
                self.form = form
            async def open_for_concurrent(self):
                opened.append(self.form.mode)
                return self.form.mode is LoginMode.ENTERPRISE
            async def login_one(self, entry):
                if self.form.mode is LoginMode.ENTERPRISE:
                    raise RuntimeError(
                        "password=enterprise-password-secret "
                        "refreshToken=enterprise-refresh-secret"
                    )
                return CredentialRecord(
                    email=entry.account, auth_method="social", provider="Google",
                    refresh_token="microsoft-refresh-secret", region="us-east-1",
                )
            async def close(self):
                pass

        progress: list[LoginProgressEvent] = []
        runtime_events = []
        coordinator = AccountLoginCoordinator(
            self.repo,
            SettingsStore(self.settings),
            exporter=Exporter(),
            runtime_factory=ModeRuntime,
        )

        report = await coordinator.run(
            [self.accounts[0].id, microsoft.id],
            progress=progress.append,
            event_sink=runtime_events.append,
        )

        # 两种 mode 各开一个 runtime(顺序按 LoginMode 枚举)
        self.assertEqual([LoginMode.ENTERPRISE, LoginMode.MICROSOFT], opened)
        self.assertEqual(1, report.failed)
        self.assertEqual(1, report.logged_in)
        terminal = [
            event
            for event in progress
            if event.status in {"reused", "success", "failed"}
        ]
        self.assertEqual([1, 2], sorted(event.completed for event in terminal))
        ent_terminal = next(e for e in terminal if e.account_id == self.accounts[0].id)
        ms_terminal = next(e for e in terminal if e.account_id == microsoft.id)
        self.assertEqual("failed", ent_terminal.status)
        self.assertEqual("login_failed", ent_terminal.code)
        self.assertEqual("automatic_login", ent_terminal.stage)
        self.assertEqual("success", ms_terminal.status)
        self.assertEqual([], runtime_events)

    async def test_runtime_worker_events_are_forwarded_to_explicit_sink(self):
        emitted = WorkerEvent("browser_stage", {"stage": "portal_init"})
        received = []

        class EventRuntime:
            def __init__(self, form, emit):
                self.form = form
                self.emit = emit
            async def open_for_concurrent(self): return True
            async def login_one(self, entry):
                self.emit(emitted)
                return CredentialRecord(
                    email=entry.account, auth_method="idc", provider="Enterprise",
                    refresh_token="refresh-" + entry.account,
                    start_url=entry.start_url, region="us-east-1",
                )
            async def close(self): pass

        coordinator = AccountLoginCoordinator(
            self.repo,
            SettingsStore(self.settings),
            exporter=Exporter(),
            runtime_factory=EventRuntime,
        )

        await coordinator.run(
            [self.accounts[0].id], event_sink=received.append
        )

        self.assertEqual([emitted], received)

    async def test_sold_account_is_rejected(self):
        self.repo.mark_sold([self.accounts[0].id], "客户")
        coordinator = AccountLoginCoordinator(self.repo, SettingsStore(self.settings), exporter=Exporter(), runtime_factory=lambda f,e: Runtime(f,e,[]))

        with self.assertRaisesRegex(ValueError, "已售出"):
            await coordinator.run([self.accounts[0].id])

    async def test_confirmed_password_syncs_even_when_runtime_later_fails(self):
        class FailingRuntime:
            def __init__(self, form, _emit):
                self.form = form
            async def open_for_concurrent(self): return True
            async def login_one(self, _entry):
                Path(self.form.password_vault_path).touch()
                raise RuntimeError("rs_import_failed")
            async def close(self):
                pass

        vault = SimpleNamespace(
            records=lambda: [
                SimpleNamespace(
                    account="first",
                    scope="us-east-1/d-first",
                    password="generated-current-password",
                    status=PasswordStatus.CONFIRMED,
                )
            ]
        )
        coordinator = AccountLoginCoordinator(
            self.repo,
            SettingsStore(self.settings),
            exporter=Exporter(),
            runtime_factory=FailingRuntime,
        )

        with patch(
            "batch_login.account_login_coordinator.PasswordVault",
            return_value=vault,
        ):
            report = await coordinator.run([self.accounts[0].id])

        saved = self.repo.get(self.accounts[0].id, include_secrets=True)
        self.assertEqual(1, report.failed)
        self.assertEqual(
            "generated-current-password",
            saved.current_password,
        )

    def test_confirmed_passwords_match_account_and_start_url_scope(self):
        shared = self.repo.upsert_entries(
            [
                AccountEntry(
                    1,
                    "shared",
                    "one",
                    "https://d-one.awsapps.com/start",
                ),
                AccountEntry(
                    2,
                    "shared",
                    "two",
                    "https://d-two.awsapps.com/start",
                ),
            ],
            login_mode=LoginMode.ENTERPRISE,
            region="us-east-1",
        )
        Path(self.settings.password_vault_path).touch()
        vault = SimpleNamespace(
            records=lambda: [
                SimpleNamespace(
                    account="shared",
                    scope="us-east-1/d-one",
                    password="password-for-one",
                    status=PasswordStatus.CONFIRMED,
                ),
                SimpleNamespace(
                    account="shared",
                    scope="us-east-1/d-two",
                    password="password-for-two",
                    status=PasswordStatus.CONFIRMED,
                ),
            ]
        )
        coordinator = AccountLoginCoordinator(
            self.repo,
            SettingsStore(self.settings),
        )

        with patch(
            "batch_login.account_login_coordinator.PasswordVault",
            return_value=vault,
        ):
            coordinator._sync_confirmed_passwords(
                SimpleNamespace(
                    password_vault_path=self.settings.password_vault_path
                ),
                shared,
            )

        first = self.repo.get(shared[0].id, include_secrets=True)
        second = self.repo.get(shared[1].id, include_secrets=True)
        self.assertEqual("password-for-one", first.current_password)
        self.assertEqual("password-for-two", second.current_password)

    def test_ambiguous_new_portal_password_history_is_not_guessed(self):
        shared = self.repo.upsert_entries(
            [
                AccountEntry(
                    1,
                    "shared-new",
                    "one",
                    "https://ssoins-one.portal.us-east-1.app.aws/",
                ),
                AccountEntry(
                    2,
                    "shared-new",
                    "two",
                    "https://ssoins-two.portal.us-east-1.app.aws/",
                ),
            ],
            login_mode=LoginMode.ENTERPRISE,
            region="us-east-1",
        )
        Path(self.settings.password_vault_path).touch()
        vault = SimpleNamespace(
            records=lambda: [
                SimpleNamespace(
                    account="shared-new",
                    scope="us-east-1/d-one",
                    password="password-for-one",
                    status=PasswordStatus.CONFIRMED,
                ),
                SimpleNamespace(
                    account="shared-new",
                    scope="us-east-1/d-two",
                    password="password-for-two",
                    status=PasswordStatus.CONFIRMED,
                ),
            ]
        )
        coordinator = AccountLoginCoordinator(
            self.repo,
            SettingsStore(self.settings),
        )

        with patch(
            "batch_login.account_login_coordinator.PasswordVault",
            return_value=vault,
        ):
            count = coordinator._sync_confirmed_passwords(
                self.settings.password_vault_path,
                shared,
            )

        self.assertEqual(0, count)
        self.assertIsNone(
            self.repo.get(shared[0].id, include_secrets=True).current_password
        )
        self.assertIsNone(
            self.repo.get(shared[1].id, include_secrets=True).current_password
        )

    def test_public_password_recovery_uses_saved_vault_path(self):
        Path(self.settings.password_vault_path).touch()
        vault = SimpleNamespace(
            records=lambda: [
                SimpleNamespace(
                    account="first",
                    scope="us-east-1/d-first",
                    password="recovered-password",
                    status=PasswordStatus.CONFIRMED,
                )
            ]
        )
        coordinator = AccountLoginCoordinator(
            self.repo,
            SettingsStore(self.settings),
        )

        with patch(
            "batch_login.account_login_coordinator.PasswordVault",
            return_value=vault,
        ):
            count = coordinator.sync_saved_passwords(
                [self.accounts[0].id]
            )

        recovered = self.repo.get(
            self.accounts[0].id, include_secrets=True
        )
        self.assertEqual(1, count)
        self.assertEqual("recovered-password", recovered.current_password)


if __name__ == "__main__": unittest.main()
