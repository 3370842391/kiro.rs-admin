from __future__ import annotations

import hashlib
import json
import os
import re
import stat
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from uuid import uuid4

from .credential_models import CredentialRecord
from .redaction import mask_account


class OidcExportError(RuntimeError):
    pass


class OidcExportMode(str, Enum):
    MERGED = "merged"
    PER_ACCOUNT = "per_account"
    BOTH = "both"


@dataclass(frozen=True, slots=True)
class OidcExportReport:
    record_count: int
    merged_path: Path | None
    account_paths: tuple[Path, ...]


class OidcCredentialExporter:
    def __init__(
        self,
        *,
        now: Callable[[], datetime] | None = None,
        warning_sink: Callable[[str], None] | None = None,
    ):
        self.now = now or (lambda: datetime.now(timezone.utc))
        self.warning_sink = warning_sink or (lambda _message: None)

    def project(self, record: CredentialRecord) -> dict[str, str]:
        payload: dict[str, str | None] = {
            "email": record.email,
            "authMethod": record.auth_method.lower(),
            "provider": record.provider,
            "region": record.region,
            "startUrl": record.start_url,
            "refreshToken": record.refresh_token,
            "clientId": record.client_id,
            "clientSecret": record.client_secret,
            "profileArn": record.profile_arn,
            "tokenEndpoint": record.token_endpoint,
            "scopes": record.scopes,
            "issuerUrl": record.issuer_url,
        }
        return {
            key: value if key == "refreshToken" else value.strip()
            for key, value in payload.items()
            if isinstance(value, str) and value.strip()
        }

    def export(
        self,
        records: Sequence[CredentialRecord],
        *,
        output_directory: Path,
        mode: OidcExportMode,
    ) -> OidcExportReport:
        items = list(records)
        invalid = [item for item in items if not (item.refresh_token or "").strip()]
        if invalid:
            masked = ", ".join(mask_account(item.email) for item in invalid)
            raise OidcExportError(f"以下账号缺少 refreshToken：{masked}")
        try:
            selected_mode = OidcExportMode(mode)
        except ValueError as error:
            raise OidcExportError("OIDC 导出方式无效") from error

        payloads = [self.project(item) for item in items]
        output_directory = Path(output_directory)
        stamp = self.now().strftime("%Y%m%d-%H%M%S")
        reserved: set[str] = set()
        merged_path: Path | None = None
        account_paths: list[Path] = []

        if selected_mode in {OidcExportMode.MERGED, OidcExportMode.BOTH}:
            merged_path = self._unused_path(
                output_directory / f"kiro-accounts-{stamp}.oidc.json",
                reserved,
            )
        if selected_mode in {OidcExportMode.PER_ACCOUNT, OidcExportMode.BOTH}:
            for index, record in enumerate(items, start=1):
                account_paths.append(
                    self._unused_path(
                        output_directory / self._account_filename(index, record),
                        reserved,
                    )
                )

        try:
            output_directory.mkdir(parents=True, exist_ok=True)
            if merged_path is not None:
                self._atomic_write(merged_path, payloads)
            for path, payload in zip(account_paths, payloads):
                self._atomic_write(path, [payload])
        except OidcExportError:
            raise
        except OSError as error:
            raise OidcExportError("OIDC 导出目录无法创建或写入") from error

        return OidcExportReport(
            record_count=len(items),
            merged_path=merged_path,
            account_paths=tuple(account_paths),
        )

    @staticmethod
    def _account_filename(index: int, record: CredentialRecord) -> str:
        safe = re.sub(r"[^A-Za-z0-9._-]+", "-", record.email).strip(". -_")
        safe = safe[:48].rstrip(". ") or "account"
        digest_source = "\0".join(record.dedupe_key()).encode("utf-8")
        digest = hashlib.sha256(digest_source).hexdigest()[:8]
        return f"{index:03d}-{safe}-{digest}.oidc.json"

    @staticmethod
    def _unused_path(path: Path, reserved: set[str] | None = None) -> Path:
        reserved = reserved if reserved is not None else set()
        candidate = path
        counter = 2
        suffix = "".join(path.suffixes)
        base = path.name[: -len(suffix)] if suffix else path.name
        while candidate.exists() or candidate.name.casefold() in reserved:
            candidate = path.with_name(f"{base}-{counter}{suffix}")
            counter += 1
        reserved.add(candidate.name.casefold())
        return candidate

    def _atomic_write(self, path: Path, payload: object) -> None:
        temporary = path.with_name(f".{path.name}.{uuid4().hex}.tmp")
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            with temporary.open("x", encoding="utf-8", newline="\n") as handle:
                json.dump(payload, handle, ensure_ascii=False, indent=2)
                handle.write("\n")
                handle.flush()
                os.fsync(handle.fileno())
            try:
                os.chmod(temporary, stat.S_IRUSR | stat.S_IWUSR)
            except OSError:
                self._warn_permissions()
            os.replace(temporary, path)
        except Exception as error:
            temporary.unlink(missing_ok=True)
            if isinstance(error, OidcExportError):
                raise
            raise OidcExportError("OIDC JSON 写入失败") from error

    def _warn_permissions(self) -> None:
        try:
            self.warning_sink(
                "无法确认 OIDC JSON 文件权限，请手动限制为仅当前用户可读写"
            )
        except Exception:
            return
