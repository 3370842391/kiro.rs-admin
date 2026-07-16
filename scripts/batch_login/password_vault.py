from __future__ import annotations

import ctypes
import hashlib
import os
import secrets
import sqlite3
import string
from collections.abc import Iterator
from contextlib import closing, contextmanager
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Protocol
from uuid import uuid4


class PasswordVaultError(RuntimeError):
    """A password could not be durably prepared or recovered."""


class StateTransitionError(PasswordVaultError):
    """A requested password-change state transition is not allowed."""


class PasswordStatus(str, Enum):
    PREPARED = "prepared"
    CONFIRMED = "confirmed"
    REJECTED = "rejected"
    UNCERTAIN = "uncertain"


class SecretProtector(Protocol):
    def protect(self, value: bytes) -> bytes: ...

    def unprotect(self, value: bytes) -> bytes: ...


class _DataBlob(ctypes.Structure):
    _fields_ = [
        ("cbData", ctypes.c_ulong),
        ("pbData", ctypes.POINTER(ctypes.c_ubyte)),
    ]


class WindowsDpapiProtector:
    """Protect values with Windows DPAPI scoped to the current user."""

    _CRYPTPROTECT_UI_FORBIDDEN = 0x1

    def __init__(self) -> None:
        if os.name != "nt":
            raise PasswordVaultError("DPAPI 仅支持 Windows 当前用户")
        try:
            self._crypt32 = ctypes.WinDLL("crypt32", use_last_error=True)
            self._kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        except OSError as error:
            raise PasswordVaultError("无法初始化 Windows DPAPI") from error

    @staticmethod
    def _input_blob(value: bytes) -> tuple[_DataBlob, ctypes.Array[ctypes.c_char]]:
        buffer = ctypes.create_string_buffer(value, len(value))
        blob = _DataBlob(
            len(value),
            ctypes.cast(buffer, ctypes.POINTER(ctypes.c_ubyte)),
        )
        return blob, buffer

    def protect(self, value: bytes) -> bytes:
        source, _buffer = self._input_blob(value)
        target = _DataBlob()
        if not self._crypt32.CryptProtectData(
            ctypes.byref(source),
            None,
            None,
            None,
            None,
            self._CRYPTPROTECT_UI_FORBIDDEN,
            ctypes.byref(target),
        ):
            raise PasswordVaultError("DPAPI 加密失败")
        return self._copy_and_free(target)

    def unprotect(self, value: bytes) -> bytes:
        source, _buffer = self._input_blob(value)
        target = _DataBlob()
        if not self._crypt32.CryptUnprotectData(
            ctypes.byref(source),
            None,
            None,
            None,
            None,
            self._CRYPTPROTECT_UI_FORBIDDEN,
            ctypes.byref(target),
        ):
            raise PasswordVaultError("DPAPI 解密失败")
        return self._copy_and_free(target)

    def _copy_and_free(self, blob: _DataBlob) -> bytes:
        try:
            return ctypes.string_at(blob.pbData, blob.cbData)
        finally:
            if blob.pbData:
                self._kernel32.LocalFree(blob.pbData)


@dataclass(frozen=True, slots=True)
class PreparedPassword:
    record_id: str
    status: PasswordStatus
    account: str = field(repr=False)
    password: str = field(repr=False)
    scope: str = field(default="", repr=False)


_UPPERCASE = string.ascii_uppercase
_LOWERCASE = string.ascii_lowercase
_DIGITS = string.digits
_SPECIAL = "!@#$%^&*()-_=+[]{}:,.?"


def generate_strong_password(length: int = 24) -> str:
    if length < 20:
        raise ValueError("password length must be at least 20")
    characters = [
        secrets.choice(_UPPERCASE),
        secrets.choice(_LOWERCASE),
        secrets.choice(_DIGITS),
        secrets.choice(_SPECIAL),
    ]
    alphabet = _UPPERCASE + _LOWERCASE + _DIGITS + _SPECIAL
    characters.extend(secrets.choice(alphabet) for _ in range(length - len(characters)))
    for index in range(len(characters) - 1, 0, -1):
        swap_index = secrets.randbelow(index + 1)
        characters[index], characters[swap_index] = (
            characters[swap_index],
            characters[index],
        )
    return "".join(characters)


_ALLOWED_TRANSITIONS = {
    PasswordStatus.PREPARED: frozenset(
        {PasswordStatus.CONFIRMED, PasswordStatus.REJECTED, PasswordStatus.UNCERTAIN}
    ),
    PasswordStatus.UNCERTAIN: frozenset(
        {PasswordStatus.CONFIRMED, PasswordStatus.REJECTED}
    ),
    PasswordStatus.CONFIRMED: frozenset(),
    PasswordStatus.REJECTED: frozenset(),
}


class PasswordVault:
    def __init__(
        self,
        path: Path,
        *,
        protector: SecretProtector | None = None,
        password_length: int = 24,
    ) -> None:
        self.path = Path(path)
        self._protector = protector or WindowsDpapiProtector()
        self._password_length = password_length
        if password_length < 20:
            raise ValueError("password length must be at least 20")
        self._initialize()

    def prepare(self, account: str, *, scope: str = "") -> PreparedPassword:
        normalized = self._normalize_account(account)
        normalized_scope = self._normalize_scope(scope)
        account_key = self._account_key(normalized, normalized_scope)
        new_record_id: str | None = None
        expected_password: str | None = None
        try:
            with self._transaction() as connection:
                row = connection.execute(
                    """
                    SELECT record_id
                    FROM password_changes
                    WHERE account_key = ? AND status IN ('prepared', 'uncertain')
                    """,
                    (account_key,),
                ).fetchone()
                if row is not None:
                    record_id = str(row[0])
                else:
                    record_id = uuid4().hex
                    password = generate_strong_password(self._password_length)
                    account_cipher = self._protector.protect(
                        (normalized_scope + "\0" + account).encode("utf-8")
                    )
                    password_cipher = self._protector.protect(password.encode("utf-8"))
                    now = self._utc_now()
                    connection.execute(
                        """
                        INSERT INTO password_changes (
                            record_id, account_key, account_cipher, password_cipher,
                            status, created_at, updated_at
                        ) VALUES (?, ?, ?, ?, 'prepared', ?, ?)
                        """,
                        (
                            record_id,
                            account_key,
                            account_cipher,
                            password_cipher,
                            now,
                            now,
                        ),
                    )
                    new_record_id = record_id
                    expected_password = password

            prepared = self._read_verified(record_id, expected_account_key=account_key)
            if new_record_id is not None and (
                prepared.record_id != new_record_id or prepared.password != expected_password
            ):
                raise PasswordVaultError("密码保存后的校验失败")
            return prepared
        except (PasswordVaultError, StateTransitionError):
            raise
        except Exception as error:
            raise PasswordVaultError("无法安全保存或读取新密码") from error

    def transition(
        self,
        record_id: str,
        target: PasswordStatus,
    ) -> PreparedPassword:
        try:
            target = PasswordStatus(target)
        except (TypeError, ValueError) as error:
            raise StateTransitionError("未知的密码状态") from error
        try:
            with self._transaction() as connection:
                row = connection.execute(
                    "SELECT status FROM password_changes WHERE record_id = ?",
                    (record_id,),
                ).fetchone()
                if row is None:
                    raise PasswordVaultError("密码记录不存在")
                current = PasswordStatus(str(row[0]))
                if target not in _ALLOWED_TRANSITIONS[current]:
                    raise StateTransitionError("不允许的密码状态转换")
                cursor = connection.execute(
                    """
                    UPDATE password_changes
                    SET status = ?, updated_at = ?
                    WHERE record_id = ? AND status = ?
                    """,
                    (target.value, self._utc_now(), record_id, current.value),
                )
                if cursor.rowcount != 1:
                    raise StateTransitionError("密码状态已被其他任务更新")
            return self._read_verified(record_id)
        except (PasswordVaultError, StateTransitionError):
            raise
        except Exception as error:
            raise PasswordVaultError("无法更新密码状态") from error

    def records(self) -> list[PreparedPassword]:
        """Decrypt records only when the user explicitly requests recovery."""
        try:
            with closing(self._connect()) as connection:
                rows = connection.execute(
                    "SELECT record_id FROM password_changes ORDER BY created_at, record_id"
                ).fetchall()
            return [self._read_verified(str(row[0])) for row in rows]
        except PasswordVaultError:
            raise
        except Exception as error:
            raise PasswordVaultError("无法读取密码恢复记录") from error

    def unresolved(self, account: str, *, scope: str = "") -> PreparedPassword | None:
        """Return an existing prepared/uncertain candidate without creating one."""
        normalized = self._normalize_account(account)
        account_key = self._account_key(
            normalized, self._normalize_scope(scope)
        )
        try:
            with closing(self._connect()) as connection:
                row = connection.execute(
                    """
                    SELECT record_id FROM password_changes
                    WHERE account_key = ? AND status IN ('prepared', 'uncertain')
                    """,
                    (account_key,),
                ).fetchone()
            if row is None:
                return None
            return self._read_verified(
                str(row[0]), expected_account_key=account_key
            )
        except PasswordVaultError:
            raise
        except Exception as error:
            raise PasswordVaultError("无法读取未决密码记录") from error

    def _initialize(self) -> None:
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            with closing(self._connect()) as connection:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute(
                    """
                    CREATE TABLE IF NOT EXISTS password_changes (
                        record_id TEXT PRIMARY KEY,
                        account_key TEXT NOT NULL,
                        account_cipher BLOB NOT NULL,
                        password_cipher BLOB NOT NULL,
                        status TEXT NOT NULL CHECK (
                            status IN ('prepared', 'confirmed', 'rejected', 'uncertain')
                        ),
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL
                    )
                    """
                )
                connection.execute(
                    """
                    CREATE UNIQUE INDEX IF NOT EXISTS password_changes_one_unresolved
                    ON password_changes(account_key)
                    WHERE status IN ('prepared', 'uncertain')
                    """
                )
                connection.commit()
        except Exception as error:
            raise PasswordVaultError("无法初始化密码保险库") from error

    @contextmanager
    def _transaction(self) -> Iterator[sqlite3.Connection]:
        connection = self._connect()
        try:
            connection.execute("BEGIN IMMEDIATE")
            yield connection
            connection.commit()
        except Exception:
            connection.rollback()
            raise
        finally:
            connection.close()

    def _connect(self) -> sqlite3.Connection:
        connection = sqlite3.connect(self.path, timeout=30.0)
        connection.execute("PRAGMA busy_timeout=30000")
        connection.execute("PRAGMA synchronous=FULL")
        return connection

    def _read_verified(
        self,
        record_id: str,
        *,
        expected_account_key: str | None = None,
    ) -> PreparedPassword:
        try:
            with closing(self._connect()) as connection:
                row = connection.execute(
                    """
                    SELECT account_key, account_cipher, password_cipher, status
                    FROM password_changes
                    WHERE record_id = ?
                    """,
                    (record_id,),
                ).fetchone()
            if row is None:
                raise PasswordVaultError("密码记录不存在")
            identity = self._protector.unprotect(bytes(row[1])).decode("utf-8")
            normalized_scope, separator, account = identity.partition("\0")
            if not separator:
                raise PasswordVaultError("密码记录完整性校验失败")
            password = self._protector.unprotect(bytes(row[2])).decode("utf-8")
            stored_key = str(row[0])
            if (
                self._account_key(self._normalize_account(account), normalized_scope)
                != stored_key
            ):
                raise PasswordVaultError("密码记录完整性校验失败")
            if expected_account_key is not None and stored_key != expected_account_key:
                raise PasswordVaultError("密码记录账号校验失败")
            if not password:
                raise PasswordVaultError("密码记录完整性校验失败")
            return PreparedPassword(
                record_id=record_id,
                account=account,
                password=password,
                status=PasswordStatus(str(row[3])),
                scope=normalized_scope,
            )
        except PasswordVaultError:
            raise
        except Exception as error:
            raise PasswordVaultError("密码保存后的解密校验失败") from error

    @staticmethod
    def _normalize_account(account: str) -> str:
        if not isinstance(account, str) or not account.strip():
            raise PasswordVaultError("账号不能为空")
        return account.strip().casefold()

    @staticmethod
    def _account_key(normalized_account: str, normalized_scope: str = "") -> str:
        return hashlib.sha256(
            (normalized_scope + "\0" + normalized_account).encode("utf-8")
        ).hexdigest()

    @staticmethod
    def _normalize_scope(scope: str) -> str:
        if not isinstance(scope, str):
            raise PasswordVaultError("密码保险库范围无效")
        return scope.strip().casefold()

    @staticmethod
    def _utc_now() -> str:
        return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
