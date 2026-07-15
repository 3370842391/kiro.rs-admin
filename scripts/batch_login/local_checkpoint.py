from __future__ import annotations

import json
import os
from dataclasses import dataclass, replace
from datetime import datetime, timezone
from hashlib import sha256
from pathlib import Path
from typing import Any

from .redaction import mask_account, redact_text


IMPORT_STATUSES = {"imported", "verified", "duplicate", "failed"}


def account_hash(account: str) -> str:
    return sha256(account.casefold().encode("utf-8")).hexdigest()


def normalize_scope(scope: str) -> str:
    return scope.casefold().rstrip("/")


def resume_key(account: str, mode: str, scope: str) -> tuple[str, str, str]:
    return mode, account_hash(account), normalize_scope(scope)


def _timestamp() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


@dataclass(slots=True)
class LocalRunRecord:
    run_id: str
    line_number: int
    account_hash: str
    account_masked: str
    mode: str
    scope: str
    status: str
    stage: str
    timestamp: str
    retryable: bool
    credential_saved: bool
    code: str | None = None
    message: str | None = None
    import_status: str | None = None
    credential_id: int | None = None

    @classmethod
    def for_account(
        cls,
        *,
        run_id: str,
        line_number: int,
        account: str,
        mode: str,
        scope: str,
        status: str,
        stage: str,
        retryable: bool,
        credential_saved: bool,
        code: str | None = None,
        message: str | None = None,
    ) -> LocalRunRecord:
        return cls(
            run_id=run_id,
            line_number=line_number,
            account_hash=account_hash(account),
            account_masked=mask_account(account),
            mode=mode,
            scope=normalize_scope(scope),
            status=status,
            stage=redact_text(stage),
            timestamp=_timestamp(),
            retryable=retryable,
            credential_saved=credential_saved,
            code=redact_text(code) if code is not None else None,
            message=redact_text(message) if message is not None else None,
        )

    @classmethod
    def success(
        cls,
        *,
        run_id: str,
        line_number: int,
        account: str,
        mode: str,
        scope: str,
        credential_saved: bool,
    ) -> LocalRunRecord:
        return cls.for_account(
            run_id=run_id,
            line_number=line_number,
            account=account,
            mode=mode,
            scope=scope,
            status="success",
            stage="saved",
            retryable=False,
            credential_saved=credential_saved,
        )

    @property
    def key(self) -> tuple[str, str, str]:
        return self.mode, self.account_hash, normalize_scope(self.scope)

    def as_json(self) -> dict[str, Any]:
        return {
            "runId": self.run_id,
            "lineNumber": self.line_number,
            "accountHash": self.account_hash,
            "accountMasked": mask_account(self.account_masked),
            "mode": self.mode,
            "scope": normalize_scope(self.scope),
            "status": self.status,
            "stage": redact_text(self.stage),
            "timestamp": self.timestamp,
            "retryable": self.retryable,
            "credentialSaved": self.credential_saved,
            "code": redact_text(self.code) if self.code is not None else None,
            "message": redact_text(self.message) if self.message is not None else None,
            "importStatus": self.import_status,
            "credentialId": self.credential_id,
        }

    @classmethod
    def from_json(cls, item: dict[str, Any]) -> LocalRunRecord:
        return cls(
            run_id=str(item["runId"]),
            line_number=int(item["lineNumber"]),
            account_hash=str(item["accountHash"]),
            account_masked=str(item["accountMasked"]),
            mode=str(item["mode"]),
            scope=normalize_scope(str(item.get("scope") or "")),
            status=str(item["status"]),
            stage=str(item["stage"]),
            timestamp=str(item["timestamp"]),
            retryable=item.get("retryable") is True,
            credential_saved=item.get("credentialSaved") is True,
            code=str(item["code"]) if item.get("code") is not None else None,
            message=(
                str(item["message"]) if item.get("message") is not None else None
            ),
            import_status=(
                str(item["importStatus"])
                if item.get("importStatus") is not None
                else None
            ),
            credential_id=(
                int(item["credentialId"])
                if item.get("credentialId") is not None
                else None
            ),
        )


class LocalCheckpointStore:
    def __init__(self, path: Path):
        self.path = path
        self._latest: dict[tuple[str, str, str], LocalRunRecord] = {}
        self._load()

    def _load(self) -> None:
        if not self.path.exists():
            return
        raw = self.path.read_text(encoding="utf-8")
        lines = raw.splitlines()
        complete: list[str] = []
        truncated_tail = False
        for index, line in enumerate(lines):
            if not line.strip():
                complete.append(line)
                continue
            try:
                item = json.loads(line)
                if not isinstance(item, dict):
                    raise ValueError("not an object")
                record = LocalRunRecord.from_json(item)
            except (json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
                if index == len(lines) - 1:
                    truncated_tail = True
                    break
                raise ValueError(f"checkpoint 第 {index + 1} 行无效") from error
            complete.append(line)
            self._latest[record.key] = record
        if truncated_tail:
            repaired = "\n".join(complete)
            if repaired:
                repaired += "\n"
            self.path.write_text(repaired, encoding="utf-8", newline="\n")

    def should_run(
        self,
        *,
        account: str,
        mode: str,
        scope: str,
        resume: bool,
    ) -> bool:
        if not resume:
            return True
        record = self._latest.get(resume_key(account, mode, scope))
        if record is None:
            return True
        if record.status == "success" and record.credential_saved:
            return False
        if record.status in {"manual_required", "cancelled"}:
            return True
        return record.retryable

    def append(self, record: LocalRunRecord) -> LocalRunRecord:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        serialized = json.dumps(
            record.as_json(), ensure_ascii=False, separators=(",", ":")
        )
        needs_newline = self.path.exists() and self.path.stat().st_size > 0
        with self.path.open("a", encoding="utf-8", newline="\n") as handle:
            if needs_newline:
                with self.path.open("rb") as reader:
                    reader.seek(-1, os.SEEK_END)
                    if reader.read(1) not in {b"\n", b"\r"}:
                        handle.write("\n")
            handle.write(serialized + "\n")
            handle.flush()
            os.fsync(handle.fileno())
        self._latest[record.key] = record
        return record

    def append_import_result(
        self,
        previous: LocalRunRecord,
        *,
        import_status: str,
        credential_id: int | None,
        message: str | None = None,
    ) -> LocalRunRecord:
        if import_status not in IMPORT_STATUSES:
            raise ValueError("导入状态无效")
        record = replace(
            previous,
            stage="import",
            timestamp=_timestamp(),
            import_status=import_status,
            credential_id=credential_id,
            message=redact_text(message) if message is not None else None,
        )
        return self.append(record)
