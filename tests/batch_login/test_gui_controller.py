import asyncio
import sys
import threading
import unittest
from unittest.mock import AsyncMock, patch
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.gui_controller import GuiController, GuiFormState
from batch_login.gui_app import BatchLoginApp
from batch_login.gui_runtime import GuiRuntime
from batch_login.gui_settings import GuiSavedSettings, GuiSettingsError
from batch_login.models import AccountEntry, LoginMode, ParseResult
from batch_login.oidc_exporter import OidcExportMode
from batch_login.credential_models import CredentialRecord
from batch_login.credential_store import CredentialStore
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
        self.exported = False

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

    async def export_existing(self):
        self.started.set()
        self.exported = True

    async def close(self):
        self.closed.set()


class FakeVar:
    def __init__(self, value=None):
        self.value = value

    def get(self):
        return self.value

    def set(self, value):
        self.value = value


class FakeText:
    def __init__(self, value):
        self.value = value

    def get(self, _start, _end):
        return self.value


class FakePreview:
    def __init__(self):
        self.rows = []

    def get_children(self):
        return ()

    def delete(self, *_items):
        self.rows.clear()

    def insert(self, _parent, _index, *, values):
        self.rows.append(values)


class FakeSettingsStore:
    def __init__(self, loaded=None, error=None):
        self.loaded = loaded
        self.error = error
        self.saved = None
        self.cleared = False
        self.path = Path("C:/LocalData/KiroBatchLogin/settings.json")

    def load(self):
        if self.error is not None:
            raise self.error
        return self.loaded

    def save(self, settings):
        self.saved = settings
        return self.path

    def clear(self):
        self.cleared = True
        return True


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

    def test_enterprise_validation_can_defer_start_url_to_entries(self):
        form = valid_form(start_url="")

        self.assertNotIn(
            "企业模式必须填写 Start URL",
            form.validate(require_start_url=False),
        )

    def test_start_allows_missing_global_url_when_every_entry_has_one(self):
        runtime = FakeRuntime()
        self.controller = GuiController(
            runtime_factory=lambda _form, _emit: runtime
        )
        entries = [
            AccountEntry(
                1,
                "admin-user",
                "one-time-password",
                "https://ssoins-example.portal.us-east-1.app.aws/",
            )
        ]

        self.controller.start(entries, valid_form(start_url=""))
        self.controller.thread.join(timeout=1)

        self.assertTrue(runtime.started.is_set())

    def test_start_requires_global_url_when_any_entry_has_no_url(self):
        self.controller = GuiController(
            runtime_factory=lambda _form, _emit: FakeRuntime()
        )
        entries = [
            AccountEntry(
                1,
                "admin-user",
                "one-time-password",
                "https://ssoins-example.portal.us-east-1.app.aws/",
            ),
            AccountEntry(2, "admin-user-2", "one-time-password-2"),
        ]

        with self.assertRaisesRegex(ValueError, "Start URL"):
            self.controller.start(entries, valid_form(start_url=""))

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

    def test_import_existing_does_not_require_enterprise_start_url(self):
        runtime = FakeRuntime()
        self.controller = GuiController(
            runtime_factory=lambda _form, _emit: runtime
        )
        form = valid_form(
            start_url="",
            result_mode=ResultMode.SAVE_AND_IMPORT,
            rs_url="https://rs.example",
            admin_key="admin-key",
        )

        self.controller.import_existing(form)
        self.controller.thread.join(timeout=1)

        self.assertTrue(runtime.imported)
        self.assertTrue(runtime.closed.is_set())

    def test_export_existing_needs_neither_enterprise_portal_nor_rs(self):
        runtime = FakeRuntime()
        self.controller = GuiController(
            runtime_factory=lambda _form, _emit: runtime
        )
        form = valid_form(
            start_url="",
            region="",
            input_template="{broken",
            output_template="{also-broken",
            result_mode=ResultMode.SAVE_AND_IMPORT,
            rs_url="",
            admin_key="",
            oidc_export_mode=OidcExportMode.BOTH,
            oidc_export_directory="C:/exports",
        )

        self.controller.export_existing(form)
        self.controller.thread.join(timeout=1)

        self.assertTrue(runtime.exported)
        self.assertFalse(runtime.imported)
        self.assertTrue(runtime.closed.is_set())

    def test_oidc_output_directory_defaults_next_to_complete_json(self):
        form = valid_form(
            credential_path="nested/credentials.json",
            oidc_export_directory="",
        )

        self.assertEqual(
            Path("nested/credentials.json").resolve().parent,
            form.oidc_output_dir(),
        )

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


class BatchLoginAppTests(unittest.TestCase):
    def require_method(self, app, name):
        method = getattr(app, name, None)
        self.assertIsNotNone(method, f"GUI 缺少配置方法 {name}")
        return method

    def make_app(self, text, *, start_url=""):
        app = BatchLoginApp.__new__(BatchLoginApp)
        app.input_text = FakeText(text)
        app.input_template_var = FakeVar("{account}|{password}|{start_url}")
        app.mode_var = FakeVar(LoginMode.ENTERPRISE.value)
        app.start_url_var = FakeVar(start_url)
        app.status_var = FakeVar("")
        app.show_password_var = FakeVar(False)
        app.preview = FakePreview()
        app.last_result = ParseResult([], [])
        app.entries = []
        return app

    def make_settings_app(self, *, admin_key="env-admin-key"):
        app = BatchLoginApp.__new__(BatchLoginApp)
        values = {
            "input_template_var": "login = {account} / onetime password = {password}",
            "output_template_var": "{account}----{password}",
            "mode_var": LoginMode.ENTERPRISE.value,
            "start_url_var": "",
            "password_vault_path_var": "",
            "region_var": "us-east-1",
            "headless_var": False,
            "timeout_var": 180.0,
            "mfa_timeout_var": 300.0,
            "result_mode_var": ResultMode.SAVE_ONLY.value,
            "credential_path_var": "",
            "checkpoint_path_var": "",
            "resume_var": False,
            "rs_url_var": "",
            "admin_key_var": admin_key,
            "use_ssh_var": False,
            "ssh_host_var": "",
            "ssh_user_var": "",
            "ssh_port_var": "22",
            "identity_file_var": "",
            "remote_host_var": "127.0.0.1",
            "remote_port_var": "8990",
            "local_port_var": "",
            "oidc_export_mode_var": "合并 JSON",
            "oidc_export_directory_var": "",
            "status_var": "准备就绪",
        }
        for name, value in values.items():
            setattr(app, name, FakeVar(value))
        app.settings_store = FakeSettingsStore()
        app.settings_warning = ""
        app.root = object()
        app.logs = []
        app._append_log = app.logs.append
        return app

    def test_saved_settings_are_applied_to_gui_variables(self):
        app = self.make_settings_app()
        settings = GuiSavedSettings(
            input_template="{account}|{password}|{start_url}",
            mode="microsoft",
            start_url="https://d-123.awsapps.com/start",
            region="us-west-2",
            result_mode="save_and_import",
            rs_url="https://rs.example/admin",
            admin_key="plain-admin-key",
            use_ssh=True,
            ssh_host="ssh.example",
            ssh_port="2222",
            oidc_export_mode="both",
            oidc_export_directory="C:/oidc-exports",
        )

        self.require_method(app, "_apply_saved_settings")(settings)

        self.assertEqual("plain-admin-key", app.admin_key_var.get())
        self.assertEqual("https://rs.example/admin", app.rs_url_var.get())
        self.assertEqual("microsoft", app.mode_var.get())
        self.assertEqual("save_and_import", app.result_mode_var.get())
        self.assertTrue(app.use_ssh_var.get())
        self.assertEqual("2222", app.ssh_port_var.get())
        self.assertEqual("两种同时", app.oidc_export_mode_var.get())
        self.assertEqual(
            "C:/oidc-exports", app.oidc_export_directory_var.get()
        )

    def test_empty_saved_admin_key_preserves_environment_default(self):
        app = self.make_settings_app(admin_key="env-admin-key")

        self.require_method(app, "_apply_saved_settings")(
            GuiSavedSettings(admin_key="")
        )

        self.assertEqual("env-admin-key", app.admin_key_var.get())

    def test_save_configuration_uses_plaintext_key_without_account_text(self):
        app = self.make_settings_app(admin_key="plain-admin-key")
        app.rs_url_var.set("https://rs.example/admin")
        app.start_url_var.set("https://d-123.awsapps.com/start")

        self.require_method(app, "_save_configuration")()

        saved = app.settings_store.saved
        self.assertEqual("plain-admin-key", saved.admin_key)
        self.assertEqual("https://rs.example/admin", saved.rs_url)
        self.assertNotIn("account", saved.as_json())
        self.assertIn("明文 Admin Key", app.status_var.get())
        self.assertEqual(app.status_var.get(), app.logs[-1])

    def test_saved_configuration_contains_oidc_mode_and_directory(self):
        app = self.make_settings_app()
        app.oidc_export_mode_var.set("逐账号 JSON")
        app.oidc_export_directory_var.set("C:/oidc-exports")

        saved = self.require_method(app, "_snapshot_settings")()

        self.assertEqual("per_account", saved.oidc_export_mode)
        self.assertEqual("C:/oidc-exports", saved.oidc_export_directory)

    def test_oidc_directory_chooser_uses_directory_dialog(self):
        app = self.make_settings_app()

        with patch(
            "batch_login.gui_app.filedialog.askdirectory",
            return_value="C:/oidc-exports",
        ):
            self.require_method(app, "_choose_oidc_export_directory")()

        self.assertEqual(
            "C:/oidc-exports", app.oidc_export_directory_var.get()
        )

    def test_clear_configuration_keeps_current_form_values(self):
        app = self.make_settings_app(admin_key="keep-current-key")

        with patch("batch_login.gui_app.messagebox.askyesno", return_value=True):
            self.require_method(app, "_clear_configuration")()

        self.assertTrue(app.settings_store.cleared)
        self.assertEqual("keep-current-key", app.admin_key_var.get())
        self.assertIn("下次启动使用默认值", app.status_var.get())

    def test_load_error_becomes_warning_instead_of_crashing(self):
        app = self.make_settings_app()
        app.settings_store = FakeSettingsStore(
            error=GuiSettingsError("无法读取 GUI 配置")
        )

        loaded = self.require_method(app, "_load_saved_settings")()

        self.assertIsNone(loaded)
        self.assertIn("无法读取 GUI 配置", app.settings_warning)

    def test_three_field_format_is_available_as_a_preset(self):
        self.assertIn(
            "{account}|{password}|{start_url}",
            BatchLoginApp.INPUT_FORMAT_PRESETS,
        )

    def test_preview_masks_password_and_displays_enterprise_portal(self):
        app = self.make_app("")
        password = "secret-password"
        portal = "https://ssoins-example.portal.us-east-1.app.aws/"

        app._render_preview(
            ParseResult(
                [AccountEntry(1, "admin-user", password, portal)],
                [],
            )
        )

        values = app.preview.rows[0]
        self.assertNotIn(password, values)
        self.assertEqual(portal, values[3])

    def test_convert_preview_fills_unique_per_entry_portal(self):
        portal = "https://ssoins-example.portal.us-east-1.app.aws/"
        app = self.make_app(f"admin-user|secret-password|{portal}")

        app._convert_preview()

        self.assertEqual(portal, app.start_url_var.get())
        self.assertNotIn("secret-password", app.status_var.get())

    def test_convert_preview_keeps_global_url_for_multiple_portals(self):
        first = "https://ssoins-first.portal.us-east-1.app.aws/"
        second = "https://ssoins-second.portal.us-east-1.app.aws/"
        app = self.make_app(
            "\n".join(
                [
                    f"admin-user|secret-one|{first}",
                    f"admin-user-2|secret-two|{second}",
                ]
            ),
            start_url="https://global.example/start",
        )

        app._convert_preview()

        self.assertEqual(
            "https://global.example/start",
            app.start_url_var.get(),
        )
        self.assertIn("按每行企业门户登录", app.status_var.get())
        self.assertNotIn("secret-one", app.status_var.get())
        self.assertNotIn("secret-two", app.status_var.get())


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
    async def test_export_existing_converts_complete_bundle_without_rs(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            credential_path = root / "credentials.json"
            output = root / "exports"
            CredentialStore(credential_path).append(
                CredentialRecord(
                    email="admin-user",
                    auth_method="idc",
                    provider="Enterprise",
                    refresh_token="refresh-secret",
                )
            )
            form = valid_form(
                start_url="",
                result_mode=ResultMode.SAVE_AND_IMPORT,
                rs_url="",
                admin_key="",
                credential_path=str(credential_path),
                oidc_export_mode=OidcExportMode.BOTH,
                oidc_export_directory=str(output),
            )
            events = []
            runtime = GuiRuntime(form, events.append)

            with patch.object(
                runtime,
                "_connect_importer",
                side_effect=AssertionError("manual export must not connect RS"),
            ):
                report = await runtime.export_existing()

            self.assertEqual(1, report.record_count)
            self.assertEqual(2, len(list(output.glob("*.json"))))
            exported_event = next(
                event for event in events if event.kind == "oidc_exported"
            )
            self.assertEqual(1, exported_event.payload["count"])
            self.assertEqual(2, exported_event.payload["fileCount"])
            self.assertNotIn(
                "refresh-secret", str(exported_event.payload)
            )

    async def test_enterprise_runtime_does_not_start_playwright(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            form = valid_form(
                credential_path=str(Path(tmp) / "credentials.json"),
                checkpoint_path=str(Path(tmp) / "checkpoint.jsonl"),
                password_vault_path=str(Path(tmp) / "passwords.sqlite3"),
            )
            runtime = GuiRuntime(form, lambda _event: None)
            captured = {}

            async def run_without_accounts(runner, _entries, _settings):
                captured["enterprise"] = runner.enterprise
                return None

            with patch(
                "batch_login.gui_runtime.async_playwright",
                side_effect=AssertionError("enterprise must not start Playwright"),
            ), patch(
                "batch_login.gui_runtime.LocalBatchRunner.run",
                new=run_without_accounts,
            ):
                await runtime.run([])

            self.assertIsNone(runtime.playwright)
            self.assertIsNone(runtime.browser)
            self.assertIsNone(runtime.enterprise_transport)
            self.assertEqual(
                "IsolatedEnterpriseAuth",
                type(captured["enterprise"]).__name__,
            )
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
