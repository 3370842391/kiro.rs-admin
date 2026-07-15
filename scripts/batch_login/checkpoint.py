from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

from .models import LoginMode, ResultStatus, RunRecord


TERMINAL_SUCCESS = {ResultStatus.SUCCESS.value, ResultStatus.DUPLICATE.value}


class CheckpointStore:
    def __init__(self, path: Path):
        self.path = path
        self._latest: dict[tuple[int, str, str], dict[str, Any]] = {}
        if path.exists():
            lines = path.read_text(encoding="utf-8").splitlines()
            for index, line in enumerate(lines):
                if not line.strip():
                    continue
                try:
                    item = json.loads(line)
                    key = (
                        int(item["lineNumber"]),
                        str(item["accountHash"]),
                        str(item["mode"]),
                    )
                except (json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
                    if index == len(lines) - 1:
                        continue
                    raise ValueError(f"checkpoint 第 {index + 1} 行无效") from error
                if not isinstance(item, dict):
                    if index == len(lines) - 1:
                        continue
                    raise ValueError(f"checkpoint 第 {index + 1} 行无效")
                self._latest[key] = item

    def append(self, record: RunRecord) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        payload = record.as_json()
        serialized = json.dumps(
            payload,
            ensure_ascii=False,
            separators=(",", ":"),
        )
        with self.path.open("a", encoding="utf-8", newline="\n") as handle:
            handle.write(serialized + "\n")
            handle.flush()
            os.fsync(handle.fileno())
        key = (record.line_number, record.account_hash, record.mode.value)
        self._latest[key] = payload

    def should_run(
        self,
        line_number: int,
        account_hash: str,
        mode: LoginMode,
        resume: bool,
    ) -> bool:
        if not resume:
            return True
        item = self._latest.get((line_number, account_hash, mode.value))
        if item is None:
            return True
        if item.get("status") in TERMINAL_SUCCESS:
            return False
        return item.get("retryable") is True


def exit_code_for(statuses: list[ResultStatus]) -> int:
    return (
        0
        if all(
            status in {ResultStatus.SUCCESS, ResultStatus.DUPLICATE}
            for status in statuses
        )
        else 2
    )
