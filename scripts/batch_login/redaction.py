from __future__ import annotations

import re
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit


SENSITIVE_QUERY_KEYS = {
    "code",
    "state",
    "access_token",
    "refresh_token",
    "id_token",
    "client_secret",
    "code_verifier",
    "password",
}
BEARER_RE = re.compile(r"(?i)\bBearer\s+[^\s,;]+")
TOKEN_ASSIGNMENT_RE = re.compile(
    r"(?i)(?P<quote>[\"']?)\b"
    r"(?P<key>access_?token|refresh_?token|id_?token|client_?secret|code_?verifier|token|password)"
    r"\b(?P=quote)\s*[:=]\s*(?:\"[^\"]*\"|'[^']*'|[^\s,;]+)"
)
URL_RE = re.compile(r"https?://[^\s<>\"']+", re.I)
EMAIL_RE = re.compile(r"[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}", re.I)


def mask_account(account: str) -> str:
    if "@" in account:
        local, domain = account.split("@", 1)
        return f"{local[:2]}***@{domain}" if local else f"***@{domain}"
    return f"{account[:2]}***" if account else "***"


def redact_url(raw_url: str) -> str:
    parts = urlsplit(raw_url)
    query = [
        (
            key,
            "<redacted>"
            if key.casefold() in SENSITIVE_QUERY_KEYS
            else EMAIL_RE.sub(lambda match: mask_account(match.group(0)), value),
        )
        for key, value in parse_qsl(parts.query, keep_blank_values=True)
    ]
    return urlunsplit((parts.scheme, parts.netloc, parts.path, urlencode(query), ""))


def redact_text(text: str) -> str:
    text = EMAIL_RE.sub(lambda match: mask_account(match.group(0)), text)
    text = URL_RE.sub(lambda match: redact_url(match.group(0)), text)
    text = BEARER_RE.sub("Bearer <redacted>", text)
    return TOKEN_ASSIGNMENT_RE.sub(
        lambda match: f"{match.group('key')}=<redacted>",
        text,
    )
