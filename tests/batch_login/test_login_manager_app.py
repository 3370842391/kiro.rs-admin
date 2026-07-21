import queue
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import batch_login.login_manager_app as module
from batch_login.login_manager_app import (
    LoginManagerApp,
    format_login_event,
    quota_cell_text,
)
from batch_login.account_login_coordinator import LoginProgressEvent
from batch_login.worker_events import WorkerEvent


class FakeVar:
    def __init__(self, value=""):
        self.value = value

    def set(self, value):
        self.value = value

    def get(self):
        return self.value


class LayoutTests(unittest.TestCase):
    def test_columns_include_quota_and_subscription(self):
        self.assertIn("quota", LoginManagerApp.TABLE_COLUMNS)
        self.assertIn("subscription", LoginManagerApp.TABLE_COLUMNS)
        self.assertIn("api_key_status", LoginManagerApp.TABLE_COLUMNS)

    def test_toolbar_and_menu_expose_login_and_refresh(self):
        self.assertEqual(("登录账号", "刷新额度", "全选", "反选"), LoginManagerApp.TOOLBAR_LABELS)
        self.assertIn("登录账号", LoginManagerApp.CONTEXT_MENU_LABELS)
        self.assertIn("刷新额度", LoginManagerApp.CONTEXT_MENU_LABELS)
        self.assertIn("复制 API Key", LoginManagerApp.CONTEXT_MENU_LABELS)


class QuotaCellTests(unittest.TestCase):
    def test_none_shows_not_queried(self):
        self.assertEqual("未查询", quota_cell_text(None))

    def test_formats_integers_and_decimals(self):
        self.assertEqual("剩余 416 / 总 550", quota_cell_text({"remaining": 416.0, "total": 550}))
        self.assertEqual("剩余 416.50 / 总 550", quota_cell_text({"remaining": 416.5, "total": 550}))


class EventFormatTests(unittest.TestCase):
    def test_quota_updated_and_failed_render(self):
        up = format_login_event(WorkerEvent("quota_updated", {"accountMasked": "ac***", "display": "剩余 5 / 总 10"}))
        self.assertIn("额度已更新", up)
        self.assertIn("剩余 5 / 总 10", up)
        fail = format_login_event(WorkerEvent("quota_failed", {"accountMasked": "ac***", "code": "http_error"}))
        self.assertIn("额度查询失败", fail)

    def test_phase_events_render(self):
        login = format_login_event(WorkerEvent("api_key_phase", {"phase": "login", "count": 3}))
        self.assertIn("第 1 步", login)
        extract = format_login_event(WorkerEvent("api_key_phase", {"phase": "extract", "count": 5}))
        self.assertIn("第 2 步", extract)


class WorkerEventStateTests(unittest.TestCase):
    def test_api_key_phase_switches_progress_prefix(self):
        app = object.__new__(LoginManagerApp)
        app.progress_prefix = "进度"
        app.progress_var = FakeVar(50)
        app.progress_text_var = FakeVar("")
        app.log_text = _FakeText()

        app._apply_worker_event(WorkerEvent("api_key_phase", {"phase": "login", "count": 4}))
        self.assertEqual("登录取 JSON 进度", app.progress_prefix)
        self.assertEqual("登录取 JSON 进度：0/4", app.progress_text_var.get())
        self.assertEqual(0, app.progress_var.get())

    def test_finished_login_summary(self):
        app = object.__new__(LoginManagerApp)
        app.busy = True
        app.status_var = FakeVar("")
        app.log_text = _FakeText()
        app.live_status_by_id = {1: "处理中"}
        app.refresh = lambda: None

        report = SimpleNamespace(created=2, refreshed=1, reused=0, failed=1, skipped=0)
        app._finished(report=("login", report), error=None)

        self.assertFalse(app.busy)
        self.assertIn("创建 2", app.status_var.get())
        self.assertEqual({}, app.live_status_by_id)

    def test_finished_quota_summary(self):
        app = object.__new__(LoginManagerApp)
        app.busy = True
        app.status_var = FakeVar("")
        app.log_text = _FakeText()
        app.live_status_by_id = {}
        app.refresh = lambda: None

        report = SimpleNamespace(updated=3, refreshed=1, failed=0, skipped=1)
        app._finished(report=("quota", report), error=None)

        self.assertIn("更新 3", app.status_var.get())


class CopyApiKeysTests(unittest.TestCase):
    def test_copy_keys_one_per_line(self):
        creds = {
            1: SimpleNamespace(kiro_api_key="ksk_a"),
            2: SimpleNamespace(kiro_api_key=None),
        }

        class Repo:
            def load_credential(self, account_id):
                return creds.get(account_id)

        app = object.__new__(LoginManagerApp)
        app.root = None
        app.service = SimpleNamespace(repository=Repo())
        app.status_var = FakeVar("")
        app._selected_ids = lambda: [1, 2]
        copied = []
        app._copy = copied.append

        app.copy_api_keys()

        self.assertEqual("ksk_a", copied[0])
        self.assertIn("已复制 1", app.status_var.get())


class _FakeText:
    def __init__(self):
        self.content = ""

    def configure(self, **kwargs):
        pass

    def insert(self, _pos, text):
        self.content += text

    def see(self, _pos):
        pass


if __name__ == "__main__":
    unittest.main()
