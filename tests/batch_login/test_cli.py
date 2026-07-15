import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.cli import build_parser, read_input, validate_args


class CliTests(unittest.TestCase):
    def test_enterprise_requires_start_url(self):
        parser = build_parser()
        with self.assertRaises(SystemExit):
            parser.parse_args(
                [
                    "enterprise",
                    "--input",
                    "accounts.txt",
                    "--rs-url",
                    "https://rs",
                ]
            )

    def test_admin_key_must_come_from_environment(self):
        parser = build_parser()
        args = parser.parse_args(
            [
                "microsoft",
                "--input",
                "accounts.txt",
                "--rs-url",
                "https://rs",
            ]
        )
        with self.assertRaises(SystemExit):
            validate_args(args, environ={})

    def test_parser_has_no_password_command_line_option(self):
        parser = build_parser()
        help_text = parser.format_help().casefold()
        help_text += parser._subparsers._group_actions[0].choices[
            "microsoft"
        ].format_help().casefold()
        self.assertNotIn("--password", help_text)

    def test_plain_http_is_only_allowed_for_loopback_ssh_forward(self):
        parser = build_parser()
        remote = parser.parse_args(
            [
                "microsoft",
                "--input",
                "accounts.txt",
                "--rs-url",
                "http://rs.example",
            ]
        )
        with self.assertRaises(SystemExit):
            validate_args(remote, environ={"KIRO_RS_ADMIN_KEY": "key"})

        loopback = parser.parse_args(
            [
                "microsoft",
                "--input",
                "accounts.txt",
                "--rs-url",
                "http://127.0.0.1:18080",
            ]
        )
        self.assertEqual(
            "key",
            validate_args(loopback, environ={"KIRO_RS_ADMIN_KEY": "key"}),
        )

    def test_result_file_cannot_overwrite_input_file(self):
        parser = build_parser()
        with tempfile.TemporaryDirectory() as tmp:
            path = str(Path(tmp) / "accounts.txt")
            args = parser.parse_args(
                [
                    "microsoft",
                    "--input",
                    path,
                    "--result",
                    path,
                    "--rs-url",
                    "https://rs.example",
                ]
            )
            with self.assertRaises(SystemExit):
                validate_args(args, environ={"KIRO_RS_ADMIN_KEY": "key"})

    def test_read_input_accepts_utf8_bom(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "accounts.txt"
            path.write_text("\ufeffuser----secret\n", encoding="utf-8")
            self.assertEqual("user----secret\n", read_input(str(path)))


if __name__ == "__main__":
    unittest.main()
