import json
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.checkpoint import CheckpointStore, exit_code_for
from batch_login.models import LoginMode, ResultStatus, RunRecord


class CheckpointTests(unittest.TestCase):
    def test_append_never_serializes_password_and_fsyncs_readable_jsonl(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "result.jsonl"
            store = CheckpointStore(path)
            record = RunRecord(
                run_id="run-1",
                line_number=3,
                account_hash="hash",
                account_masked="us***@example.com",
                mode=LoginMode.MICROSOFT,
                status=ResultStatus.SUCCESS,
                stage="done",
                attempts=1,
                timestamp="2026-07-15T00:00:00Z",
                credential_id=12,
            )
            store.append(record)
            raw = path.read_text(encoding="utf-8")
            self.assertNotIn("password", raw.casefold())
            self.assertEqual(12, json.loads(raw)["credentialId"])

    def test_resume_skips_success_and_retries_retryable_failure_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "result.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps(
                            {
                                "lineNumber": 1,
                                "accountHash": "a",
                                "mode": "enterprise",
                                "status": "success",
                                "retryable": False,
                            }
                        ),
                        json.dumps(
                            {
                                "lineNumber": 2,
                                "accountHash": "b",
                                "mode": "enterprise",
                                "status": "failed",
                                "retryable": True,
                            }
                        ),
                        json.dumps(
                            {
                                "lineNumber": 3,
                                "accountHash": "c",
                                "mode": "enterprise",
                                "status": "failed",
                                "retryable": False,
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            store = CheckpointStore(path)
            self.assertFalse(store.should_run(1, "a", LoginMode.ENTERPRISE, resume=True))
            self.assertTrue(store.should_run(2, "b", LoginMode.ENTERPRISE, resume=True))
            self.assertFalse(store.should_run(3, "c", LoginMode.ENTERPRISE, resume=True))

    def test_latest_record_wins_for_same_account_and_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "result.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps(
                            {
                                "lineNumber": 1,
                                "accountHash": "a",
                                "mode": "enterprise",
                                "status": "failed",
                                "retryable": True,
                            }
                        ),
                        json.dumps(
                            {
                                "lineNumber": 1,
                                "accountHash": "a",
                                "mode": "enterprise",
                                "status": "success",
                                "retryable": False,
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            store = CheckpointStore(path)
            self.assertFalse(store.should_run(1, "a", LoginMode.ENTERPRISE, resume=True))

    def test_truncated_final_line_is_ignored_but_corrupt_middle_line_is_rejected(self):
        valid = json.dumps(
            {
                "lineNumber": 1,
                "accountHash": "a",
                "mode": "enterprise",
                "status": "success",
                "retryable": False,
            }
        )
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "result.jsonl"
            path.write_text(valid + "\n{" + "\n", encoding="utf-8")
            store = CheckpointStore(path)
            self.assertFalse(store.should_run(1, "a", LoginMode.ENTERPRISE, resume=True))

            path.write_text(valid + "\nnot-json\n" + valid + "\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "第 2 行"):
                CheckpointStore(path)

    def test_recovery_repairs_truncated_tail_before_next_append(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "result.jsonl"
            path.write_text("{", encoding="utf-8")
            store = CheckpointStore(path)
            store.append(
                RunRecord(
                    run_id="run-after-crash",
                    line_number=2,
                    account_hash="hash",
                    account_masked="us***@example.com",
                    mode=LoginMode.MICROSOFT,
                    status=ResultStatus.SUCCESS,
                    stage="done",
                    attempts=1,
                    timestamp="2026-07-15T00:00:00Z",
                )
            )

            reopened = CheckpointStore(path)
            self.assertFalse(
                reopened.should_run(2, "hash", LoginMode.MICROSOFT, resume=True)
            )

    def test_exit_codes_match_contract(self):
        self.assertEqual(
            0,
            exit_code_for([ResultStatus.SUCCESS, ResultStatus.DUPLICATE]),
        )
        self.assertEqual(
            2,
            exit_code_for([ResultStatus.SUCCESS, ResultStatus.FAILED]),
        )


if __name__ == "__main__":
    unittest.main()
