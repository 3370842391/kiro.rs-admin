import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.api_key_exporter import ApiKeyExporter
from batch_login.credential_models import CredentialRecord


def cred(email, key=None):
    return CredentialRecord(
        email=email,
        auth_method="idc",
        provider="Enterprise",
        access_token="tok",
        kiro_api_key=key,
    )


class ApiKeyExporterTests(unittest.TestCase):
    def _exporter(self):
        return ApiKeyExporter(
            now=lambda: datetime(2026, 7, 16, 23, 0, 0, tzinfo=timezone.utc)
        )

    def test_exports_login_apikey_lines_and_lists_missing(self):
        with tempfile.TemporaryDirectory() as tmp:
            report = self._exporter().export(
                [cred("codeflow2-7", "ksk_aaa"), cred("codeflow2-8")],
                output_directory=Path(tmp),
            )
            self.assertIsNotNone(report)
            self.assertEqual(1, report.with_key)
            self.assertEqual(1, report.without_key)
            text = report.path.read_text(encoding="utf-8")
            self.assertEqual("ksk_aaa\n", text)  # 纯 key,一行一个,无前缀无注释

    def test_returns_none_when_no_keys(self):
        with tempfile.TemporaryDirectory() as tmp:
            report = self._exporter().export(
                [cred("codeflow2-8")], output_directory=Path(tmp)
            )
            self.assertIsNone(report)
            self.assertEqual([], list(Path(tmp).glob("*.txt")))


if __name__ == "__main__":
    unittest.main()
