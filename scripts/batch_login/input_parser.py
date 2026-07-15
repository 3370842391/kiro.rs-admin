from __future__ import annotations

import re
from dataclasses import dataclass

from .models import AccountEntry, LoginMode, ParseIssue, ParseResult


EMAIL_RE = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")


@dataclass(frozen=True, slots=True)
class CompiledFormat:
    separator: str
    account_first: bool


def compile_format(template: str) -> CompiledFormat:
    if template.count("{account}") != 1 or template.count("{password}") != 1:
        raise ValueError("格式模板必须恰好包含一次 {account} 和一次 {password}")

    account_index = template.index("{account}")
    password_index = template.index("{password}")
    if account_index < password_index:
        separator = template[account_index + len("{account}") : password_index]
        prefix = template[:account_index]
        suffix = template[password_index + len("{password}") :]
        account_first = True
    else:
        separator = template[password_index + len("{password}") : account_index]
        prefix = template[:password_index]
        suffix = template[account_index + len("{account}") :]
        account_first = False

    if prefix or suffix or not separator or "{" in separator or "}" in separator:
        raise ValueError("格式模板只允许两个占位符和一个非空字面分隔符")

    return CompiledFormat(separator=separator, account_first=account_first)


def parse_accounts(text: str, template: str, mode: LoginMode) -> ParseResult:
    compiled = compile_format(template)
    entries: list[AccountEntry] = []
    issues: list[ParseIssue] = []
    seen: set[str] = set()

    for line_number, raw_line in enumerate(text.splitlines(), start=1):
        line = raw_line.lstrip("\ufeff") if line_number == 1 else raw_line
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if compiled.separator not in line:
            issues.append(ParseIssue(line_number, "format_mismatch", "缺少格式分隔符"))
            continue

        if compiled.account_first:
            account, password = line.split(compiled.separator, 1)
        else:
            password, account = line.rsplit(compiled.separator, 1)

        account = account.strip()
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
