import inspect
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import batch_login.account_manager_app as account_manager_module
from batch_login.account_manager_app import (
    AccountManagerApp,
    atomic_write_text,
    clear_secret_vars,
    password_cell_text,
)
from batch_login.account_login_coordinator import LoginProgressEvent
from batch_login.account_repository import CredentialStatus, LoginStatus
from batch_login.worker_events import WorkerEvent

format_worker_event = getattr(
    account_manager_module, "format_worker_event", lambda _event: ""
)
json_status_text = getattr(
    account_manager_module, "json_status_text", lambda *_args: ""
)


class FakeVar:
    def __init__(self, value):
        self.value = value

    def set(self, value):
        self.value = value

    def get(self):
        return self.value


class FakeTree:
    def __init__(self, selected=(), row="", column="#2", rows=None):
        self.selected = tuple(str(item) for item in selected)
        self.row = str(row) if row else ""
        self.column = column
        self.rows = tuple(
            str(item) for item in (rows if rows is not None else selected)
        )
        self.values = {}
        self.selection_set_calls = []

    def selection(self):
        return self.selected

    def selection_set(self, item):
        item = str(item)
        self.selected = (item,)
        self.selection_set_calls.append(item)

    def identify_row(self, _y):
        return self.row

    def identify_column(self, _x):
        return self.column

    def get_children(self):
        return self.rows

    def set(self, item, column, value):
        self.values[(str(item), column)] = value

    def exists(self, item):
        return str(item) in self.rows


class FakeText:
    def __init__(self):
        self.content = ""
        self.states = []
        self.seen = []

    def configure(self, **kwargs):
        self.states.append(kwargs)

    def insert(self, _position, text):
        self.content += text

    def see(self, position):
        self.seen.append(position)


class FakeSelectionService:
    def __init__(self, selected=()):
        self._selected = {int(item) for item in selected}

    @property
    def selected_ids(self):
        return set(self._selected)

    def set_selected(self, ids):
        self._selected = {int(item) for item in ids}

    def toggle_selected(self, account_id):
        account_id = int(account_id)
        if account_id in self._selected:
            self._selected.remove(account_id)
        else:
            self._selected.add(account_id)


class FakeMenu:
    def __init__(self):
        self.popup_calls = []
        self.released = False

    def tk_popup(self, x, y):
        self.popup_calls.append((x, y))

    def grab_release(self):
        self.released = True


class AccountManagerAppTests(unittest.TestCase):
    def test_main_table_contains_management_columns(self):
        self.assertEqual(
            (
                "account",
                "password",
                "start_url",
                "login_status",
                "credential_status",
                "json_status",
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
        self.assertEqual(
            (
                "粘贴并识别",
                "指定 URL",
                "一键登录导出 JSON",
                "自动登录设置",
            ),
            AccountManagerApp.PRIMARY_TOOLBAR_LABELS,
        )
        self.assertEqual(
            (
                "全选",
                "反选",
                "取消选择",
                "查看密码",
                "更新密码",
                "导出账号密码",
                "标记已售",
                "恢复管理",
            ),
            AccountManagerApp.SELECTION_TOOLBAR_LABELS,
        )

    def test_password_dialog_clear_removes_both_plaintext_values(self):
        initial = FakeVar("one-time-secret")
        current = FakeVar("current-secret")

        clear_secret_vars(initial, current)

        self.assertEqual("", initial.get())
        self.assertEqual("", current.get())

    def test_password_cell_reflects_saved_password_not_credential_status(self):
        without_password = type(
            "Account", (), {"has_current_password": False}
        )()
        with_password = type(
            "Account", (), {"has_current_password": True}
        )()

        self.assertEqual("未设置", password_cell_text(without_password))
        self.assertEqual("••••••", password_cell_text(with_password))

    def test_json_status_uses_live_override_then_persisted_account_state(self):
        failed = SimpleNamespace(
            credential_status=CredentialStatus.MISSING,
            login_status=LoginStatus.FAILED,
            last_error_code="http_error",
        )
        valid = SimpleNamespace(
            credential_status=CredentialStatus.VALID,
            login_status=LoginStatus.SUCCESS,
            last_error_code=None,
        )

        self.assertEqual("等待中", json_status_text(failed, "等待中"))
        self.assertEqual("失败：http_error", json_status_text(failed))
        self.assertEqual("成功", json_status_text(valid))

    def test_progress_event_updates_json_row_progress_and_redacted_log(self):
        app = object.__new__(AccountManagerApp)
        app.json_status_by_id = {}
        app.login_progress_var = FakeVar(0)
        app.login_progress_text_var = FakeVar("")
        app.tree = FakeTree(rows=(7,))
        app.login_log_text = FakeText()

        app._apply_login_progress(
            LoginProgressEvent(
                account_id=7,
                index=2,
                total=4,
                completed=2,
                status="failed",
                account_masked="ac***",
                code="password=raw-secret",
                stage="automatic_login",
            )
        )

        self.assertEqual("失败：password=<redacted>", app.json_status_by_id[7])
        self.assertEqual(
            "失败：password=<redacted>",
            app.tree.values[("7", "json_status")],
        )
        self.assertEqual(50, app.login_progress_var.get())
        self.assertEqual("JSON 进度：2/4", app.login_progress_text_var.get())
        self.assertIn("ac***", app.login_log_text.content)
        self.assertNotIn("raw-secret", app.login_log_text.content)
        self.assertEqual(["end"], app.login_log_text.seen)

    def test_worker_event_log_formatter_only_uses_safe_whitelisted_fields(self):
        text = format_worker_event(
            WorkerEvent(
                "account_finished",
                {
                    "status": "failed",
                    "code": "http_error",
                    "stage": "password",
                    "message": "unlabelled-password-secret",
                    "refreshToken": "refresh-secret",
                    "adminKey": "admin-secret",
                },
            )
        )

        self.assertIn("http_error", text)
        self.assertIn("password", text)
        self.assertNotIn("unlabelled-password-secret", text)
        self.assertNotIn("refresh-secret", text)
        self.assertNotIn("admin-secret", text)

    def test_password_view_recovers_confirmed_password_when_missing(self):
        missing = SimpleNamespace(current_password=None)
        recovered = SimpleNamespace(current_password="recovered-password")

        class Repository:
            def __init__(self):
                self.calls = 0

            def get(self, _account_id, *, include_secrets):
                self.calls += 1
                return missing if self.calls == 1 else recovered

        repository = Repository()
        coordinator = SimpleNamespace(
            calls=[],
            sync_saved_passwords=lambda ids: coordinator.calls.append(ids),
        )
        app = object.__new__(AccountManagerApp)
        app.service = SimpleNamespace(repository=repository)
        app.coordinator = coordinator

        account = app._load_account_with_password_recovery(7)

        self.assertIs(recovered, account)
        self.assertEqual([[7]], coordinator.calls)

    def test_import_confirmation_always_reparses_current_fields(self):
        source = inspect.getsource(AccountManagerApp.open_import_dialog)
        parse_source = source[source.index("def parse_preview") :]

        self.assertIn("result = parse_preview()", source)
        self.assertNotIn('result = state.get("preview")', source)
        self.assertLess(
            parse_source.index("preview_box.delete"),
            parse_source.index("try:"),
        )
        self.assertIn('summary.set("解析失败")', parse_source)

    def test_tree_selection_syncs_native_highlights_to_service(self):
        app = object.__new__(AccountManagerApp)
        app.tree = FakeTree(selected=(2, 3))
        app.service = FakeSelectionService(selected=(1,))
        app.selected_count_var = FakeVar("")
        app._refreshing_tree = False

        app._tree_selection()

        self.assertEqual({2, 3}, app.service.selected_ids)
        self.assertEqual("已选择 2 个账号", app.selected_count_var.get())

    def test_tree_selection_is_ignored_while_refreshing(self):
        app = object.__new__(AccountManagerApp)
        app.tree = FakeTree(selected=(2, 3))
        app.service = FakeSelectionService(selected=(1,))
        app.selected_count_var = FakeVar("unchanged")
        app._refreshing_tree = True

        app._tree_selection()

        self.assertEqual({1}, app.service.selected_ids)
        self.assertEqual("unchanged", app.selected_count_var.get())

    def test_action_ids_use_all_checked_accounts_not_blue_focus(self):
        app = object.__new__(AccountManagerApp)
        app.tree = FakeTree(selected=(2,))
        app.service = FakeSelectionService(selected=(1, 2))

        self.assertEqual([1, 2], app._selected_action_ids())
        self.assertEqual({1, 2}, app.service.selected_ids)

    def test_right_click_unhighlighted_target_replaces_current_selection(self):
        app = object.__new__(AccountManagerApp)
        app.tree = FakeTree(selected=(1,), row=2)
        app.service = FakeSelectionService(selected=(1, 2))
        app.selected_count_var = FakeVar("")
        app.context_menu = FakeMenu()
        event = type(
            "Event",
            (),
            {"y": 10, "x_root": 100, "y_root": 200},
        )()

        result = app._tree_context_menu(event)

        self.assertEqual("break", result)
        self.assertEqual({2}, app.service.selected_ids)
        self.assertEqual(["2"], app.tree.selection_set_calls)
        self.assertEqual("已选择 1 个账号", app.selected_count_var.get())
        self.assertEqual([(100, 200)], app.context_menu.popup_calls)
        self.assertTrue(app.context_menu.released)

    def test_right_click_highlighted_target_preserves_extended_selection(self):
        app = object.__new__(AccountManagerApp)
        app.tree = FakeTree(selected=(1, 2), row=2)
        app.service = FakeSelectionService(selected=(1,))
        app.selected_count_var = FakeVar("")
        app.context_menu = FakeMenu()
        event = type(
            "Event",
            (),
            {"y": 10, "x_root": 100, "y_root": 200},
        )()

        result = app._tree_context_menu(event)

        self.assertEqual("break", result)
        self.assertEqual({1, 2}, app.service.selected_ids)
        self.assertEqual([], app.tree.selection_set_calls)
        self.assertEqual([(100, 200)], app.context_menu.popup_calls)
        self.assertTrue(app.context_menu.released)

    def test_context_menu_exposes_requested_account_actions(self):
        self.assertEqual(
            (
                "一键获取 JSON",
                "复制账号",
                "复制账号信息",
                "复制 Start URL",
                "查看密码",
                "更新密码",
                "导出账号密码",
                "标记已售",
                "恢复管理",
            ),
            AccountManagerApp.CONTEXT_MENU_LABELS,
        )

    def test_copy_account_info_uses_all_checked_ids(self):
        class Service(FakeSelectionService):
            def __init__(self):
                super().__init__((1, 2))
                self.render_calls = []

            def render_text(self, ids, template):
                self.render_calls.append((list(ids), template))
                return "first----password-one----https://portal/one\nsecond----password-two----https://portal/two"

        app = object.__new__(AccountManagerApp)
        app.tree = FakeTree(selected=(2,))
        app.service = Service()
        app.status_var = FakeVar("")
        copied = []
        app._copy = copied.append

        app.copy_selected_account_info()

        self.assertEqual(
            [([1, 2], AccountManagerApp.DEFAULT_EXPORT_TEMPLATE)],
            app.service.render_calls,
        )
        self.assertEqual(1, len(copied))
        self.assertIn("first----password-one", copied[0])
        self.assertIn("second----password-two", copied[0])
        self.assertEqual("已复制 2 个账号信息", app.status_var.get())

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
