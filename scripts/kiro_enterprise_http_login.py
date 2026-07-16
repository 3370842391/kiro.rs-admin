from __future__ import annotations

import argparse
import asyncio
import json
import sys
from pathlib import Path

from batch_login.credential_store import CredentialStore
from batch_login.enterprise_http import (
    CurlCffiTransport,
    EnterpriseHttpClient,
    EnterpriseHttpError,
)
from batch_login.input_parser import parse_accounts
from batch_login.local_auth import EnterpriseSettings, LocalEnterpriseAuth
from batch_login.models import AccountEntry, LoginMode
from batch_login.password_vault import PasswordVault
from batch_login.redaction import mask_account, redact_text


DEFAULT_FORMAT = "login = {account} / onetime password = {password}"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="企业账号纯 HTTP 自动登录（不启动浏览器）"
    )
    parser.add_argument("--input", required=True, help="账号文件；- 表示从 stdin 读取")
    parser.add_argument("--format", default=DEFAULT_FORMAT, help="账号密码解析模板")
    parser.add_argument("--start-url", default="", help="企业 AWS apps Start URL")
    parser.add_argument("--region", default="us-east-1")
    parser.add_argument("--output", required=True, type=Path, help="完整凭据 JSON")
    parser.add_argument(
        "--password-vault",
        type=Path,
        help="DPAPI 加密密码保险库；默认位于输出 JSON 旁边",
    )
    parser.add_argument("--timeout", type=float, default=180)
    parser.add_argument("--resume", action="store_true", help="跳过输出 JSON 中已有账号")
    return parser


def validate_paths(args) -> None:
    if args.timeout <= 0:
        raise ValueError("timeout 必须大于 0")
    if not args.region.strip():
        raise ValueError("region 不能为空")
    if args.password_vault is None:
        args.password_vault = Path(str(args.output) + ".passwords.sqlite3")
    output = args.output.resolve()
    vault = args.password_vault.resolve()
    if args.input != "-" and Path(args.input).resolve() == output:
        raise ValueError("完整凭据 JSON 不能覆盖账号输入文件")
    if vault == output:
        raise ValueError("密码保险库不能覆盖完整凭据 JSON")
    if args.input != "-" and Path(args.input).resolve() == vault:
        raise ValueError("密码保险库不能覆盖账号输入文件")


def read_input(path: str) -> str:
    if path == "-":
        return sys.stdin.read()
    return Path(path).read_text(encoding="utf-8-sig")


async def process_entries(
    entries: list[AccountEntry],
    auth,
    store,
    *,
    start_url: str,
    region: str,
    emit=lambda _event: None,
) -> dict[str, int]:
    summary = {"total": len(entries), "succeeded": 0, "failed": 0}
    for index, entry in enumerate(entries, start=1):
        emit(
            {
                "kind": "account_started",
                "index": index,
                "total": len(entries),
                "account": mask_account(entry.account),
            }
        )
        try:
            effective_start_url = entry.start_url or start_url
            if not effective_start_url.strip():
                raise EnterpriseHttpError(
                    "missing_start_url",
                    "configuration",
                    False,
                    "企业 Start URL 不能为空",
                )
            settings = EnterpriseSettings(effective_start_url, region)
            record = await auth.login(entry, settings)
            store.append(record)
            summary["succeeded"] += 1
            emit({"kind": "account_finished", "status": "success"})
        except EnterpriseHttpError as error:
            summary["failed"] += 1
            emit(
                {
                    "kind": "account_finished",
                    "status": "failed",
                    "code": error.code,
                    "stage": error.stage,
                }
            )
    return summary


async def async_main(args) -> int:
    validate_paths(args)
    parsed = parse_accounts(read_input(args.input), args.format, LoginMode.ENTERPRISE)
    for issue in parsed.issues:
        print(
            json.dumps(
                {
                    "kind": "input_issue",
                    "line": issue.line_number,
                    "code": issue.code,
                    "message": issue.message,
                },
                ensure_ascii=False,
            ),
            file=sys.stderr,
        )
    fatal = [issue for issue in parsed.issues if issue.code != "duplicate_input"]
    if fatal or not parsed.entries:
        return 1

    store = CredentialStore(args.output)
    entries = parsed.entries
    if args.resume:
        existing = {record.email.casefold() for record in store.load()}
        entries = [entry for entry in entries if entry.account.casefold() not in existing]
    if not entries:
        print(json.dumps({"kind": "batch_finished", "skipped": True}))
        return 0

    def emit(event):
        print(json.dumps(event, ensure_ascii=False, separators=(",", ":")))

    transport = CurlCffiTransport(timeout=args.timeout)
    try:
        protocol = EnterpriseHttpClient(
            transport,
            vault=PasswordVault(args.password_vault),
            event_sink=emit,
        )
        summary = await process_entries(
            entries,
            LocalEnterpriseAuth(protocol),
            store,
            start_url=args.start_url,
            region=args.region,
            emit=emit,
        )
    finally:
        await transport.close()
    emit({"kind": "batch_finished", **summary})
    return 0 if summary["failed"] == 0 else 1


def main(argv=None) -> int:
    try:
        args = build_parser().parse_args(argv)
        return asyncio.run(async_main(args))
    except KeyboardInterrupt:
        return 130
    except Exception as error:
        print(
            f"企业批量登录启动失败：{redact_text(str(error))}",
            file=sys.stderr,
        )
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
