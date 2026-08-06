from __future__ import annotations

import os
import stat
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

from .credential_models import CredentialRecord


class ApiKeyExportError(RuntimeError):
    pass


#: 与 `account_manager_app.REGION_DISPLAY_NAMES` 同源的默认区。空 region 归到这里，
#: 免得导出一个文件名带空字段的清单。
DEFAULT_REGION = "us-east-1"


def normalize_region(region: str | None) -> str:
    """归一化区域，供文件名使用。大小写/空白不该分出两个文件。"""
    return (region or "").strip().lower() or DEFAULT_REGION


@dataclass(frozen=True, slots=True)
class ApiKeyExportReport:
    #: 主清单路径。多区时指第一个区（按 `paths_by_region` 的顺序）的文件，
    #: 保持既有调用方（读 `report.path` 发事件）不用改。
    path: Path
    with_key: int
    without_key: int
    #: 区域 → 该区清单路径。单区时也填，调用方可统一按这个展示。
    paths_by_region: dict[str, Path] = field(default_factory=dict)
    #: 区域 → 该区导出的 key 数量。
    counts_by_region: dict[str, int] = field(default_factory=dict)

    @property
    def region_count(self) -> int:
        return len(self.paths_by_region)


class ApiKeyExporter:
    """把 ksk_ API Key 导出成纯清单:每行一个 ksk_xxx,无前缀无注释。

    **按区域分文件**:美区与欧区的 key 不能混在一个清单里——下游按区导入时
    混着的清单要人工挑。文件名形如 `kiro-apikeys-us-east-1-20260806-101530.txt`。
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

        # 按区域分桶。dict 保插入序 → 文件生成顺序与记录顺序一致，可复现。
        buckets: dict[str, list[str]] = {}
        for record in with_key:
            key = normalize_region(record.region)
            buckets.setdefault(key, []).append((record.kiro_api_key or "").strip())

        paths_by_region: dict[str, Path] = {}
        counts_by_region: dict[str, int] = {}
        try:
            output_directory.mkdir(parents=True, exist_ok=True)
            for region, keys in buckets.items():
                path = self._unused_path(
                    output_directory / f"kiro-apikeys-{region}-{stamp}.txt"
                )
                self._atomic_write(path, "\n".join(keys) + "\n")
                paths_by_region[region] = path
                counts_by_region[region] = len(keys)
        except ApiKeyExportError:
            raise
        except OSError as error:
            raise ApiKeyExportError("API Key 清单导出目录无法创建或写入") from error

        return ApiKeyExportReport(
            path=next(iter(paths_by_region.values())),
            with_key=len(with_key),
            without_key=len(without_key),
            paths_by_region=paths_by_region,
            counts_by_region=counts_by_region,
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
