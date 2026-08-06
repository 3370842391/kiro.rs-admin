import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.api_key_exporter import ApiKeyExporter
from batch_login.credential_models import CredentialRecord


def cred(email, key=None, region=None):
    return CredentialRecord(
        email=email,
        auth_method="idc",
        provider="Enterprise",
        access_token="tok",
        kiro_api_key=key,
        region=region,
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


class ApiKeyExporterRegionTests(unittest.TestCase):
    """按区域分文件：美区与欧区的 key 混在一个清单里，下游按区导入时要人工挑。"""

    def _exporter(self):
        return ApiKeyExporter(
            now=lambda: datetime(2026, 8, 6, 10, 15, 30, tzinfo=timezone.utc)
        )

    def test_splits_keys_into_one_file_per_region(self):
        with tempfile.TemporaryDirectory() as tmp:
            report = self._exporter().export(
                [
                    cred("a", "ksk_us1", region="us-east-1"),
                    cred("b", "ksk_eu1", region="eu-central-1"),
                    cred("c", "ksk_us2", region="us-east-1"),
                ],
                output_directory=Path(tmp),
            )
            self.assertIsNotNone(report)
            self.assertEqual(2, report.region_count)
            self.assertEqual({"us-east-1": 2, "eu-central-1": 1}, report.counts_by_region)

            us = report.paths_by_region["us-east-1"]
            eu = report.paths_by_region["eu-central-1"]
            self.assertIn("us-east-1", us.name)
            self.assertIn("eu-central-1", eu.name)
            # 各区只含本区的 key，不串
            self.assertEqual("ksk_us1\nksk_us2\n", us.read_text(encoding="utf-8"))
            self.assertEqual("ksk_eu1\n", eu.read_text(encoding="utf-8"))

    def test_blank_and_mixed_case_regions_normalize_to_one_bucket(self):
        with tempfile.TemporaryDirectory() as tmp:
            report = self._exporter().export(
                [
                    cred("a", "ksk_1", region=None),
                    cred("b", "ksk_2", region="  US-East-1 "),
                    cred("c", "ksk_3", region="us-east-1"),
                ],
                output_directory=Path(tmp),
            )
            # 空值归默认区，大小写/空白不分桶 → 三个 key 一个文件
            self.assertEqual(1, report.region_count)
            self.assertEqual({"us-east-1": 3}, report.counts_by_region)

    def test_path_stays_populated_for_existing_callers(self):
        with tempfile.TemporaryDirectory() as tmp:
            report = self._exporter().export(
                [cred("a", "ksk_eu", region="eu-central-1")],
                output_directory=Path(tmp),
            )
            # 既有调用方读 report.path 发事件，单区时它必须仍然指向那个文件
            self.assertEqual(report.path, report.paths_by_region["eu-central-1"])
            self.assertTrue(report.path.exists())
