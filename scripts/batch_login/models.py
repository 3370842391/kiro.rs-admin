from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from hashlib import sha256
from typing import Any


class LoginMode(str, Enum):
    ENTERPRISE = "enterprise"
    MICROSOFT = "microsoft"


class ResultStatus(str, Enum):
    SUCCESS = "success"
    DUPLICATE = "duplicate_credential"
    FAILED = "failed"
    MANUAL_REQUIRED = "manual_required"
    CANCELLED = "cancelled"


@dataclass(slots=True)
class AccountEntry:
    line_number: int
    account: str
    password: str = field(repr=False)

    @property
    def account_hash(self) -> str:
        return sha256(self.account.casefold().encode("utf-8")).hexdigest()


@dataclass(slots=True, frozen=True)
class ParseIssue:
    line_number: int
    code: str
    message: str


@dataclass(slots=True)
class ParseResult:
    entries: list[AccountEntry]
    issues: list[ParseIssue]


@dataclass(slots=True)
class LoginOutcome:
    status: ResultStatus
    credential_id: int | None = None
    code: str | None = None
    stage: str | None = None
    retryable: bool = False
    message: str | None = None
    duplicate: bool = False


@dataclass(slots=True)
class RunRecord:
    run_id: str
    line_number: int
    account_hash: str
    account_masked: str
    mode: LoginMode
    status: ResultStatus
    stage: str
    attempts: int
    timestamp: str
    credential_id: int | None = None
    code: str | None = None
    retryable: bool = False
    message: str | None = None

    def as_json(self) -> dict[str, Any]:
        from .redaction import mask_account, redact_text

        return {
            "runId": self.run_id,
            "lineNumber": self.line_number,
            "accountHash": self.account_hash,
            "accountMasked": mask_account(self.account_masked),
            "mode": self.mode.value,
            "status": self.status.value,
            "stage": redact_text(self.stage),
            "attempts": self.attempts,
            "timestamp": self.timestamp,
            "credentialId": self.credential_id,
            "code": redact_text(self.code) if self.code is not None else None,
            "retryable": self.retryable,
            "message": redact_text(self.message) if self.message is not None else None,
        }
