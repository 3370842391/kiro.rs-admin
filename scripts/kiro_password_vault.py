from __future__ import annotations

import argparse
import json
import os
import stat
import sys
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

from batch_login.password_vault import PasswordVault


def export_records(vault: PasswordVault, output: Path) -> int:
    """Explicitly decrypt a vault into a user-selected recovery JSON file."""
    output = Path(output)
    temporary = output.with_name(f".{output.name}.{uuid4().hex}.tmp")
    records = vault.records()
    payload = {
        "version": 1,
        "exportedAt": datetime.now(timezone.utc)
        .isoformat()
        .replace("+00:00", "Z"),
        "passwords": [
            {
                "recordId": record.record_id,
                "status": record.status.value,
                "account": record.account,
                "password": record.password,
            }
            for record in records
        ],
    }
    try:
        output.parent.mkdir(parents=True, exist_ok=True)
        with temporary.open("x", encoding="utf-8", newline="\n") as handle:
            json.dump(payload, handle, ensure_ascii=False, indent=2)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, stat.S_IRUSR | stat.S_IWUSR)
        os.replace(temporary, output)
        return len(records)
    except Exception:
        temporary.unlink(missing_ok=True)
        raise


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="显式导出企业账号密码保险库（输出文件包含明文密码）"
    )
    parser.add_argument("--vault", required=True, type=Path, help="SQLite 密码保险库")
    parser.add_argument("--output", required=True, type=Path, help="明文恢复 JSON 输出路径")
    parser.add_argument(
        "--confirm-plaintext",
        action="store_true",
        help="确认输出文件包含明文密码",
    )
    return parser


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    if not args.confirm_plaintext:
        print("拒绝导出：请显式添加 --confirm-plaintext", file=sys.stderr)
        return 2
    try:
        count = export_records(PasswordVault(args.vault), args.output)
    except Exception as error:
        print(f"密码保险库导出失败：{error}", file=sys.stderr)
        return 1
    print(f"已导出 {count} 条密码记录到 {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
