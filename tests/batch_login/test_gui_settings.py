import importlib
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))


def settings_module():
    try:
        return importlib.import_module("batch_login.gui_settings")
    except ModuleNotFoundError as error:
        raise AssertionError("GUI 配置存储模块尚未实现") from error


class GuiSettingsStoreTests(unittest.TestCase):
    def test_round_trip_preserves_plaintext_admin_key_and_all_fields(self):
        module = settings_module()
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "settings.json"
            store = module.GuiSettingsStore(path)
            saved = module.GuiSavedSettings(
                input_template="{account}|{password}|{start_url}",
                output_template="{account}----{password}",
                mode="microsoft",
                start_url="https://d-123.awsapps.com/start",
                password_vault_path="C:/vault.sqlite3",
                region="us-west-2",
                headless=True,
                timeout_seconds=90,
                mfa_timeout_seconds=240,
                result_mode="save_and_import",
                credential_path="C:/credentials.json",
                checkpoint_path="C:/checkpoint.jsonl",
                resume=True,
                rs_url="https://rs.example/admin",
                admin_key="plain-admin-key",
                use_ssh=True,
                ssh_host="ssh.example",
                ssh_user="root",
                ssh_port="2222",
                identity_file="C:/id_ed25519",
                remote_host="127.0.0.1",
                remote_port="8990",
                local_port="4567",
            )

            returned_path = store.save(saved)
            loaded = store.load()
            raw = path.read_text(encoding="utf-8")

            self.assertEqual(path, returned_path)
            self.assertEqual(saved, loaded)
            self.assertIn("plain-admin-key", raw)
            self.assertNotIn("accountText", raw)
            self.assertNotIn("refreshToken", raw)
            self.assertEqual([], list(path.parent.glob("settings.json.tmp-*")))

    def test_invalid_json_version_and_field_types_are_rejected(self):
        module = settings_module()
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "settings.json"
            store = module.GuiSettingsStore(path)
            invalid_values = (
                "{broken",
                json.dumps({"version": 99}),
                json.dumps({"version": 1, "use_ssh": "yes"}),
                json.dumps({"version": 1, "mode": "unknown"}),
                json.dumps({"version": 1, "timeout_seconds": 0}),
            )
            for raw in invalid_values:
                with self.subTest(raw=raw):
                    path.write_text(raw, encoding="utf-8")
                    with self.assertRaises(module.GuiSettingsError):
                        store.load()

    def test_missing_file_and_clear_are_idempotent(self):
        module = settings_module()
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "settings.json"
            store = module.GuiSettingsStore(path)

            self.assertIsNone(store.load())
            self.assertFalse(store.clear())
            store.save(module.GuiSavedSettings())
            self.assertTrue(store.clear())
            self.assertFalse(store.clear())

    def test_default_path_uses_local_app_data(self):
        module = settings_module()
        with patch.dict(os.environ, {"LOCALAPPDATA": "C:/LocalData"}, clear=False):
            self.assertEqual(
                Path("C:/LocalData/KiroBatchLogin/settings.json"),
                module.default_settings_path(),
            )


if __name__ == "__main__":
    unittest.main()
