from __future__ import annotations

import os
import stat
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

from .credential_models import CredentialRecord


class ApiKeyExportError(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class ApiKeyExportReport:
    path: Path
    with_key: int
    without_key: int


class ApiKeyExporter:
    """把 ksk_ API Key 导出成文本清单:`login = <账号> / apikey = ksk_xxx`,每行一条。

    没有 key 的账号列在文件尾部注释里,便于人工补。
    """

    def __init__(
        self,
        *,
        now: Callable[[], datetime] | None = None,
        warning_sink: Callable[[str], None] | None = None,
    ):
        self.now = now or (lambda: datetime.now(timezone.utc))
        self.warning_sink = warning_sink or (lambda _message: None)

    def export(
        self,
        records: Sequence[CredentialRecord],
        *,
        output_directory: Path,
    ) -> ApiKeyExportReport | None:
        with_key = [r for r in records if (r.kiro_api_key or "").strip()]
        without_key = [r for r in records if not (r.kiro_api_key or "").strip()]
        if not with_key:
            return None

        output_directory = Path(output_directory)
        stamp = self.now().strftime("%Y%m%d-%H%M%S")
        path = self._unused_path(output_directory / f"kiro-apikeys-{stamp}.txt")

        lines = [
            f"login = {r.email} / apikey = {(r.kiro_api_key or '').strip()}"
            for r in with_key
        ]
        body = "\n".join(lines)
        if without_key:
            body += "\n\n# 未拿到 API Key(需人工补):\n"
            body += "\n".join(f"# {r.email}" for r in without_key)
        body += "\n"

        try:
            output_directory.mkdir(parents=True, exist_ok=True)
            self._atomic_write(path, body)
        except ApiKeyExportError:
            raise
        except OSError as error:
            raise ApiKeyExportError("API Key 清单导出目录无法创建或写入") from error

        return ApiKeyExportReport(
            path=path,
            with_key=len(with_key),
            without_key=len(without_key),
        )

    @staticmethod
    def _unused_path(path: Path) -> Path:
        candidate = path
        counter = 2
        suffix = path.suffix
        base = path.name[: -len(suffix)] if suffix else path.name
        while candidate.exists():
            candidate = path.with_name(f"{base}-{counter}{suffix}")
            counter += 1
        return candidate

    def _atomic_write(self, path: Path, text: str) -> None:
        temporary = path.with_name(f".{path.name}.{uuid4().hex}.tmp")
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            with temporary.open("x", encoding="utf-8", newline="\n") as handle:
                handle.write(text)
                handle.flush()
                os.fsync(handle.fileno())
            try:
                os.chmod(temporary, stat.S_IRUSR | stat.S_IWUSR)
            except OSError:
                self._warn_permissions()
            os.replace(temporary, path)
        except Exception as error:
            temporary.unlink(missing_ok=True)
            if isinstance(error, ApiKeyExportError):
                raise
            raise ApiKeyExportError("API Key 清单写入失败") from error

    def _warn_permissions(self) -> None:
        try:
            self.warning_sink(
                "无法确认 API Key 清单文件权限，请手动限制为仅当前用户可读写"
            )
        except Exception:
            return
