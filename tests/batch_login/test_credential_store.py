import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.credential_models import CredentialRecord
from batch_login.credential_store import CredentialStore, CredentialStoreError


class CredentialModelTests(unittest.TestCase):
    def idc_record(self, *, email="Admin-User"):
        return CredentialRecord(
            email=email,
            auth_method="idc",
            provider="Enterprise",
            refresh_token="refresh-secret",
            access_token="access-secret",
            client_id="client-id",
            client_secret="client-secret",
            start_url="https://example.awsapps.com/start/",
            region="us-east-1",
            expires_at="2026-07-15T01:00:00Z",
        )

    def test_as_add_request_uses_rs_camel_case_and_omits_none(self):
        payload = self.idc_record().as_add_request()

        self.assertEqual("refresh-secret", payload["refreshToken"])
        self.assertEqual("client-secret", payload["clientSecret"])
        self.assertEqual("https://example.awsapps.com/start/", payload["startUrl"])
        self.assertEqual("batch-login-gui", payload["sourceChannel"])
        self.assertNotIn("refresh_token", payload)
        self.assertNotIn("profileArn", payload)

    def test_round_trip_preserves_supported_rs_fields(self):
        original = self.idc_record().as_add_request()

        restored = CredentialRecord.from_add_request(original)

        self.assertEqual(original, restored.as_add_request())

    def test_dedupe_key_casefolds_email_and_normalizes_scope_slash(self):
        first = self.idc_record(email="ADMIN-USER")
        second = self.idc_record(email="admin-user")
        second.start_url = "https://example.awsapps.com/start"

        self.assertEqual(first.dedupe_key(), second.dedupe_key())

    def test_repr_never_contains_tokens_or_client_secret(self):
        representation = repr(self.idc_record())

        self.assertNotIn("refresh-secret", representation)
        self.assertNotIn("access-secret", representation)
        self.assertNotIn("client-secret", representation)

    def test_from_add_request_rejects_non_string_required_identity(self):
        with self.assertRaises(ValueError):
            CredentialRecord.from_add_request(
                {"email": 123, "authMethod": "idc", "provider": "Enterprise"}
            )


class CredentialStoreTests(unittest.TestCase):
    def idc_record(self, *, email="Admin-User"):
        return CredentialRecord(
            email=email,
            auth_method="idc",
            provider="Enterprise",
            refresh_token="refresh-secret",
            access_token="access-secret",
            client_id="client-id",
            client_secret="client-secret",
            start_url="https://example.awsapps.com/start",
            region="us-east-1",
        )

    def test_append_writes_versioned_bundle_and_deduplicates(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "credentials.json"
            store = CredentialStore(path)

            self.assertTrue(store.append(self.idc_record()))
            self.assertFalse(store.append(self.idc_record(email="admin-user")))

            bundle = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(1, bundle["version"])
            self.assertRegex(bundle["generatedAt"], r"Z$")
            self.assertEqual(1, len(bundle["credentials"]))
            self.assertNotIn("password", path.read_text(encoding="utf-8").casefold())

    def test_complete_bundle_keeps_internal_fields_for_recovery(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "credentials.json"
            CredentialStore(path).append(self.idc_record())

            bundle = json.loads(path.read_text(encoding="utf-8"))
            saved = bundle["credentials"][0]

            self.assertIsInstance(bundle, dict)
            self.assertEqual(1, bundle["version"])
            self.assertEqual("access-secret", saved["accessToken"])
            self.assertEqual("batch-login-gui", saved["sourceChannel"])
            self.assertEqual(10, saved["rpmLimit"])

    def test_append_uses_same_directory_temp_and_atomic_replace(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "nested" / "credentials.json"
            store = CredentialStore(path)
            calls = []
            real_replace = os.replace

            def observe_replace(source, target):
                calls.append((Path(source), Path(target)))
                return real_replace(source, target)

            with patch("batch_login.credential_store.os.replace", side_effect=observe_replace):
                store.append(self.idc_record())

            self.assertEqual(1, len(calls))
            source, target = calls[0]
            self.assertEqual(path.parent, source.parent)
            self.assertEqual(path, target)
            self.assertFalse(source.exists())

    def test_permission_failure_warns_but_does_not_lose_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "credentials.json"
            warnings = []
            store = CredentialStore(path, warning_sink=warnings.append)

            with patch("batch_login.credential_store.os.chmod", side_effect=OSError("denied")):
                self.assertTrue(store.append(self.idc_record()))

            self.assertTrue(path.exists())
            self.assertEqual(1, len(warnings))
            self.assertNotIn("refresh-secret", warnings[0])

    def test_replace_failure_removes_temp_and_keeps_existing_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "credentials.json"
            path.write_text('{"version":1,"generatedAt":"old","credentials":[]}', encoding="utf-8")
            store = CredentialStore(path)

            with patch("batch_login.credential_store.os.replace", side_effect=OSError("busy")):
                with self.assertRaises(CredentialStoreError):
                    store.append(self.idc_record())

            self.assertEqual([], list(path.parent.glob(".*.tmp")))
            self.assertEqual([], json.loads(path.read_text(encoding="utf-8"))["credentials"])

    def test_load_rejects_invalid_bundle_without_echoing_contents(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "credentials.json"
            path.write_text('{"version":2,"credentials":"refresh-secret"}', encoding="utf-8")

            with self.assertRaises(CredentialStoreError) as raised:
                CredentialStore(path).load()

            self.assertNotIn("refresh-secret", str(raised.exception))

    def test_directory_creation_failure_is_wrapped_without_path_or_secret(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "private-name" / "credentials.json"
            store = CredentialStore(path)

            with patch.object(Path, "mkdir", side_effect=OSError("private-name denied")):
                with self.assertRaises(CredentialStoreError) as raised:
                    store.append(self.idc_record())

            self.assertNotIn("private-name", str(raised.exception))
            self.assertNotIn("refresh-secret", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
