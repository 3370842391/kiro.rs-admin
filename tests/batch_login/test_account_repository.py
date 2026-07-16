import sqlite3
import sys
import tempfile
import unittest
from contextlib import closing
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.account_repository import (
    AccountRepository,
    AccountRepositoryError,
    CredentialStatus,
    LifecycleStatus,
    LoginStatus,
)
from batch_login.models import AccountEntry, LoginMode
from batch_login.credential_models import CredentialRecord


class FakeProtector:
    def protect(self, value: bytes) -> bytes:
        return b"protected:" + value[::-1]

    def unprotect(self, value: bytes) -> bytes:
        if not value.startswith(b"protected:"):
            raise ValueError("cannot decrypt secret-value")
        return value.removeprefix(b"protected:")[::-1]


class BrokenProtector(FakeProtector):
    def unprotect(self, value: bytes) -> bytes:
        raise ValueError("cannot decrypt private-password")


def entry(account="admin-user", password="one-time", start_url=None):
    return AccountEntry(1, account, password, start_url)


class AccountRepositoryTests(unittest.TestCase):
    def make_repo(self, root, protector=None):
        return AccountRepository(
            Path(root) / "accounts.sqlite3",
            protector=protector or FakeProtector(),
        )

    def test_upsert_encrypts_password_and_list_hides_secrets(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.make_repo(tmp)

            saved = repo.upsert_entries(
                [entry(password="private-one-time")],
                login_mode=LoginMode.ENTERPRISE,
                region="us-east-1",
            )

            self.assertEqual(1, len(saved))
            listed = repo.list_accounts()
            self.assertEqual("admin-user", listed[0].account)
            self.assertIsNone(listed[0].initial_password)
            self.assertIsNone(listed[0].current_password)
            raw = repo.path.read_bytes()
            self.assertNotIn(b"private-one-time", raw)
            revealed = repo.get(saved[0].id, include_secrets=True)
            self.assertEqual("private-one-time", revealed.initial_password)
            self.assertIsNone(revealed.current_password)

    def test_duplicate_upsert_preserves_sold_note_and_current_password(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.make_repo(tmp)
            original = repo.upsert_entries(
                [entry(start_url="https://portal.example/start/")],
                login_mode=LoginMode.ENTERPRISE,
                region="us-east-1",
            )[0]
            repo.update_current_passwords([original.id], "current-password")
            repo.mark_sold([original.id], "客户 A")

            updated = repo.upsert_entries(
                [
                    entry(
                        account="ADMIN-USER",
                        password="new-one-time",
                        start_url="https://portal.example/start",
                    )
                ],
                login_mode=LoginMode.ENTERPRISE,
                region="us-west-2",
            )[0]

            self.assertEqual(original.id, updated.id)
            revealed = repo.get(original.id, include_secrets=True)
            self.assertEqual("new-one-time", revealed.initial_password)
            self.assertEqual("current-password", revealed.current_password)
            self.assertEqual("客户 A", revealed.note)
            self.assertIs(LifecycleStatus.SOLD, revealed.lifecycle_status)
            self.assertEqual("us-west-2", revealed.region)

    def test_update_password_marks_credentials_stale(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.make_repo(tmp)
            item = repo.upsert_entries(
                [entry()],
                login_mode=LoginMode.ENTERPRISE,
                region="us-east-1",
            )[0]

            count = repo.update_current_passwords([item.id], "new-password")
            updated = repo.get(item.id, include_secrets=True)

            self.assertEqual(1, count)
            self.assertEqual("new-password", updated.current_password)
            self.assertIs(CredentialStatus.STALE, updated.credential_status)

    def test_batch_status_update_rolls_back_when_any_id_is_missing(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.make_repo(tmp)
            item = repo.upsert_entries(
                [entry()],
                login_mode=LoginMode.ENTERPRISE,
                region="us-east-1",
            )[0]

            with self.assertRaises(AccountRepositoryError):
                repo.mark_sold([item.id, 999999], "客户 A")

            unchanged = repo.get(item.id)
            self.assertIs(LifecycleStatus.MANAGED, unchanged.lifecycle_status)
            self.assertEqual("", unchanged.note)

    def test_restore_managed_keeps_note(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.make_repo(tmp)
            item = repo.upsert_entries(
                [entry()],
                login_mode=LoginMode.ENTERPRISE,
                region="us-east-1",
            )[0]
            repo.mark_sold([item.id], "客户 A")

            repo.restore_managed([item.id])

            restored = repo.get(item.id)
            self.assertIs(LifecycleStatus.MANAGED, restored.lifecycle_status)
            self.assertEqual("客户 A", restored.note)

    def test_unknown_schema_version_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "accounts.sqlite3"
            with closing(sqlite3.connect(path)) as connection:
                connection.execute(
                    "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
                )
                connection.execute(
                    "INSERT INTO metadata(key, value) VALUES ('schema_version', '99')"
                )
                connection.commit()

            with self.assertRaisesRegex(AccountRepositoryError, "版本"):
                AccountRepository(path, protector=FakeProtector())

    def test_decryption_error_does_not_echo_secret_or_ciphertext(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.make_repo(tmp)
            item = repo.upsert_entries(
                [entry(password="private-password")],
                login_mode=LoginMode.ENTERPRISE,
                region="us-east-1",
            )[0]
            broken = self.make_repo(tmp, protector=BrokenProtector())

            with self.assertRaises(AccountRepositoryError) as raised:
                broken.get(item.id, include_secrets=True)

            self.assertNotIn("private-password", str(raised.exception))
            self.assertNotIn("protected", str(raised.exception))

    def test_history_contains_only_safe_operation_metadata(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.make_repo(tmp)
            item = repo.upsert_entries(
                [entry(password="one-time-secret")],
                login_mode=LoginMode.ENTERPRISE,
                region="us-east-1",
            )[0]
            repo.update_current_passwords([item.id], "current-secret")
            repo.mark_sold([item.id], "客户 A")

            with closing(sqlite3.connect(repo.path)) as connection:
                rows = connection.execute(
                    "SELECT operation, detail FROM operation_history"
                ).fetchall()

            serialized = repr(rows)
            self.assertNotIn("one-time-secret", serialized)
            self.assertNotIn("current-secret", serialized)
            self.assertIn("password_updated", serialized)
            self.assertIn("marked_sold", serialized)

    def test_credential_is_encrypted_and_round_trips_with_success_status(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.make_repo(tmp)
            item = repo.upsert_entries(
                [entry()], login_mode=LoginMode.ENTERPRISE, region="us-east-1"
            )[0]
            credential = CredentialRecord(
                email="admin-user", auth_method="idc", provider="Enterprise",
                refresh_token="refresh-secret", client_secret="client-secret",
                start_url="https://portal.example/start",
            )

            repo.save_credential(item.id, credential)
            restored = repo.load_credential(item.id)
            account = repo.get(item.id)

            self.assertEqual(credential.as_add_request(), restored.as_add_request())
            self.assertIs(CredentialStatus.VALID, account.credential_status)
            self.assertIs(LoginStatus.SUCCESS, account.login_status)
            raw = repo.path.read_bytes()
            self.assertNotIn(b"refresh-secret", raw)
            self.assertNotIn(b"client-secret", raw)

    def test_login_failure_stores_only_safe_diagnostics(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.make_repo(tmp)
            item = repo.upsert_entries(
                [entry()], login_mode=LoginMode.ENTERPRISE, region="us-east-1"
            )[0]

            repo.mark_login_running([item.id])
            repo.mark_login_failed(item.id, "invalid_credentials", "password")

            failed = repo.get(item.id)
            self.assertIs(LoginStatus.FAILED, failed.login_status)
            self.assertEqual("invalid_credentials", failed.last_error_code)
            self.assertEqual("password", failed.last_error_stage)


if __name__ == "__main__":
    unittest.main()
