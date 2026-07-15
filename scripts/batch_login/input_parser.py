from __future__ import annotations

import re
from collections.abc import Iterable
from dataclasses import dataclass

from .models import AccountEntry, LoginMode, ParseIssue, ParseResult


EMAIL_RE = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")
PLACEHOLDER_RE = re.compile(r"{(?:account|password)}")


@dataclass(frozen=True, slots=True)
class CompiledFormat:
    pattern: re.Pattern[str]


def compile_format(template: str) -> CompiledFormat:
    if template.count("{account}") != 1 or template.count("{password}") != 1:
        raise ValueError("格式模板必须恰好包含一次 {account} 和一次 {password}")

    account_index = template.index("{account}")
    password_index = template.index("{password}")
    if account_index < password_index:
        prefix = template[:account_index]
        separator = template[account_index + len("{account}") : password_index]
        suffix = template[password_index + len("{password}") :]
        expression = (
            re.escape(prefix)
            + r"(?P<account>.*?)"
            + re.escape(separator)
            + r"(?P<password>.*)"
            + re.escape(suffix)
        )
    else:
        prefix = template[:password_index]
        separator = template[password_index + len("{password}") : account_index]
        suffix = template[account_index + len("{account}") :]
        expression = (
            re.escape(prefix)
            + r"(?P<password>.*)"
            + re.escape(separator)
            + r"(?P<account>.*?)"
            + re.escape(suffix)
        )

    return CompiledFormat(pattern=re.compile(r"\A" + expression + r"\Z"))


def parse_accounts(text: str, template: str, mode: LoginMode) -> ParseResult:
    compiled = compile_format(template)
    entries: list[AccountEntry] = []
    issues: list[ParseIssue] = []
    seen: set[str] = set()

    for line_number, raw_line in enumerate(text.splitlines(), start=1):
        line = raw_line.lstrip("\ufeff") if line_number == 1 else raw_line
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        match = compiled.pattern.fullmatch(line)
        if match is None:
            issues.append(ParseIssue(line_number, "format_mismatch", "缺少格式分隔符"))
            continue

        account = match.group("account").strip()
        password = match.group("password")
        if not account:
            issues.append(ParseIssue(line_number, "empty_account", "账号为空"))
            continue
        if password == "":
            issues.append(ParseIssue(line_number, "empty_password", "密码为空"))
            continue
        if mode is LoginMode.MICROSOFT and not EMAIL_RE.fullmatch(account):
            issues.append(ParseIssue(line_number, "invalid_account", "Microsoft 模式要求邮箱账号"))
            continue

        key = account.casefold()
        if key in seen:
            issues.append(ParseIssue(line_number, "duplicate_input", "输入中账号重复"))
            continue
        seen.add(key)
        entries.append(AccountEntry(line_number=line_number, account=account, password=password))

    return ParseResult(entries=entries, issues=issues)


def render_accounts(entries: Iterable[AccountEntry], template: str) -> str:
    compile_format(template)

    def render_entry(entry: AccountEntry) -> str:
        return PLACEHOLDER_RE.sub(
            lambda match: entry.account if match.group() == "{account}" else entry.password,
            template,
        )

    return "\n".join(render_entry(entry) for entry in entries)
