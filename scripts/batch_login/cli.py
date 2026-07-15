from __future__ import annotations

import argparse
import asyncio
import os
import sys
from pathlib import Path
from urllib.parse import urlsplit
from uuid import uuid4

from playwright.async_api import async_playwright

from .browser_flows import BrowserFlows
from .checkpoint import CheckpointStore, exit_code_for
from .input_parser import parse_accounts
from .models import LoginMode
from .redaction import mask_account, redact_text
from .rs_client import RsClient
from .runner import BatchLoginRunner, RunnerSettings


DEFAULT_FORMAT = "{account}----{password}"
LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost"}


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Kiro RS 批量企业/Microsoft 自动登录",
    )
    subparsers = parser.add_subparsers(dest="mode", required=True)
    for name in ("enterprise", "microsoft"):
        command = subparsers.add_parser(name)
        command.add_argument("--input", required=True, help="账号文件路径，- 表示 stdin")
        command.add_argument("--format", default=DEFAULT_FORMAT, help="账号密码格式模板")
        command.add_argument("--rs-url", required=True, help="RS 地址或 SSH 本地转发地址")
        command.add_argument(
            "--admin-key-env",
            default="KIRO_RS_ADMIN_KEY",
            help="保存 RS Admin Key 的环境变量名",
        )
        command.add_argument("--region", default="us-east-1")
        command.add_argument("--timeout", type=float, default=180)
        command.add_argument("--mfa-timeout", type=float, default=300)
        command.add_argument("--result", type=Path, help="JSONL 结果/checkpoint 路径")
        command.add_argument("--resume", action="store_true", help="按 checkpoint 恢复")
        command.add_argument("--headless", action="store_true", help="无头运行浏览器")
    subparsers.choices["enterprise"].add_argument(
        "--start-url",
        required=True,
        help="AWS IAM Identity Center start URL",
    )
    return parser


def validate_args(args, environ=os.environ) -> str:
    key = environ.get(args.admin_key_env, "").strip()
    if not key:
        raise SystemExit(f"环境变量 {args.admin_key_env} 未设置")
    if args.timeout <= 0 or args.mfa_timeout <= 0:
        raise SystemExit("timeout 和 mfa-timeout 必须大于 0")
    if not args.region.strip():
        raise SystemExit("region 不能为空")

    try:
        parts = urlsplit(args.rs_url.strip())
    except ValueError as error:
        raise SystemExit("rs-url 无效") from error
    if parts.scheme == "http" and parts.hostname not in LOOPBACK_HOSTS:
        raise SystemExit("远程 RS 必须使用 HTTPS；HTTP 仅允许 SSH 本地转发")
    if parts.scheme not in {"http", "https"} or not parts.hostname:
        raise SystemExit("rs-url 必须是 HTTP(S) URL")

    if args.result is not None and args.input != "-":
        if Path(args.input).resolve() == args.result.resolve():
            raise SystemExit("result 不能覆盖账号输入文件")
    return key


def read_input(path: str) -> str:
    if path == "-":
        return sys.stdin.read()
    return Path(path).read_text(encoding="utf-8-sig")


async def async_main(args, admin_key: str) -> int:
    mode = LoginMode(args.mode)
    parsed = parse_accounts(read_input(args.input), args.format, mode)
    for issue in parsed.issues:
        print(
            f"第 {issue.line_number} 行 [{issue.code}] {issue.message}",
            file=sys.stderr,
        )
    fatal_issues = [
        issue for issue in parsed.issues if issue.code != "duplicate_input"
    ]
    if fatal_issues:
        return 1
    if not parsed.entries:
        print("没有可执行账号", file=sys.stderr)
        return 1

    print(f"将串行处理 {len(parsed.entries)} 个账号：")
    for item in parsed.entries:
        print(f"  第 {item.line_number} 行 {mask_account(item.account)}")

    run_id = uuid4().hex
    result_path = args.result or Path(f"batch-login-{run_id}.jsonl")
    checkpoint = CheckpointStore(result_path)
    settings = RunnerSettings(
        region=args.region,
        start_url=getattr(args, "start_url", None),
    )

    async with RsClient(args.rs_url, admin_key) as client:
        await client.preflight()
        async with async_playwright() as playwright:
            browser = await playwright.chromium.launch(headless=args.headless)
            try:
                browser_flows = BrowserFlows(
                    browser,
                    timeout_seconds=args.timeout,
                    mfa_timeout_seconds=args.mfa_timeout,
                )
                runner = BatchLoginRunner(client, browser_flows, checkpoint)
                outcomes = await runner.run_batch(
                    mode,
                    parsed.entries,
                    settings,
                    resume=args.resume,
                    run_id=run_id,
                )
            finally:
                await browser.close()
    return exit_code_for([outcome.status for outcome in outcomes])


def main(argv=None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        admin_key = validate_args(args)
        return asyncio.run(async_main(args, admin_key))
    except KeyboardInterrupt:
        return 130
    except Exception as error:
        print(
            f"批量登录启动失败：{redact_text(str(error))}",
            file=sys.stderr,
        )
        return 1
