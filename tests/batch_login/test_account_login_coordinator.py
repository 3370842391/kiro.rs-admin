import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.account_login_coordinator import AccountLoginCoordinator
from batch_login.account_repository import AccountRepository, LifecycleStatus
from batch_login.credential_models import CredentialRecord
from batch_login.credential_store import CredentialStore
from batch_login.gui_settings import GuiSavedSettings
from batch_login.models import AccountEntry, LoginMode
from batch_login.password_vault import PasswordStatus


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
    def __init__(self, form, emit, calls): self.form=form; self.calls=calls
    async def run(self, entries):
        self.calls.append([item.account for item in entries])
        for item in entries:
            CredentialStore(Path(self.form.credential_path)).append(CredentialRecord(
                email=item.account, auth_method="idc", provider="Enterprise",
                refresh_token="refresh-" + item.account, start_url=item.start_url,
                region="us-east-1",
            ))
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

        self.assertEqual([["first", "second"]], calls)
        self.assertEqual(2, report.logged_in)
        self.assertEqual(2, report.exported)
        self.assertEqual("refresh-first", self.repo.load_credential(self.accounts[0].id).refresh_token)

    async def test_sold_account_is_rejected(self):
        self.repo.mark_sold([self.accounts[0].id], "客户")
        coordinator = AccountLoginCoordinator(self.repo, SettingsStore(self.settings), exporter=Exporter(), runtime_factory=lambda f,e: Runtime(f,e,[]))

        with self.assertRaisesRegex(ValueError, "已售出"):
            await coordinator.run([self.accounts[0].id])

    async def test_confirmed_password_syncs_even_when_runtime_later_fails(self):
        class FailingRuntime:
            def __init__(self, form, _emit):
                self.form = form

            async def run(self, _entries):
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
