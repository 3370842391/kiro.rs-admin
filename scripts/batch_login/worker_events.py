from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Any

from .models import LoginMode


class ResultMode(str, Enum):
    SAVE_ONLY = "save_only"
    SAVE_AND_IMPORT = "save_and_import"


@dataclass(slots=True, frozen=True)
class WorkerEvent:
    kind: str
    payload: dict[str, Any]


@dataclass(slots=True, frozen=True)
class LocalRunSettings:
    mode: LoginMode
    region: str
    start_url: str | None
    headless: bool
    timeout_seconds: float
    mfa_timeout_seconds: float
    result_mode: ResultMode
    credential_path: Path
    checkpoint_path: Path
    password_vault_path: Path = Path("enterprise-passwords.sqlite3")
    resume: bool = False


@dataclass(slots=True)
class BatchSummary:
    total: int
    succeeded: int = 0
    duplicate: int = 0
    failed: int = 0
    manual_required: int = 0
    cancelled: int = 0
    imported: int = 0
