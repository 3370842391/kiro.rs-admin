import hashlib
import re
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.input_parser import compile_format, parse_accounts, render_accounts
from batch_login.models import AccountEntry, LoginMode, ResultStatus, RunRecord


class ModelTests(unittest.TestCase):
    def test_login_mode_and_result_status_values_match_cli_contract(self):
        self.assertEqual({"enterprise", "microsoft"}, {mode.value for mode in LoginMode})
        self.assertEqual(
            {
                "success",
                "duplicate_credential",
                "failed",
                "manual_required",
                "cancelled",
            },
            {status.value for status in ResultStatus},
        )

    def test_account_hash_is_sha256_of_casefolded_account(self):
        entry = AccountEntry(3, "Straße@example.com", "highly-secret-password")
        expected = hashlib.sha256("strasse@example.com".encode("utf-8")).hexdigest()

        self.assertEqual(expected, entry.account_hash)

    def test_account_entry_repr_never_contains_password(self):
        entry = AccountEntry(3, "user@example.com", "highly-secret-password")

        self.assertNotIn("highly-secret-password", repr(entry))

    def test_run_record_serializes_only_safe_camel_case_fields(self):
        payload = RunRecord(
            run_id="run-1",
            line_number=3,
            account_hash="sha256-hash",
            account_masked="us***@example.com",
            mode=LoginMode.MICROSOFT,
            status=ResultStatus.SUCCESS,
            stage="done",
            attempts=1,
            timestamp="2026-07-15T00:00:00Z",
            credential_id=12,
        ).as_json()

        self.assertEqual("run-1", payload["runId"])
        self.assertEqual(3, payload["lineNumber"])
        self.assertEqual("sha256-hash", payload["accountHash"])
        self.assertEqual("us***@example.com", payload["accountMasked"])
        self.assertEqual("microsoft", payload["mode"])
        self.assertEqual("success", payload["status"])
        self.assertEqual(12, payload["credentialId"])
        self.assertNotIn("line_number", payload)
        self.assertNotIn("account", payload)
        self.assertNotIn("password", payload)
        self.assertNotIn("token", payload)


class InputParserTests(unittest.TestCase):
    def test_full_line_template_parses_literal_prefix_suffix_and_special_password(self):
        password = r"^_S!Ibq1xcU*EwBD$\_AsY8/Oo)"
        result = parse_accounts(
            f"login = admin-user30 / onetime password = {password}\n",
            "login = {account} / onetime password = {password}",
            LoginMode.ENTERPRISE,
        )

        self.assertEqual([], result.issues)
        self.assertEqual("admin-user30", result.entries[0].account)
        self.assertEqual(password, result.entries[0].password)

    def test_default_format_splits_once_and_preserves_password_separator(self):
        result = parse_accounts(
            "user@example.com----abc----123\n",
            "{account}----{password}",
            LoginMode.MICROSOFT,
        )

        self.assertEqual([], result.issues)
        self.assertEqual("user@example.com", result.entries[0].account)
        self.assertEqual("abc----123", result.entries[0].password)

    def test_password_first_uses_last_separator(self):
        result = parse_accounts(
            "abc----123####user@example.com\n",
            "{password}####{account}",
            LoginMode.MICROSOFT,
        )

        self.assertEqual([], result.issues)
        self.assertEqual("abc----123", result.entries[0].password)
        self.assertEqual("user@example.com", result.entries[0].account)

    def test_bom_blank_lines_and_comments_are_ignored_without_losing_line_numbers(self):
        result = parse_accounts(
            "\ufeff  # first comment\n\n   # second comment\nuser@example.com----pw\n",
            "{account}----{password}",
            LoginMode.MICROSOFT,
        )

        self.assertEqual([], result.issues)
        self.assertEqual(4, result.entries[0].line_number)

    def test_account_is_trimmed_while_password_spaces_are_preserved(self):
        result = parse_accounts(
            "  enterprise-user  ----  password with spaces  \n",
            "{account}----{password}",
            LoginMode.ENTERPRISE,
        )

        self.assertEqual([], result.issues)
        self.assertEqual("enterprise-user", result.entries[0].account)
        self.assertEqual("  password with spaces  ", result.entries[0].password)

    def test_microsoft_requires_email_but_enterprise_accepts_plain_username(self):
        microsoft = parse_accounts(
            "plain-user----pw\n",
            "{account}----{password}",
            LoginMode.MICROSOFT,
        )
        enterprise = parse_accounts(
            "plain-user----pw\n",
            "{account}----{password}",
            LoginMode.ENTERPRISE,
        )

        self.assertEqual(["invalid_account"], [issue.code for issue in microsoft.issues])
        self.assertEqual([], enterprise.issues)
        self.assertEqual("plain-user", enterprise.entries[0].account)

    def test_casefold_duplicates_keep_only_first_input(self):
        result = parse_accounts(
            "USER@example.com----first\nuser@example.com----second\n",
            "{account}----{password}",
            LoginMode.MICROSOFT,
        )

        self.assertEqual(["USER@example.com"], [entry.account for entry in result.entries])
        self.assertEqual([2], [issue.line_number for issue in result.issues])
        self.assertEqual(["duplicate_input"], [issue.code for issue in result.issues])

    def test_empty_fields_and_missing_separator_report_line_numbered_issues(self):
        result = parse_accounts(
            "----password\nuser@example.com----\nmissing-separator\n",
            "{account}----{password}",
            LoginMode.MICROSOFT,
        )

        self.assertEqual([], result.entries)
        self.assertEqual(
            [(1, "empty_account"), (2, "empty_password"), (3, "format_mismatch")],
            [(issue.line_number, issue.code) for issue in result.issues],
        )

    def test_compiled_format_stores_a_compiled_pattern(self):
        compiled = compile_format("{account}----{password}")

        self.assertIsInstance(compiled.pattern, re.Pattern)

    def test_template_requires_each_placeholder_exactly_once(self):
        invalid_templates = [
            "{account}",
            "{password}",
            "{account}|{account}|{password}",
            "{account}|{password}|{password}",
        ]

        for template in invalid_templates:
            with self.subTest(template=template):
                with self.assertRaises(ValueError):
                    parse_accounts("a|b", template, LoginMode.ENTERPRISE)

    def test_template_allows_fixed_text_adjacent_placeholders_and_literal_braces(self):
        cases = [
            ("prefix:{account}|{password}:suffix", "prefix:user|pw:suffix", "user", "pw"),
            ("{account}{password}", "", "", ""),
            (
                "login={{account={account}}}; password=<{password}>!",
                "login={{account=enterprise-user}}; password=<pw>!",
                "enterprise-user",
                "pw",
            ),
        ]

        for template, line, expected_account, expected_password in cases:
            with self.subTest(template=template):
                compiled = compile_format(template)
                match = compiled.pattern.fullmatch(line)
                self.assertIsNotNone(match)
                self.assertEqual(expected_account, match.group("account"))
                self.assertEqual(expected_password, match.group("password"))

    def test_render_accounts_preserves_special_password_text_and_joins_with_newlines(self):
        entries = [
            AccountEntry(1, "first@example.com", "/ # < > {account} {password}"),
            AccountEntry(2, "second@example.com", "trailing\\"),
        ]

        rendered = render_accounts(entries, "{account}----{password}")

        self.assertEqual(
            "first@example.com----/ # < > {account} {password}\n"
            "second@example.com----trailing\\",
            rendered,
        )

    def test_render_accounts_validates_required_placeholders(self):
        invalid_templates = [
            "{account}",
            "{password}",
            "{account}|{account}|{password}",
            "{account}|{password}|{password}",
        ]

        for template in invalid_templates:
            with self.subTest(template=template):
                with self.assertRaises(ValueError):
                    render_accounts([], template)


if __name__ == "__main__":
    unittest.main()
