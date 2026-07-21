import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.account_repository import AccountRepository, AccountRepositoryError
from batch_login.models import AccountEntry, LoginMode


class Protector:
    def protect(self, value):
        return b"p:" + value[::-1]

    def unprotect(self, value):
        return value.removeprefix(b"p:")[::-1]


class QuotaRepositoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.path = Path(self.temp.name) / "accounts.sqlite3"
        self.repo = AccountRepository(self.path, protector=Protector())
        self.account = self.repo.upsert_entries(
            [AccountEntry(1, "acc", "pw", "https://d-1.awsapps.com/start")],
            login_mode=LoginMode.ENTERPRISE, region="us-east-1",
        )[0]

    def tearDown(self):
        self.temp.cleanup()

    def test_save_and_load_round_trip(self):
        self.repo.save_quota(
            self.account.id, remaining=416.5, total=550, used=133.5,
            subscription="Kiro Pro", free_trial=True, next_reset="2025-08-01T00:00:00Z",
        )
        loaded = self.repo.load_quota(self.account.id)
        self.assertEqual(416.5, loaded["remaining"])
        self.assertEqual(550.0, loaded["total"])
        self.assertEqual("Kiro Pro", loaded["subscription"])
        self.assertTrue(loaded["free_trial"])
        self.assertEqual("2025-08-01T00:00:00Z", loaded["next_reset"])
        self.assertIn("updated_at", loaded)

    def test_upsert_overwrites(self):
        self.repo.save_quota(self.account.id, remaining=100, total=500, used=400,
                             subscription="Free", free_trial=False, next_reset=None)
        self.repo.save_quota(self.account.id, remaining=50, total=500, used=450,
                             subscription="Free", free_trial=False, next_reset=None)
        self.assertEqual(50.0, self.repo.load_quota(self.account.id)["remaining"])

    def test_load_missing_returns_none(self):
        self.assertIsNone(self.repo.load_quota(self.account.id))

    def test_load_quotas_batch(self):
        second = self.repo.upsert_entries(
            [AccountEntry(2, "acc2", "pw", "https://d-1.awsapps.com/start")],
            login_mode=LoginMode.ENTERPRISE, region="us-east-1",
        )[0]
        self.repo.save_quota(self.account.id, remaining=1, total=2, used=1,
                             subscription=None, free_trial=False, next_reset=None)
        result = self.repo.load_quotas([self.account.id, second.id])
        self.assertIn(self.account.id, result)
        self.assertNotIn(second.id, result)  # 未存过额度的不返回

    def test_quota_table_created_on_existing_db(self):
        # 重新打开(metadata 已存在,_initialize 会 early-return),额度表仍应可用
        reopened = AccountRepository(self.path, protector=Protector())
        reopened.save_quota(self.account.id, remaining=9, total=10, used=1,
                            subscription=None, free_trial=False, next_reset=None)
        self.assertEqual(9.0, reopened.load_quota(self.account.id)["remaining"])

    def test_save_quota_unknown_account_raises(self):
        with self.assertRaises(AccountRepositoryError):
            self.repo.save_quota(99999, remaining=1, total=1, used=0,
                                subscription=None, free_trial=False, next_reset=None)


if __name__ == "__main__":
    unittest.main()
