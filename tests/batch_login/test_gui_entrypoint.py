import contextlib
import importlib
import io
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))


class GuiEntrypointTests(unittest.TestCase):
    def test_check_mode_does_not_create_tk_window(self):
        module = importlib.import_module("kiro_batch_login_gui")

        result = module.main(["--check"], checker=lambda: [])

        self.assertEqual(0, result)

    def test_check_mode_reports_missing_dependencies(self):
        module = importlib.import_module("kiro_batch_login_gui")
        stderr = io.StringIO()

        with contextlib.redirect_stderr(stderr):
            result = module.main(
                ["--check"],
                checker=lambda: ["当前 Python 未安装 Tkinter"],
            )

        self.assertEqual(1, result)
        self.assertIn("Tkinter", stderr.getvalue())

    def test_check_mode_treats_missing_ssh_as_non_fatal(self):
        module = importlib.import_module("kiro_batch_login_gui")
        stderr = io.StringIO()

        with contextlib.redirect_stderr(stderr):
            result = module.main(
                ["--check"],
                checker=lambda: ["未找到系统 OpenSSH；SSH 模式不可用"],
            )

        self.assertEqual(0, result)
        self.assertIn("OpenSSH", stderr.getvalue())

    def test_gui_app_import_has_no_window_side_effect(self):
        module = importlib.import_module("batch_login.gui_app")

        self.assertTrue(hasattr(module, "BatchLoginApp"))


if __name__ == "__main__":
    unittest.main()
