import asyncio
import sys
import threading
import unittest
from unittest.mock import AsyncMock, patch
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.gui_controller import GuiController, GuiFormState
from batch_login.gui_runtime import GuiRuntime
from batch_login.models import AccountEntry, LoginMode
from batch_login.worker_events import ResultMode


def valid_form(**overrides):
    values = {
        "mode": LoginMode.ENTERPRISE,
        "start_url": "https://example.awsapps.com/start",
        "credential_path": "credentials.json",
        "checkpoint_path": "checkpoint.jsonl",
        "result_mode": ResultMode.SAVE_ONLY,
    }
    values.update(overrides)
    return GuiFormState(**values)


def valid_entries():
    return [AccountEntry(1, "admin-user", "one-time-password")]


class FakeRuntime:
    def __init__(self, *, block=False, error=None):
        self.block = block
        self.error = error
        self.started = threading.Event()
        self.cancelled = threading.Event()
        self.closed = threading.Event()
        self.imported = False

    async def run(self, _entries):
        self.started.set()
        if self.error is not None:
            raise self.error
        if self.block:
            try:
                await asyncio.Future()
            except asyncio.CancelledError:
                self.cancelled.set()
                raise

    async def import_existing(self):
        self.started.set()
        self.imported = True

    async def close(self):
        self.closed.set()


class GuiControllerTests(unittest.TestCase):
    def tearDown(self):
        controller = getattr(self, "controller", None)
        if controller is not None:
            controller.cancel()
            if controller.thread is not None:
                controller.thread.join(timeout=2)

    def test_save_only_does_not_require_rs_or_ssh_fields(self):
        form = valid_form(rs_url="", admin_key="", use_ssh=False)

        self.assertEqual([], form.validate())

    def test_enterprise_requires_start_url(self):
        form = valid_form(start_url="")

        self.assertIn("企业模式必须填写 Start URL", form.validate())

    def test_enterprise_password_vault_defaults_next_to_credential_json(self):
        form = valid_form(password_vault_path="")

        self.assertEqual(
            Path("credentials.json.passwords.sqlite3"),
            form.to_run_settings().password_vault_path,
        )

    def test_direct_remote_plain_http_is_rejected(self):
        form = valid_form(
            result_mode=ResultMode.SAVE_AND_IMPORT,
            rs_url="http://rs.example:8990",
            admin_key="admin-key",
        )

        self.assertIn("远程 RS 必须使用 HTTPS", form.validate())

    def test_start_and_cancel_are_marshaled_to_worker_loop(self):
        runtime = FakeRuntime(block=True)
        self.controller = GuiController(
            runtime_factory=lambda _form, _emit: runtime
        )

        self.controller.start(valid_entries(), valid_form())
        self.assertTrue(runtime.started.wait(timeout=1))
        self.controller.cancel()
        self.assertTrue(runtime.cancelled.wait(timeout=1))
        self.controller.thread.join(timeout=1)

        self.assertTrue(runtime.closed.is_set())

    def test_repeated_start_is_rejected(self):
        runtime = FakeRuntime(block=True)
        self.controller = GuiController(
            runtime_factory=lambda _form, _emit: runtime
        )
        self.controller.start(valid_entries(), valid_form())
        self.assertTrue(runtime.started.wait(timeout=1))

        with self.assertRaisesRegex(RuntimeError, "已有任务"):
            self.controller.start(valid_entries(), valid_form())

    def test_runtime_error_is_redacted_and_runtime_is_closed(self):
        runtime = FakeRuntime(error=RuntimeError("refresh_token=secret-value"))
        self.controller = GuiController(
            runtime_factory=lambda _form, _emit: runtime
        )

        self.controller.start(valid_entries(), valid_form())
        self.controller.thread.join(timeout=1)
        events = self.controller.drain_events()

        self.assertTrue(runtime.closed.is_set())
        fatal = next(event for event in events if event.kind == "fatal_error")
        self.assertNotIn("secret-value", fatal.payload["message"])

    def test_import_existing_uses_same_worker_boundary(self):
        runtime = FakeRuntime()
        self.controller = GuiController(
            runtime_factory=lambda _form, _emit: runtime
        )
        form = valid_form(
            result_mode=ResultMode.SAVE_AND_IMPORT,
            rs_url="https://rs.example",
            admin_key="admin-key",
        )

        self.controller.import_existing(form)
        self.controller.thread.join(timeout=1)

        self.assertTrue(runtime.imported)
        self.assertTrue(runtime.closed.is_set())

    def test_input_and_credential_paths_must_differ(self):
        path = str(Path("same-file.txt").resolve())

        errors = valid_form(
            input_path=path,
            credential_path=path,
        ).validate()

        self.assertIn("完整凭据 JSON 不能覆盖账号输入文件", errors)

    def test_password_vault_cannot_overwrite_credential_json(self):
        form = valid_form(password_vault_path="credentials.json")

        self.assertIn("密码保险库不能覆盖完整凭据 JSON", form.validate())


class FakeResource:
    def __init__(self, name, calls, *, error=None):
        self.name = name
        self.calls = calls
        self.error = error

    async def _finish(self):
        self.calls.append(self.name)
        if self.error is not None:
            raise self.error

    close = _finish
    stop = _finish
    aclose = _finish


class GuiRuntimeTests(unittest.IsolatedAsyncioTestCase):
    async def test_enterprise_runtime_does_not_start_playwright(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            form = valid_form(
                credential_path=str(Path(tmp) / "credentials.json"),
                checkpoint_path=str(Path(tmp) / "checkpoint.jsonl"),
                password_vault_path=str(Path(tmp) / "passwords.sqlite3"),
            )
            runtime = GuiRuntime(form, lambda _event: None)
            with patch(
                "batch_login.gui_runtime.async_playwright",
                side_effect=AssertionError("enterprise must not start Playwright"),
            ), patch(
                "batch_login.gui_runtime.LocalBatchRunner.run",
                new=AsyncMock(return_value=None),
            ):
                await runtime.run([])

            self.assertIsNone(runtime.playwright)
            self.assertIsNone(runtime.browser)
            self.assertIsNotNone(runtime.enterprise_transport)
            await runtime.close()

    async def test_runner_importer_defers_rs_connection_until_import_stage(self):
        form = valid_form(
            result_mode=ResultMode.SAVE_AND_IMPORT,
            rs_url="https://rs.example",
            admin_key="admin-key",
        )
        runtime = GuiRuntime(form, lambda _event: None)
        calls = []

        class FakeImporter:
            async def batch_import(self, credentials, _on_event):
                calls.append(("batch_import", credentials))
                return {"imported": 1, "verified": 0}

        async def connect():
            calls.append(("connect", None))
            return FakeImporter()

        runtime._connect_importer = connect

        importer = runtime.runner_importer()
        self.assertEqual([], calls)
        await importer.batch_import([{"email": "user@example.com"}], lambda _event: None)

        self.assertEqual(["connect", "batch_import"], [item[0] for item in calls])

    async def test_close_releases_every_resource_in_order_after_failure(self):
        calls = []
        runtime = GuiRuntime(valid_form(), lambda _event: None)
        runtime.browser = FakeResource(
            "browser",
            calls,
            error=RuntimeError("browser close failed"),
        )
        runtime.playwright = FakeResource("playwright", calls)
        runtime.http = FakeResource("http", calls)
        runtime.importer = FakeResource("importer", calls)
        runtime.tunnel = FakeResource("tunnel", calls)
        runtime.enterprise_transport = FakeResource("enterprise_transport", calls)

        with self.assertRaisesRegex(RuntimeError, "browser close failed"):
            await runtime.close()

        self.assertEqual(
            [
                "browser",
                "playwright",
                "http",
                "enterprise_transport",
                "importer",
                "tunnel",
            ],
            calls,
        )


if __name__ == "__main__":
    unittest.main()
