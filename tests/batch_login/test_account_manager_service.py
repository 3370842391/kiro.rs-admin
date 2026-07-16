import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.account_manager_service import (
    AccountManagerService,
    AccountManagerServiceError,
)
from batch_login.account_repository import (
    AccountRepository,
    CredentialStatus,
    LifecycleStatus,
    LoginStatus,
)
from batch_login.models import LoginMode


class FakeProtector:
    def protect(self, value: bytes) -> bytes:
        return b"protected:" + value[::-1]

    def unprotect(self, value: bytes) -> bytes:
        return value.removeprefix(b"protected:")[::-1]


class AccountManagerServiceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.repo = AccountRepository(
            Path(self.temp.name) / "accounts.sqlite3",
            protector=FakeProtector(),
        )
        self.service = AccountManagerService(self.repo)

    def tearDown(self):
        self.temp.cleanup()

    def import_accounts(self):
        raw = "\n".join(
            [
                "first|one-time-1|https://portal.example/one",
                "second|one-time-2|https://portal.example/two",
            ]
        )
        preview = self.service.preview_import(
            raw,
            "{account}|{password}|{start_url}",
            LoginMode.ENTERPRISE,
        )
        return self.service.confirm_import(preview, region="us-east-1")

    def test_preview_then_confirm_imports_only_valid_entries(self):
        preview = self.service.preview_import(
            "\n".join(
                [
                    "first|one|https://portal.example/one",
                    "broken",
                    "FIRST|duplicate|https://portal.example/two",
                ]
            ),
            "{account}|{password}|{start_url}",
            LoginMode.ENTERPRISE,
        )

        report = self.service.confirm_import(preview, region="us-east-1")

        self.assertEqual(1, report.saved)
        self.assertEqual(2, len(preview.issues))
        self.assertEqual(1, len(self.repo.list_accounts()))

    def test_search_and_status_filters(self):
        report = self.import_accounts()
        first, second = report.accounts
        self.repo.mark_sold([second.id], "客户 Beta")

        self.assertEqual(
            [first.id],
            [item.id for item in self.service.list_accounts(query="FIRST")],
        )
        self.assertEqual(
            [second.id],
            [
                item.id
                for item in self.service.list_accounts(
                    query="beta", status="sold"
                )
            ],
        )
        self.assertEqual(
            [first.id],
            [item.id for item in self.service.list_accounts(status="managed")],
        )

    def test_selection_survives_filtering_and_supports_invert(self):
        first, second = self.import_accounts().accounts

        self.service.set_selected([first.id])
        self.service.invert_visible([first.id, second.id])

        self.assertEqual({second.id}, self.service.selected_ids)
        self.service.toggle_selected(first.id)
        self.assertEqual({first.id, second.id}, self.service.selected_ids)
        self.service.clear_selected()
        self.assertEqual(set(), self.service.selected_ids)

    def test_update_password_rejects_sold_accounts(self):
        first, _second = self.import_accounts().accounts
        self.repo.mark_sold([first.id], "客户 A")

        with self.assertRaisesRegex(AccountManagerServiceError, "已售出"):
            self.service.update_password([first.id], "current-password")

        self.assertIsNone(
            self.repo.get(first.id, include_secrets=True).current_password
        )

    def test_export_requires_current_password_without_fallback(self):
        first, _second = self.import_accounts().accounts
        calls = []

        with self.assertRaisesRegex(AccountManagerServiceError, "当前密码"):
            self.service.export_text(
                [first.id],
                template="{account}----{password}----{start_url}",
                writer=calls.append,
                note="客户 A",
                mark_sold=True,
            )

        self.assertEqual([], calls)
        self.assertIs(
            LifecycleStatus.MANAGED,
            self.repo.get(first.id).lifecycle_status,
        )

    def test_export_marks_sold_only_after_writer_succeeds(self):
        first, second = self.import_accounts().accounts
        self.service.update_password([first.id, second.id], "current-password")
        captured = []

        report = self.service.export_text(
            [first.id, second.id],
            template="{account}----{password}----{start_url}",
            writer=captured.append,
            note="客户 A",
            mark_sold=True,
        )

        self.assertEqual(2, report.exported)
        self.assertIn(
            "first----current-password----https://portal.example/one",
            captured[0],
        )
        for account_id in (first.id, second.id):
            saved = self.repo.get(account_id)
            self.assertIs(LifecycleStatus.SOLD, saved.lifecycle_status)
            self.assertEqual("客户 A", saved.note)
            self.assertIsNotNone(saved.last_exported_at)

    def test_writer_failure_leaves_accounts_managed(self):
        first, _second = self.import_accounts().accounts
        self.service.update_password([first.id], "current-password")

        def fail(_text):
            raise OSError("private path failed")

        with self.assertRaises(AccountManagerServiceError) as raised:
            self.service.export_text(
                [first.id],
                template="{account}----{password}----{start_url}",
                writer=fail,
                note="客户 A",
                mark_sold=True,
            )

        self.assertNotIn("private path", str(raised.exception))
        self.assertIs(
            LifecycleStatus.MANAGED,
            self.repo.get(first.id).lifecycle_status,
        )


if __name__ == "__main__":
    unittest.main()
