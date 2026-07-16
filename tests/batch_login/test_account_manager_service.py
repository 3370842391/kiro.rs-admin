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

    def test_legacy_login_format_uses_uniform_enterprise_start_url(self):
        preview = self.service.preview_import(
            "\n".join(
                (
                    "login = legacy-user-248 / onetime password = Fake!Pass01",
                    "login = legacy-user-249 / onetime password = "
                    "Synthetic/%#Password-j-t4M6XHj7)With&Symbols<02",
                )
            ),
            "login = {account} / onetime password = {password}",
            LoginMode.ENTERPRISE,
            default_start_url="https://portal.example/start",
        )

        self.assertEqual([], preview.issues)
        self.assertEqual(2, len(preview.entries))
        self.assertEqual("legacy-user-248", preview.entries[0].account)
        self.assertEqual("Fake!Pass01", preview.entries[0].password)
        self.assertEqual("legacy-user-249", preview.entries[1].account)
        self.assertEqual(
            "Synthetic/%#Password-j-t4M6XHj7)With&Symbols<02",
            preview.entries[1].password,
        )
        self.assertEqual(
            "https://portal.example/start",
            preview.entries[0].start_url,
        )
        self.assertEqual(
            "https://portal.example/start",
            preview.entries[1].start_url,
        )

    def test_per_line_start_url_overrides_uniform_start_url(self):
        preview = self.service.preview_import(
            "enterprise-user|secret|https://portal.example/per-account",
            "{account}|{password}|{start_url}",
            LoginMode.ENTERPRISE,
            default_start_url="https://portal.example/uniform",
        )

        self.assertEqual([], preview.issues)
        self.assertEqual(
            "https://portal.example/per-account",
            preview.entries[0].start_url,
        )

    def test_blank_per_line_start_url_falls_back_to_uniform_start_url(self):
        preview = self.service.preview_import(
            "enterprise-user|secret|",
            "{account}|{password}|{start_url}",
            LoginMode.ENTERPRISE,
            default_start_url="https://portal.example/uniform",
        )

        self.assertEqual([], preview.issues)
        self.assertEqual(1, len(preview.entries))
        self.assertEqual(
            "https://portal.example/uniform",
            preview.entries[0].start_url,
        )

    def test_enterprise_import_without_any_start_url_is_rejected(self):
        preview = self.service.preview_import(
            "login = enterprise-user / onetime password = secret",
            "login = {account} / onetime password = {password}",
            LoginMode.ENTERPRISE,
        )

        self.assertEqual([], preview.entries)
        self.assertEqual(1, len(preview.issues))
        self.assertEqual("missing_start_url", preview.issues[0].code)

    def test_uniform_enterprise_start_url_must_be_safe_https(self):
        for start_url in (
            "http://portal.example/start",
            "https://user:password@portal.example/start",
            "https://portal.example:bad/start",
            "https://portal example/start",
            "https://portal.example/has space",
            "https://../start",
            "https://.example/start",
            "https://portal.example:0/start",
        ):
            with self.subTest(start_url=start_url):
                with self.assertRaisesRegex(
                    AccountManagerServiceError,
                    "HTTPS",
                ):
                    self.service.preview_import(
                        "login = enterprise-user / onetime password = secret",
                        "login = {account} / onetime password = {password}",
                        LoginMode.ENTERPRISE,
                        default_start_url=start_url,
                    )

    def test_invalid_per_line_start_url_does_not_fall_back_to_uniform_url(self):
        preview = self.service.preview_import(
            "enterprise-user|secret|https://portal example/start",
            "{account}|{password}|{start_url}",
            LoginMode.ENTERPRISE,
            default_start_url="https://portal.example/uniform",
        )

        self.assertEqual([], preview.entries)
        self.assertEqual(1, len(preview.issues))
        self.assertEqual("invalid_start_url", preview.issues[0].code)

    def test_microsoft_import_does_not_require_start_url(self):
        preview = self.service.preview_import(
            "user@example.com|secret",
            "{account}|{password}",
            LoginMode.MICROSOFT,
        )

        self.assertEqual([], preview.issues)
        self.assertEqual(1, len(preview.entries))
        self.assertIsNone(preview.entries[0].start_url)

    def test_microsoft_import_allows_blank_start_url_field(self):
        preview = self.service.preview_import(
            "user@example.com|secret|",
            "{account}|{password}|{start_url}",
            LoginMode.MICROSOFT,
        )

        self.assertEqual([], preview.issues)
        self.assertEqual(1, len(preview.entries))
        self.assertIsNone(preview.entries[0].start_url)

    def test_saved_start_urls_support_default_deduplication_and_delete(self):
        first = self.service.save_start_url(
            "https://portal.example/one/",
            make_default=True,
        )
        second = self.service.save_start_url(
            "https://portal.example/one",
        )
        third = self.service.save_start_url(
            "https://portal.example/two",
        )

        self.assertEqual(
            ("https://portal.example/one/",),
            first.urls,
        )
        self.assertEqual(first, second)
        self.assertEqual(
            (
                "https://portal.example/one/",
                "https://portal.example/two",
            ),
            third.urls,
        )
        self.assertEqual(
            "https://portal.example/one/",
            third.default_url,
        )

        updated = self.service.set_default_start_url(
            "https://portal.example/two"
        )
        self.assertEqual(
            "https://portal.example/two",
            updated.default_url,
        )

        remaining = self.service.delete_start_url(
            "https://portal.example/two/"
        )
        self.assertEqual(
            ("https://portal.example/one/",),
            remaining.urls,
        )
        self.assertEqual(
            "https://portal.example/one/",
            remaining.default_url,
        )

    def test_saved_start_url_rejects_unsafe_value(self):
        with self.assertRaisesRegex(AccountManagerServiceError, "HTTPS"):
            self.service.save_start_url(
                "http://portal.example/start",
                make_default=True,
            )

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

    def test_export_uses_initial_password_before_first_login(self):
        first, _second = self.import_accounts().accounts
        calls = []

        report = self.service.export_text(
            [first.id],
            template="{account}----{password}----{start_url}",
            writer=calls.append,
            note="客户 A",
            mark_sold=False,
        )

        self.assertEqual(1, report.exported)
        self.assertEqual(
            "first----one-time-1----https://portal.example/one",
            calls[0],
        )
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
