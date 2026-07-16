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

    async def login(self, entry, settings):
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


if __name__ == "__main__":
    unittest.main()
