from __future__ import annotations

import json
import os
import stat
from collections.abc import Callable
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from uuid import uuid4

from .credential_models import CredentialRecord


class CredentialStoreError(RuntimeError):
    pass


class CredentialStore:
    def __init__(
        self,
        path: Path,
        *,
        warning_sink: Callable[[str], None] | None = None,
    ):
        self.path = path
        self.warning_sink = warning_sink or (lambda _message: None)

    def load(self) -> list[CredentialRecord]:
        if not self.path.exists():
            return []
        try:
            payload: Any = json.loads(self.path.read_text(encoding="utf-8-sig"))
            if (
                not isinstance(payload, dict)
                or payload.get("version") != 1
                or not isinstance(payload.get("credentials"), list)
            ):
                raise ValueError("invalid bundle")
            records = []
            for item in payload["credentials"]:
                if not isinstance(item, dict):
                    raise ValueError("invalid credential")
                records.append(CredentialRecord.from_add_request(item))
            return records
        except (OSError, json.JSONDecodeError, TypeError, ValueError) as error:
            raise CredentialStoreError("凭据文件格式无效或无法读取") from error

    def append(self, record: CredentialRecord) -> bool:
        records = self.load()
        known = {item.dedupe_key() for item in records}
        if record.dedupe_key() in known:
            return False
        records.append(record)
        self._write(records)
        return True

    def _write(self, records: list[CredentialRecord]) -> None:
        temp = self.path.with_name(f".{self.path.name}.{uuid4().hex}.tmp")
        payload = {
            "version": 1,
            "generatedAt": datetime.now(timezone.utc)
            .isoformat()
            .replace("+00:00", "Z"),
            "credentials": [item.as_add_request() for item in records],
        }
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            with temp.open("x", encoding="utf-8", newline="\n") as handle:
                json.dump(payload, handle, ensure_ascii=False, indent=2)
                handle.write("\n")
                handle.flush()
                os.fsync(handle.fileno())
            try:
                os.chmod(temp, stat.S_IRUSR | stat.S_IWUSR)
            except OSError:
                self._warn_permissions()
            os.replace(temp, self.path)
        except Exception as error:
            temp.unlink(missing_ok=True)
            if isinstance(error, CredentialStoreError):
                raise
            raise CredentialStoreError("完整凭据 JSON 写入失败") from error

    def _warn_permissions(self) -> None:
        try:
            self.warning_sink(
                "无法确认凭据文件权限，请手动限制为仅当前用户可读写"
            )
        except Exception:
            return
