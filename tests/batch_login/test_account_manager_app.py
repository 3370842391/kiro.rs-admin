import inspect
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.account_manager_app import (
    AccountManagerApp,
    atomic_write_text,
    clear_secret_vars,
    select_range_ids,
)


class FakeVar:
    def __init__(self, value):
        self.value = value

    def set(self, value):
        self.value = value

    def get(self):
        return self.value


class AccountManagerAppTests(unittest.TestCase):
    def test_main_table_contains_management_columns(self):
        self.assertEqual(
            (
                "checked",
                "account",
                "password",
                "start_url",
                "login_status",
                "credential_status",
                "lifecycle_status",
                "note",
                "updated_at",
            ),
            AccountManagerApp.TABLE_COLUMNS,
        )
        self.assertEqual(
            "{account}----{password}----{start_url}",
            AccountManagerApp.DEFAULT_EXPORT_TEMPLATE,
        )
        self.assertEqual(
            "一键登录导出 JSON", AccountManagerApp.PRIMARY_ACTION_LABEL
        )
        self.assertEqual(
            "login = {account} / onetime password = {password}",
            AccountManagerApp.INPUT_TEMPLATE,
        )
        self.assertEqual(
            (
                "login = {account} / onetime password = {password}",
                "{account}----{password}",
                "{account}|{password}|{start_url}",
            ),
            AccountManagerApp.INPUT_TEMPLATE_PRESETS,
        )

    def test_password_dialog_clear_removes_both_plaintext_values(self):
        initial = FakeVar("one-time-secret")
        current = FakeVar("current-secret")

        clear_secret_vars(initial, current)

        self.assertEqual("", initial.get())
        self.assertEqual("", current.get())

    def test_import_confirmation_always_reparses_current_fields(self):
        source = inspect.getsource(AccountManagerApp.open_import_dialog)

        self.assertIn("result = parse_preview()", source)
        self.assertNotIn('result = state.get("preview")', source)
        self.assertLess(source.index("preview_box.delete"), source.index("try:"))
        self.assertIn('summary.set("解析失败")', source)

    def test_drag_range_selection_is_order_independent(self):
        rows = ["10", "11", "12", "13"]

        self.assertEqual({11, 12, 13}, select_range_ids(rows, "11", "13"))
        self.assertEqual({11, 12, 13}, select_range_ids(rows, "13", "11"))
        self.assertEqual(set(), select_range_ids(rows, "missing", "11"))

    def test_atomic_text_write_replaces_target_without_temp_residue(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "export.txt"
            path.write_text("old", encoding="utf-8")

            atomic_write_text(path, "账号----密码----URL")

            self.assertEqual(
                "账号----密码----URL\n",
                path.read_text(encoding="utf-8"),
            )
            self.assertEqual([], list(path.parent.glob(".*.tmp")))

    def test_atomic_text_write_failure_keeps_old_target(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "export.txt"
            path.write_text("old", encoding="utf-8")

            with patch(
                "batch_login.account_manager_app.os.replace",
                side_effect=OSError("private path"),
            ):
                with self.assertRaises(OSError):
                    atomic_write_text(path, "new-secret")

            self.assertEqual("old", path.read_text(encoding="utf-8"))
            self.assertEqual([], list(path.parent.glob(".*.tmp")))


if __name__ == "__main__":
    unittest.main()
