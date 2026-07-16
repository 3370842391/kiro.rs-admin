import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.credential_models import CredentialRecord
from batch_login.enterprise_http import EnterpriseHttpError
from batch_login.models import AccountEntry
from kiro_enterprise_http_login import (
    build_parser,
    process_entries,
    validate_paths,
)


def credential(account="admin-user"):
    return CredentialRecord(
        email=account,
        auth_method="idc",
        provider="Enterprise",
        refresh_token="refresh",
        access_token="access",
        client_id="client",
        client_secret="secret",
        start_url="https://d-123.awsapps.com/start",
        region="us-east-1",
    )


class FakeAuth:
    def __init__(self, results):
        self.results = list(results)
        self.settings = []

    async def login(self, entry, settings):
        self.settings.append(settings)
        result = self.results.pop(0)
        if isinstance(result, BaseException):
            raise result
        return result


class FakeStore:
    def __init__(self):
        self.records = []

    def append(self, record):
        self.records.append(record)
        return True


class EnterpriseCliTests(unittest.IsolatedAsyncioTestCase):
    def test_parser_allows_per_account_start_urls_without_global_start_url(self):
        args = build_parser().parse_args(
            [
                "--input",
                "accounts.txt",
                "--output",
                "credentials.json",
            ]
        )

        self.assertEqual("", args.start_url)

    def test_parser_defaults_password_vault_next_to_output(self):
        args = build_parser().parse_args(
            [
                "--input",
                "accounts.txt",
                "--start-url",
                "https://d-123.awsapps.com/start",
                "--output",
                "credentials.json",
            ]
        )

        validate_paths(args)

        self.assertEqual(
            Path("credentials.json.passwords.sqlite3"),
            args.password_vault,
        )

    def test_output_and_input_cannot_be_same_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "accounts.txt"
            args = build_parser().parse_args(
                [
                    "--input",
                    str(path),
                    "--start-url",
                    "https://d-123.awsapps.com/start",
                    "--output",
                    str(path),
                ]
            )

            with self.assertRaisesRegex(ValueError, "不能覆盖账号输入文件"):
                validate_paths(args)

    async def test_batch_continues_after_one_enterprise_failure(self):
        error = EnterpriseHttpError(
            "invalid_credentials", "password", False, "账号或密码错误"
        )
        store = FakeStore()
        events = []

        summary = await process_entries(
            [
                AccountEntry(1, "bad-user", "bad-password"),
                AccountEntry(2, "good-user", "good-password"),
            ],
            FakeAuth([error, credential("good-user")]),
            store,
            start_url="https://d-123.awsapps.com/start",
            region="us-east-1",
            emit=events.append,
        )

        self.assertEqual({"total": 2, "succeeded": 1, "failed": 1}, summary)
        self.assertEqual("good-user", store.records[0].email)
        self.assertNotIn("bad-password", str(events))
        self.assertNotIn("good-password", str(events))

    async def test_per_account_start_url_overrides_global_and_falls_back(self):
        auth = FakeAuth([credential("first-user"), credential("second-user")])

        summary = await process_entries(
            [
                AccountEntry(
                    1,
                    "first-user",
                    "first-password",
                    "https://ssoins-first.portal.us-east-1.app.aws/",
                ),
                AccountEntry(2, "second-user", "second-password"),
            ],
            auth,
            FakeStore(),
            start_url="https://d-global.awsapps.com/start",
            region="us-east-1",
        )

        self.assertEqual({"total": 2, "succeeded": 2, "failed": 0}, summary)
        self.assertEqual(
            [
                "https://ssoins-first.portal.us-east-1.app.aws/",
                "https://d-global.awsapps.com/start",
            ],
            [settings.start_url for settings in auth.settings],
        )

    async def test_missing_start_url_fails_only_that_account_and_continues(self):
        auth = FakeAuth([credential("good-user")])
        events = []

        summary = await process_entries(
            [
                AccountEntry(1, "bad-user", "never-log-this-password"),
                AccountEntry(
                    2,
                    "good-user",
                    "also-secret",
                    "https://ssoins-good.portal.us-east-1.app.aws/",
                ),
            ],
            auth,
            FakeStore(),
            start_url="",
            region="us-east-1",
            emit=events.append,
        )

        self.assertEqual({"total": 2, "succeeded": 1, "failed": 1}, summary)
        self.assertEqual(
            ["https://ssoins-good.portal.us-east-1.app.aws/"],
            [settings.start_url for settings in auth.settings],
        )
        self.assertEqual("missing_start_url", events[1]["code"])
        self.assertEqual("configuration", events[1]["stage"])
        self.assertNotIn("never-log-this-password", str(events))
        self.assertNotIn("also-secret", str(events))


if __name__ == "__main__":
    unittest.main()
