import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.account_login_coordinator import AccountLoginCoordinator
from batch_login.account_repository import AccountRepository, LifecycleStatus
from batch_login.credential_models import CredentialRecord
from batch_login.credential_store import CredentialStore
from batch_login.gui_settings import GuiSavedSettings
from batch_login.models import AccountEntry, LoginMode


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


if __name__ == "__main__": unittest.main()
