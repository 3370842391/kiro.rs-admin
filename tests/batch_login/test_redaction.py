import json
import sys
import unittest
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlsplit


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.models import LoginMode, ResultStatus, RunRecord
from batch_login.redaction import mask_account, redact_text, redact_url


class RedactionTests(unittest.TestCase):
    def test_masks_email_and_plain_username(self):
        self.assertEqual("us***@example.com", mask_account("user@example.com"))
        self.assertEqual("ad***", mask_account("admin"))

    def test_redact_url_masks_all_sensitive_query_values_and_removes_fragment(self):
        sensitive = {
            "code": "secret-code",
            "state": "secret-state",
            "access_token": "secret-access",
            "refresh_token": "secret-refresh",
            "id_token": "secret-id",
            "client_secret": "secret-client",
            "code_verifier": "secret-verifier",
            "password": "secret-password",
        }
        query = "&".join(f"{key}={value}" for key, value in sensitive.items())
        redacted = redact_url(f"https://localhost/callback?{query}&safe=value#secret-fragment")
        parts = urlsplit(redacted)
        parsed_query = parse_qs(parts.query)

        self.assertEqual("", parts.fragment)
        self.assertEqual(["value"], parsed_query["safe"])
        for key, secret in sensitive.items():
            with self.subTest(key=key):
                self.assertEqual(["<redacted>"], parsed_query[key])
                self.assertNotIn(secret, redacted)

    def test_redact_url_matches_sensitive_keys_case_insensitively(self):
        redacted = redact_url("https://example.com/callback?Access_Token=secret")

        self.assertNotIn("secret", redacted)

    def test_redact_text_masks_urls_emails_bearer_and_token_password_assignments(self):
        text = (
            "callback=https://localhost/callback?code=url-secret&state=state-secret#fragment "
            "email=user@example.com Authorization: Bearer abc.def.ghi "
            "token=plain-token password=plain-password "
            'access_token="quoted token" client_secret:\'quoted client secret\''
        )
        redacted = redact_text(text)

        for secret in [
            "url-secret",
            "state-secret",
            "fragment",
            "user@example.com",
            "abc.def.ghi",
            "plain-token",
            "plain-password",
            "quoted token",
            "quoted client secret",
        ]:
            with self.subTest(secret=secret):
                self.assertNotIn(secret, redacted)
        self.assertIn("us***@example.com", redacted)
        self.assertIn("Bearer <redacted>", redacted)

    def test_redact_text_masks_email_inside_non_sensitive_url_query(self):
        redacted = redact_text(
            "https://example.com/callback?login_hint=user@example.com"
        )

        decoded = unquote(redacted)
        self.assertNotIn("user@example.com", decoded)
        self.assertIn("us***@example.com", decoded)

    def test_redact_text_masks_percent_encoded_email_inside_url_query(self):
        redacted = redact_text(
            "https://example.com/callback?login_hint=user%40example.com"
        )

        decoded = unquote(redacted)
        self.assertNotIn("user@example.com", decoded)
        self.assertIn("us***@example.com", decoded)

    def test_run_record_json_redacts_sensitive_values_with_quoted_keys(self):
        record = RunRecord(
            run_id="run-quoted-keys",
            line_number=8,
            account_hash="sha256-hash",
            account_masked="us***@example.com",
            mode=LoginMode.MICROSOFT,
            status=ResultStatus.FAILED,
            stage="callback",
            attempts=1,
            timestamp="2026-07-15T00:00:00Z",
            message=(
                '{"password": "plain-secret", '
                '"access_token": "access-secret"}'
            ),
        )

        serialized = json.dumps(record.as_json(), ensure_ascii=False)

        self.assertNotIn("plain-secret", serialized)
        self.assertNotIn("access-secret", serialized)
        self.assertIn("<redacted>", serialized)

    def test_run_record_json_redacts_accidentally_supplied_sensitive_text(self):
        record = RunRecord(
            run_id="run-1",
            line_number=7,
            account_hash="sha256-hash",
            account_masked="user@example.com",
            mode=LoginMode.MICROSOFT,
            status=ResultStatus.FAILED,
            stage="token=stage-secret",
            attempts=1,
            timestamp="2026-07-15T00:00:00Z",
            code="password=code-secret",
            message=(
                "user@example.com Bearer bearer-secret "
                "https://localhost/callback?access_token=url-token"
            ),
        )

        serialized = json.dumps(record.as_json(), ensure_ascii=False)

        self.assertIn("us***@example.com", serialized)
        for secret in [
            "user@example.com",
            "stage-secret",
            "code-secret",
            "bearer-secret",
            "url-token",
        ]:
            with self.subTest(secret=secret):
                self.assertNotIn(secret, serialized)
        self.assertNotIn('"password"', serialized.casefold())
        self.assertNotIn('"token"', serialized.casefold())


if __name__ == "__main__":
    unittest.main()
