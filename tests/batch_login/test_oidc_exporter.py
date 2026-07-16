import json
import os
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.credential_models import CredentialRecord
from batch_login.oidc_exporter import (
    OidcCredentialExporter,
    OidcExportError,
    OidcExportMode,
)


FIXED_TIME = datetime(2026, 7, 16, 12, 30, 45, tzinfo=timezone.utc)


def record(email="admin-user", *, refresh_token="refresh-secret"):
    return CredentialRecord(
        email=email,
        auth_method="IdC",
        provider="Enterprise",
        refresh_token=refresh_token,
        access_token="access-secret",
        profile_arn="arn:aws:codewhisperer:us-east-1:123:profile/test",
        expires_at="2026-07-16T13:30:45Z",
        client_id="client-id",
        client_secret="client-secret",
        start_url="https://example.awsapps.com/start",
        token_endpoint=" https://token.example/ ",
        issuer_url="https://issuer.example/",
        scopes="openid profile",
        region="us-east-1",
        priority=9,
        rpm_limit=99,
        source_channel="private-source",
    )


class OidcCredentialExporterTests(unittest.TestCase):
    def exporter(self, warnings=None):
        return OidcCredentialExporter(
            now=lambda: FIXED_TIME,
            warning_sink=(warnings if warnings is not None else []).append,
        )

    def test_project_uses_explicit_kam_whitelist(self):
        payload = self.exporter().project(record())

        self.assertEqual(
            {
                "email": "admin-user",
                "authMethod": "idc",
                "provider": "Enterprise",
                "region": "us-east-1",
                "startUrl": "https://example.awsapps.com/start",
                "refreshToken": "refresh-secret",
                "clientId": "client-id",
                "clientSecret": "client-secret",
                "profileArn": "arn:aws:codewhisperer:us-east-1:123:profile/test",
                "tokenEndpoint": "https://token.example/",
                "scopes": "openid profile",
                "issuerUrl": "https://issuer.example/",
            },
            payload,
        )
        for forbidden in (
            "accessToken",
            "expiresAt",
            "priority",
            "rpmLimit",
            "sourceChannel",
            "password",
        ):
            self.assertNotIn(forbidden, payload)

    def test_project_omits_blank_optional_values(self):
        item = record()
        item.region = "  "
        item.start_url = None
        item.client_secret = ""

        payload = self.exporter().project(item)

        self.assertNotIn("region", payload)
        self.assertNotIn("startUrl", payload)
        self.assertNotIn("clientSecret", payload)

    def test_merged_mode_writes_kam_array_without_overwriting_same_second(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp)
            exporter = self.exporter()

            first = exporter.export(
                [record(), record("second-user")],
                output_directory=output,
                mode=OidcExportMode.MERGED,
            )
            second = exporter.export(
                [record()],
                output_directory=output,
                mode=OidcExportMode.MERGED,
            )

            self.assertEqual(2, first.record_count)
            self.assertEqual([], list(first.account_paths))
            self.assertEqual(
                "kiro-accounts-20260716-123045.oidc.json",
                first.merged_path.name,
            )
            self.assertEqual(
                "kiro-accounts-20260716-123045-2.oidc.json",
                second.merged_path.name,
            )
            self.assertEqual(
                ["admin-user", "second-user"],
                [
                    item["email"]
                    for item in json.loads(
                        first.merged_path.read_text(encoding="utf-8")
                    )
                ],
            )

    def test_per_account_and_both_write_single_item_arrays_with_safe_names(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp)
            exporter = self.exporter()

            per_account = exporter.export(
                [record("../CON. "), record("same/name")],
                output_directory=output,
                mode=OidcExportMode.PER_ACCOUNT,
            )
            both = exporter.export(
                [record("same\\name")],
                output_directory=output,
                mode=OidcExportMode.BOTH,
            )

            self.assertIsNone(per_account.merged_path)
            self.assertEqual(2, len(per_account.account_paths))
            self.assertIsNotNone(both.merged_path)
            self.assertEqual(1, len(both.account_paths))
            for path in (*per_account.account_paths, *both.account_paths):
                self.assertEqual(output, path.parent)
                self.assertNotIn("..", path.name)
                payload = json.loads(path.read_text(encoding="utf-8"))
                self.assertEqual(1, len(payload))

    def test_missing_refresh_token_rejects_batch_before_writing(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp)
            item = record("private-user", refresh_token="  ")

            with self.assertRaises(OidcExportError) as raised:
                self.exporter().export(
                    [item],
                    output_directory=output,
                    mode=OidcExportMode.BOTH,
                )

            message = str(raised.exception)
            self.assertIn("pr***", message)
            self.assertNotIn("private-user", message)
            self.assertNotIn("access-secret", message)
            self.assertEqual([], list(output.iterdir()))

    def test_replace_failure_cleans_temp_and_preserves_existing_target(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp)
            target = output / "kiro-accounts-20260716-123045.oidc.json"
            target.write_text('[{"old":true}]', encoding="utf-8")
            exporter = self.exporter()

            with patch(
                "batch_login.oidc_exporter.os.replace",
                side_effect=OSError("busy refresh-secret"),
            ):
                with self.assertRaises(OidcExportError) as raised:
                    exporter._atomic_write(target, [exporter.project(record())])

            self.assertEqual('[{"old":true}]', target.read_text(encoding="utf-8"))
            self.assertEqual([], list(output.glob(".*.tmp")))
            self.assertNotIn("refresh-secret", str(raised.exception))

    def test_permission_failure_warns_without_losing_file_or_secret(self):
        with tempfile.TemporaryDirectory() as tmp:
            warnings = []
            exporter = self.exporter(warnings)

            with patch(
                "batch_login.oidc_exporter.os.chmod",
                side_effect=OSError("denied refresh-secret"),
            ):
                report = exporter.export(
                    [record()],
                    output_directory=Path(tmp),
                    mode=OidcExportMode.MERGED,
                )

            self.assertTrue(report.merged_path.exists())
            self.assertEqual(1, len(warnings))
            self.assertNotIn("refresh-secret", warnings[0])


if __name__ == "__main__":
    unittest.main()
