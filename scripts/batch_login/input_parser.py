from __future__ import annotations

import re
from ipaddress import ip_address
from collections.abc import Iterable
from dataclasses import dataclass
from urllib.parse import urlsplit

from .models import AccountEntry, LoginMode, ParseIssue, ParseResult


EMAIL_RE = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")
HOST_LABEL_RE = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?$")
PLACEHOLDER_RE = re.compile(r"{(?:account|password|start_url)}")
PLACEHOLDER_TOKEN_RE = re.compile(r"{(?P<name>account|password|start_url)}")
AUTO_START_URL_FORMAT = "{account}----{password}----{start_url}"


@dataclass(frozen=True, slots=True)
class CompiledFormat:
    pattern: re.Pattern[str]


def is_valid_start_url(value: str) -> bool:
    if any(
        character.isspace()
        or ord(character) < 32
        or ord(character) == 127
        for character in value
    ):
        return False
    try:
        parts = urlsplit(value)
        port = parts.port
        hostname = parts.hostname or ""
        try:
            ip_address(hostname)
            valid_hostname = True
        except ValueError:
            ascii_hostname = hostname.encode("idna").decode("ascii")
            labels = ascii_hostname.split(".")
            valid_hostname = (
                len(ascii_hostname) <= 253
                and all(HOST_LABEL_RE.fullmatch(label) for label in labels)
            )
        return (
            parts.scheme == "https"
            and valid_hostname
            and parts.username is None
            and parts.password is None
            and port != 0
        )
    except (UnicodeError, ValueError):
        return False


def compile_format(template: str) -> CompiledFormat:
    if template.count("{account}") != 1 or template.count("{password}") != 1:
        raise ValueError("格式模板必须恰好包含一次 {account} 和一次 {password}")
    if template.count("{start_url}") > 1:
        raise ValueError("格式模板最多包含一次 {start_url}")

    matches = list(PLACEHOLDER_TOKEN_RE.finditer(template))
    expression_parts: list[str] = []
    cursor = 0
    for match in matches:
        expression_parts.append(re.escape(template[cursor : match.start()]))
        name = match.group("name")
        # Password is greedy so a separator character inside the password is
        # preserved and the final delimiter before the next field is used.
        quantifier = ".*" if name == "password" else ".*?"
        expression_parts.append(f"(?P<{name}>{quantifier})")
        cursor = match.end()
    expression_parts.append(re.escape(template[cursor:]))
    expression = "".join(expression_parts)

    return CompiledFormat(pattern=re.compile(r"\A" + expression + r"\Z"))


def parse_accounts(
    text: str,
    template: str,
    mode: LoginMode,
    *,
    auto_detect_start_url: bool = False,
) -> ParseResult:
    compiled = compile_format(template)
    auto_compiled = (
        compile_format(AUTO_START_URL_FORMAT)
        if auto_detect_start_url and "{start_url}" not in template
        else None
    )
    entries: list[AccountEntry] = []
    issues: list[ParseIssue] = []
    seen: set[str] = set()

    for line_number, raw_line in enumerate(text.splitlines(), start=1):
        line = raw_line.lstrip("\ufeff") if line_number == 1 else raw_line
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        match = auto_compiled.pattern.fullmatch(line) if auto_compiled else None
        if match is None:
            match = compiled.pattern.fullmatch(line)
        if match is None:
            issues.append(ParseIssue(line_number, "format_mismatch", "缺少格式分隔符"))
            continue

        account = match.group("account").strip()
        password = match.group("password")
        start_url = match.groupdict().get("start_url")
        if start_url is not None:
            start_url = start_url.strip() or None
        if not account:
            issues.append(ParseIssue(line_number, "empty_account", "账号为空"))
            continue
        if password == "":
            issues.append(ParseIssue(line_number, "empty_password", "密码为空"))
            continue
        if start_url is not None:
            if not is_valid_start_url(start_url):
                issues.append(
                    ParseIssue(line_number, "invalid_start_url", "企业门户 URL 必须是 HTTPS")
                )
                continue
        if mode is LoginMode.MICROSOFT and not EMAIL_RE.fullmatch(account):
            issues.append(ParseIssue(line_number, "invalid_account", "Microsoft 模式要求邮箱账号"))
            continue

        key = account.casefold()
        if key in seen:
            issues.append(ParseIssue(line_number, "duplicate_input", "输入中账号重复"))
            continue
        seen.add(key)
        entries.append(
            AccountEntry(
                line_number=line_number,
                account=account,
                password=password,
                start_url=start_url,
            )
        )

    return ParseResult(entries=entries, issues=issues)


def render_accounts(entries: Iterable[AccountEntry], template: str) -> str:
    compile_format(template)

    def render_entry(entry: AccountEntry) -> str:
        values = {
            "{account}": entry.account,
            "{password}": entry.password,
            "{start_url}": entry.start_url or "",
        }
        return PLACEHOLDER_RE.sub(lambda match: values[match.group()], template)

    return "\n".join(render_entry(entry) for entry in entries)
