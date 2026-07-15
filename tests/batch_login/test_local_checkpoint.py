import json
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.local_checkpoint import (
    LocalCheckpointStore,
    LocalRunRecord,
    account_hash,
    resume_key,
)
from batch_login.models import LoginMode
from batch_login.worker_events import (
    BatchSummary,
    LocalRunSettings,
    ResultMode,
    WorkerEvent,
)


class LocalCheckpointTests(unittest.TestCase):
    def success_record(self, *, line_number=2):
        return LocalRunRecord.success(
            run_id="run-1",
            line_number=line_number,
            account="Admin-User",
            mode="enterprise",
            scope="https://example.awsapps.com/start/",
            credential_saved=True,
        )

    def test_resume_key_does_not_depend_on_line_number_or_case(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = LocalCheckpointStore(Path(tmp) / "run.jsonl")
            store.append(self.success_record(line_number=2))

            self.assertFalse(
                store.should_run(
                    account="admin-user",
                    mode="enterprise",
                    scope="https://example.awsapps.com/start",
                    resume=True,
                )
            )
            raw = store.path.read_text(encoding="utf-8")
            self.assertNotIn("Admin-User", raw)
            self.assertNotIn("password", raw.casefold())
            self.assertNotIn("token", raw.casefold())

    def test_resume_decision_retries_manual_cancelled_and_retryable_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            cases = [
                ("manual_required", False, True),
                ("cancelled", False, True),
                ("failed", True, True),
                ("failed", False, False),
            ]
            for index, (status, retryable, expected) in enumerate(cases):
                with self.subTest(status=status, retryable=retryable):
                    store = LocalCheckpointStore(Path(tmp) / f"run-{index}.jsonl")
                    store.append(
                        LocalRunRecord.for_account(
                            run_id="run-1",
                            line_number=99,
                            account="user@example.com",
                            mode="microsoft",
                            scope="microsoft",
                            status=status,
                            stage="browser",
                            retryable=retryable,
                            credential_saved=False,
                            code="temporary" if retryable else "terminal",
                            message='refreshToken="must-not-leak" user@example.com',
                        )
                    )
                    self.assertEqual(
                        expected,
                        store.should_run(
                            account="USER@example.com",
                            mode="microsoft",
                            scope="MICROSOFT/",
                            resume=True,
                        ),
                    )
                    raw = store.path.read_text(encoding="utf-8")
                    self.assertNotIn("must-not-leak", raw)
                    self.assertNotIn("user@example.com", raw.casefold())

    def test_resume_false_and_unknown_account_always_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = LocalCheckpointStore(Path(tmp) / "run.jsonl")
            store.append(self.success_record())

            self.assertTrue(
                store.should_run(
                    account="Admin-User",
                    mode="enterprise",
                    scope="https://example.awsapps.com/start",
                    resume=False,
                )
            )
            self.assertTrue(
                store.should_run(
                    account="other-user",
                    mode="enterprise",
                    scope="https://example.awsapps.com/start",
                    resume=True,
                )
            )

    def test_import_result_appends_latest_record_without_secrets(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = LocalCheckpointStore(Path(tmp) / "run.jsonl")
            previous = self.success_record()
            store.append(previous)

            imported = store.append_import_result(
                previous,
                import_status="verified",
                credential_id=17,
                message='accessToken="must-not-leak"',
            )

            self.assertEqual("import", imported.stage)
            self.assertEqual("verified", imported.import_status)
            self.assertEqual(17, imported.credential_id)
            lines = store.path.read_text(encoding="utf-8").splitlines()
            self.assertEqual(2, len(lines))
            self.assertNotIn("must-not-leak", lines[-1])
            self.assertFalse(
                store.should_run(
                    account="admin-user",
                    mode="enterprise",
                    scope="https://example.awsapps.com/start",
                    resume=True,
                )
            )

    def test_import_result_rejects_unknown_status(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = LocalCheckpointStore(Path(tmp) / "run.jsonl")
            with self.assertRaises(ValueError):
                store.append_import_result(
                    self.success_record(),
                    import_status="unknown",
                    credential_id=None,
                )

    def test_truncated_last_line_is_repaired_before_append(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "run.jsonl"
            path.write_text(
                json.dumps(self.success_record().as_json()) + "\n{truncated",
                encoding="utf-8",
            )

            store = LocalCheckpointStore(path)
            store.append(self.success_record(line_number=10))

            parsed = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
            self.assertEqual(2, len(parsed))

    def test_hash_and_resume_key_are_stable(self):
        self.assertEqual(account_hash("Straße"), account_hash("STRASSE"))
        self.assertEqual(
            resume_key("Admin", "enterprise", "HTTPS://EXAMPLE/"),
            resume_key("admin", "enterprise", "https://example"),
        )


class WorkerEventModelTests(unittest.TestCase):
    def test_settings_and_summary_have_stable_defaults(self):
        settings = LocalRunSettings(
            mode=LoginMode.ENTERPRISE,
            region="us-east-1",
            start_url="https://example.awsapps.com/start",
            headless=False,
            timeout_seconds=180,
            mfa_timeout_seconds=300,
            result_mode=ResultMode.SAVE_ONLY,
            credential_path=Path("credentials.json"),
            checkpoint_path=Path("checkpoint.jsonl"),
        )
        summary = BatchSummary(total=3)
        event = WorkerEvent("batch_started", {"total": 3})

        self.assertFalse(settings.resume)
        self.assertEqual(0, summary.failed)
        self.assertEqual("save_only", settings.result_mode.value)
        self.assertEqual("batch_started", event.kind)


if __name__ == "__main__":
    unittest.main()
