import concurrent.futures
import sqlite3
import sys
import tempfile
import unittest
from contextlib import closing
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.password_vault import (
    PasswordStatus,
    PasswordVault,
    PasswordVaultError,
    StateTransitionError,
)
from kiro_password_vault import export_records


class ReversibleProtector:
    def protect(self, value: bytes) -> bytes:
        return b"protected:" + value[::-1]

    def unprotect(self, value: bytes) -> bytes:
        if not value.startswith(b"protected:"):
            raise ValueError("invalid protected value")
        return value.removeprefix(b"protected:")[::-1]


class CorruptingProtector(ReversibleProtector):
    def unprotect(self, value: bytes) -> bytes:
        return super().unprotect(value) + b"corrupt"


class PasswordVaultTests(unittest.TestCase):
    def make_vault(self, directory: str, *, protector=None) -> PasswordVault:
        return PasswordVault(
            Path(directory) / "password-recovery.sqlite3",
            protector=protector or ReversibleProtector(),
        )

    def test_prepare_generates_strong_password_and_persists_encrypted_values(self):
        with tempfile.TemporaryDirectory() as tmp:
            vault = self.make_vault(tmp)

            prepared = vault.prepare("admin-user24")

            self.assertEqual(PasswordStatus.PREPARED, prepared.status)
            self.assertGreaterEqual(len(prepared.password), 20)
            self.assertRegex(prepared.password, r"[A-Z]")
            self.assertRegex(prepared.password, r"[a-z]")
            self.assertRegex(prepared.password, r"[0-9]")
            self.assertRegex(prepared.password, r"[^A-Za-z0-9]")
            raw_database = (Path(tmp) / "password-recovery.sqlite3").read_bytes()
            self.assertNotIn(b"admin-user24", raw_database)
            self.assertNotIn(prepared.password.encode("utf-8"), raw_database)

    def test_database_uses_wal_and_full_synchronous_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "password-recovery.sqlite3"
            self.make_vault(tmp)

            with closing(sqlite3.connect(path)) as connection:
                journal_mode = connection.execute("PRAGMA journal_mode").fetchone()[0]
                synchronous = connection.execute("PRAGMA synchronous").fetchone()[0]

            self.assertEqual("wal", journal_mode.casefold())
            self.assertEqual(2, synchronous)

    def test_prepare_commits_then_reads_back_and_decrypts_before_returning(self):
        with tempfile.TemporaryDirectory() as tmp:
            vault = self.make_vault(tmp, protector=CorruptingProtector())

            with self.assertRaises(PasswordVaultError):
                vault.prepare("admin-user24")

            with closing(
                sqlite3.connect(Path(tmp) / "password-recovery.sqlite3")
            ) as connection:
                count = connection.execute("SELECT COUNT(*) FROM password_changes").fetchone()[0]
            self.assertEqual(1, count)

    def test_prepare_reuses_existing_unresolved_password_for_same_account(self):
        with tempfile.TemporaryDirectory() as tmp:
            vault = self.make_vault(tmp)

            first = vault.prepare("admin-user24")
            second = vault.prepare("admin-user24")

            self.assertEqual(first.record_id, second.record_id)
            self.assertEqual(first.password, second.password)

    def test_same_username_in_different_enterprise_directories_is_isolated(self):
        with tempfile.TemporaryDirectory() as tmp:
            vault = self.make_vault(tmp)

            first = vault.prepare("admin-user24", scope="us-east-1/d-111")
            second = vault.prepare("admin-user24", scope="us-east-1/d-222")

            self.assertNotEqual(first.record_id, second.record_id)
            self.assertNotEqual(first.password, second.password)

    def test_repr_hides_account_and_password(self):
        with tempfile.TemporaryDirectory() as tmp:
            prepared = self.make_vault(tmp).prepare("admin-user24")

            representation = repr(prepared)

            self.assertNotIn("admin-user24", representation)
            self.assertNotIn(prepared.password, representation)

    def test_status_transitions_are_strict_and_terminal_states_cannot_change(self):
        with tempfile.TemporaryDirectory() as tmp:
            vault = self.make_vault(tmp)
            prepared = vault.prepare("admin-user24")

            uncertain = vault.transition(prepared.record_id, PasswordStatus.UNCERTAIN)
            confirmed = vault.transition(uncertain.record_id, PasswordStatus.CONFIRMED)

            self.assertEqual(PasswordStatus.CONFIRMED, confirmed.status)
            with self.assertRaises(StateTransitionError):
                vault.transition(confirmed.record_id, PasswordStatus.REJECTED)

    def test_prepared_can_be_rejected_but_cannot_transition_to_itself(self):
        with tempfile.TemporaryDirectory() as tmp:
            vault = self.make_vault(tmp)
            prepared = vault.prepare("admin-user24")

            rejected = vault.transition(prepared.record_id, PasswordStatus.REJECTED)

            self.assertEqual(PasswordStatus.REJECTED, rejected.status)
            with self.assertRaises(StateTransitionError):
                vault.transition(rejected.record_id, PasswordStatus.REJECTED)

    def test_concurrent_prepare_for_same_account_returns_one_durable_password(self):
        with tempfile.TemporaryDirectory() as tmp:
            vault = self.make_vault(tmp)

            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
                prepared = list(
                    executor.map(lambda _index: vault.prepare("admin-user24"), range(16))
                )

            self.assertEqual(1, len({item.record_id for item in prepared}))
            self.assertEqual(1, len({item.password for item in prepared}))
            with closing(
                sqlite3.connect(Path(tmp) / "password-recovery.sqlite3")
            ) as connection:
                count = connection.execute("SELECT COUNT(*) FROM password_changes").fetchone()[0]
            self.assertEqual(1, count)

    def test_errors_do_not_echo_sensitive_values(self):
        with tempfile.TemporaryDirectory() as tmp:
            vault = self.make_vault(tmp, protector=CorruptingProtector())

            with self.assertRaises(PasswordVaultError) as raised:
                vault.prepare("admin-user24-sensitive")

            self.assertNotIn("admin-user24-sensitive", str(raised.exception))

    def test_records_can_be_decrypted_for_explicit_recovery_export(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "password-recovery.sqlite3"
            protector = ReversibleProtector()
            vault = PasswordVault(path, protector=protector)
            prepared = vault.prepare("admin-user24", scope="us-east-1/d-123")
            vault.transition(prepared.record_id, PasswordStatus.CONFIRMED)

            reopened = PasswordVault(path, protector=protector)
            records = reopened.records()

            self.assertEqual(1, len(records))
            self.assertEqual("admin-user24", records[0].account)
            self.assertEqual(prepared.password, records[0].password)
            self.assertEqual(PasswordStatus.CONFIRMED, records[0].status)

    def test_unresolved_returns_saved_candidate_without_generating_new_password(self):
        with tempfile.TemporaryDirectory() as tmp:
            vault = self.make_vault(tmp)
            self.assertIsNone(
                vault.unresolved("admin-user24", scope="us-east-1/d-123")
            )
            prepared = vault.prepare(
                "admin-user24", scope="us-east-1/d-123"
            )

            recovered = vault.unresolved(
                "admin-user24", scope="us-east-1/d-123"
            )

            self.assertEqual(prepared.record_id, recovered.record_id)
            self.assertEqual(prepared.password, recovered.password)

    def test_explicit_export_writes_recoverable_password_json(self):
        import json

        with tempfile.TemporaryDirectory() as tmp:
            vault = self.make_vault(tmp)
            prepared = vault.prepare("admin-user24", scope="us-east-1/d-123")
            output = Path(tmp) / "password-export.json"

            export_records(vault, output)

            payload = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual("admin-user24", payload["passwords"][0]["account"])
            self.assertEqual(prepared.password, payload["passwords"][0]["password"])
            self.assertEqual("prepared", payload["passwords"][0]["status"])


if __name__ == "__main__":
    unittest.main()
