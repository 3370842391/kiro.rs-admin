from __future__ import annotations

import json
import os
import sqlite3
from collections.abc import Iterator, Sequence
from contextlib import closing, contextmanager
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path

from .models import AccountEntry, LoginMode
from .password_vault import SecretProtector, WindowsDpapiProtector


SCHEMA_VERSION = 1


class AccountRepositoryError(RuntimeError):
    pass


class LoginStatus(str, Enum):
    PENDING = "pending"
    RUNNING = "running"
    SUCCESS = "success"
    FAILED = "failed"


class CredentialStatus(str, Enum):
    MISSING = "missing"
    VALID = "valid"
    STALE = "stale"


class LifecycleStatus(str, Enum):
    MANAGED = "managed"
    SOLD = "sold"


@dataclass(frozen=True, slots=True)
class ManagedAccount:
    id: int
    login_mode: LoginMode
    account: str
    start_url: str | None
    region: str
    login_status: LoginStatus
    credential_status: CredentialStatus
    lifecycle_status: LifecycleStatus
    note: str
    initial_password: str | None = field(default=None, repr=False)
    current_password: str | None = field(default=None, repr=False)
    last_error_code: str | None = None
    last_error_stage: str | None = None
    last_login_at: str | None = None
    last_exported_at: str | None = None
    created_at: str = ""
    updated_at: str = ""


def default_account_db_path() -> Path:
    local_app_data = os.environ.get("LOCALAPPDATA")
    base = Path(local_app_data) if local_app_data else Path.home() / ".local" / "share"
    return base / "KiroBatchLogin" / "accounts.sqlite3"


class AccountRepository:
    def __init__(
        self,
        path: Path,
        *,
        protector: SecretProtector | None = None,
    ):
        self.path = Path(path)
        self.protector = protector or WindowsDpapiProtector()
        self._initialize()

    def upsert_entries(
        self,
        entries: Sequence[AccountEntry],
        *,
        login_mode: LoginMode,
        region: str,
    ) -> list[ManagedAccount]:
        prepared = []
        for item in entries:
            try:
                encrypted = self.protector.protect(item.password.encode("utf-8"))
            except Exception as error:
                raise AccountRepositoryError("账号密码加密失败") from error
            prepared.append(
                (
                    item,
                    self._normalize_account(item.account),
                    self._normalize_scope(item.start_url),
                    encrypted,
                )
            )
        saved_ids: list[int] = []
        now = self._utc_now()
        try:
            with self._transaction() as connection:
                for item, normalized_account, normalized_scope, encrypted in prepared:
                    connection.execute(
                        """
                        INSERT INTO accounts (
                            login_mode, account, normalized_account,
                            start_url, normalized_start_url, region,
                            initial_password_ciphertext, login_status,
                            credential_status, lifecycle_status,
                            note, created_at, updated_at
                        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '', ?, ?)
                        ON CONFLICT(login_mode, normalized_account, normalized_start_url)
                        DO UPDATE SET
                            account = excluded.account,
                            start_url = COALESCE(NULLIF(excluded.start_url, ''), accounts.start_url),
                            region = COALESCE(NULLIF(excluded.region, ''), accounts.region),
                            initial_password_ciphertext = excluded.initial_password_ciphertext,
                            updated_at = excluded.updated_at
                        """,
                        (
                            login_mode.value,
                            item.account.strip(),
                            normalized_account,
                            (item.start_url or "").strip() or None,
                            normalized_scope,
                            region.strip() or "us-east-1",
                            encrypted,
                            LoginStatus.PENDING.value,
                            CredentialStatus.MISSING.value,
                            LifecycleStatus.MANAGED.value,
                            now,
                            now,
                        ),
                    )
                    row = connection.execute(
                        """
                        SELECT id FROM accounts
                        WHERE login_mode = ? AND normalized_account = ?
                          AND normalized_start_url = ?
                        """,
                        (login_mode.value, normalized_account, normalized_scope),
                    ).fetchone()
                    account_id = int(row[0])
                    saved_ids.append(account_id)
                    self._append_history(
                        connection,
                        account_id,
                        "account_upserted",
                        {"mode": login_mode.value},
                        now,
                    )
        except AccountRepositoryError:
            raise
        except sqlite3.Error as error:
            raise AccountRepositoryError("账号批量保存失败") from error
        return [self.get(account_id) for account_id in saved_ids]

    def list_accounts(
        self,
        *,
        lifecycle_status: LifecycleStatus | None = None,
        include_secrets: bool = False,
    ) -> list[ManagedAccount]:
        sql = "SELECT * FROM accounts"
        parameters: tuple[object, ...] = ()
        if lifecycle_status is not None:
            sql += " WHERE lifecycle_status = ?"
            parameters = (lifecycle_status.value,)
        sql += " ORDER BY id"
        try:
            with closing(self._connect()) as connection:
                rows = connection.execute(sql, parameters).fetchall()
            return [self._from_row(row, include_secrets=include_secrets) for row in rows]
        except AccountRepositoryError:
            raise
        except sqlite3.Error as error:
            raise AccountRepositoryError("账号列表读取失败") from error

    def get(self, account_id: int, *, include_secrets: bool = False) -> ManagedAccount:
        try:
            with closing(self._connect()) as connection:
                row = connection.execute(
                    "SELECT * FROM accounts WHERE id = ?", (account_id,)
                ).fetchone()
            if row is None:
                raise AccountRepositoryError("账号不存在")
            return self._from_row(row, include_secrets=include_secrets)
        except AccountRepositoryError:
            raise
        except sqlite3.Error as error:
            raise AccountRepositoryError("账号读取失败") from error

    def update_current_passwords(
        self,
        account_ids: Sequence[int],
        password: str,
    ) -> int:
        if not password:
            raise AccountRepositoryError("当前密码不能为空")
        ids = self._unique_ids(account_ids)
        try:
            encrypted = self.protector.protect(password.encode("utf-8"))
        except Exception as error:
            raise AccountRepositoryError("当前密码加密失败") from error
        now = self._utc_now()
        try:
            with self._transaction() as connection:
                self._require_ids(connection, ids)
                for account_id in ids:
                    connection.execute(
                        """
                        UPDATE accounts
                        SET current_password_ciphertext = ?, credential_status = ?,
                            updated_at = ?
                        WHERE id = ?
                        """,
                        (encrypted, CredentialStatus.STALE.value, now, account_id),
                    )
                    self._append_history(
                        connection,
                        account_id,
                        "password_updated",
                        {},
                        now,
                    )
        except AccountRepositoryError:
            raise
        except sqlite3.Error as error:
            raise AccountRepositoryError("当前密码批量更新失败") from error
        return len(ids)

    def mark_sold(self, account_ids: Sequence[int], note: str) -> int:
        return self._update_lifecycle(
            account_ids,
            LifecycleStatus.SOLD,
            note=note,
            operation="marked_sold",
        )

    def restore_managed(self, account_ids: Sequence[int]) -> int:
        return self._update_lifecycle(
            account_ids,
            LifecycleStatus.MANAGED,
            note=None,
            operation="restored_managed",
        )

    def record_exported(
        self,
        account_ids: Sequence[int],
        *,
        note: str,
        mark_sold: bool,
    ) -> int:
        ids = self._unique_ids(account_ids)
        now = self._utc_now()
        try:
            with self._transaction() as connection:
                self._require_ids(connection, ids)
                for account_id in ids:
                    if mark_sold:
                        connection.execute(
                            """
                            UPDATE accounts
                            SET last_exported_at = ?, lifecycle_status = ?,
                                note = ?, updated_at = ?
                            WHERE id = ?
                            """,
                            (
                                now,
                                LifecycleStatus.SOLD.value,
                                note.strip(),
                                now,
                                account_id,
                            ),
                        )
                    else:
                        connection.execute(
                            """
                            UPDATE accounts
                            SET last_exported_at = ?, updated_at = ?
                            WHERE id = ?
                            """,
                            (now, now, account_id),
                        )
                    self._append_history(
                        connection,
                        account_id,
                        "account_exported",
                        {"markedSold": mark_sold},
                        now,
                    )
        except AccountRepositoryError:
            raise
        except sqlite3.Error as error:
            raise AccountRepositoryError("账号导出状态更新失败") from error
        return len(ids)

    def _update_lifecycle(
        self,
        account_ids: Sequence[int],
        status: LifecycleStatus,
        *,
        note: str | None,
        operation: str,
    ) -> int:
        ids = self._unique_ids(account_ids)
        now = self._utc_now()
        try:
            with self._transaction() as connection:
                self._require_ids(connection, ids)
                for account_id in ids:
                    if note is None:
                        connection.execute(
                            """
                            UPDATE accounts SET lifecycle_status = ?, updated_at = ?
                            WHERE id = ?
                            """,
                            (status.value, now, account_id),
                        )
                    else:
                        connection.execute(
                            """
                            UPDATE accounts
                            SET lifecycle_status = ?, note = ?, updated_at = ?
                            WHERE id = ?
                            """,
                            (status.value, note.strip(), now, account_id),
                        )
                    self._append_history(connection, account_id, operation, {}, now)
        except AccountRepositoryError:
            raise
        except sqlite3.Error as error:
            raise AccountRepositoryError("账号状态批量更新失败") from error
        return len(ids)

    def _from_row(self, row: sqlite3.Row, *, include_secrets: bool) -> ManagedAccount:
        initial = None
        current = None
        if include_secrets:
            try:
                if row["initial_password_ciphertext"] is not None:
                    initial = self.protector.unprotect(
                        bytes(row["initial_password_ciphertext"])
                    ).decode("utf-8")
                if row["current_password_ciphertext"] is not None:
                    current = self.protector.unprotect(
                        bytes(row["current_password_ciphertext"])
                    ).decode("utf-8")
            except Exception as error:
                raise AccountRepositoryError("账号密码解密失败") from error
        return ManagedAccount(
            id=int(row["id"]),
            login_mode=LoginMode(str(row["login_mode"])),
            account=str(row["account"]),
            start_url=row["start_url"],
            region=str(row["region"]),
            login_status=LoginStatus(str(row["login_status"])),
            credential_status=CredentialStatus(str(row["credential_status"])),
            lifecycle_status=LifecycleStatus(str(row["lifecycle_status"])),
            note=str(row["note"]),
            initial_password=initial,
            current_password=current,
            last_error_code=row["last_error_code"],
            last_error_stage=row["last_error_stage"],
            last_login_at=row["last_login_at"],
            last_exported_at=row["last_exported_at"],
            created_at=str(row["created_at"]),
            updated_at=str(row["updated_at"]),
        )

    def _initialize(self) -> None:
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            with closing(self._connect()) as connection:
                has_metadata = connection.execute(
                    """
                    SELECT 1 FROM sqlite_master
                    WHERE type = 'table' AND name = 'metadata'
                    """
                ).fetchone()
                if has_metadata:
                    row = connection.execute(
                        "SELECT value FROM metadata WHERE key = 'schema_version'"
                    ).fetchone()
                    if row is None or str(row[0]) != str(SCHEMA_VERSION):
                        raise AccountRepositoryError("账号数据库版本不受支持")
                    return
                connection.executescript(
                    """
                    CREATE TABLE metadata (
                        key TEXT PRIMARY KEY,
                        value TEXT NOT NULL
                    );
                    INSERT INTO metadata(key, value) VALUES ('schema_version', '1');
                    CREATE TABLE accounts (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        login_mode TEXT NOT NULL,
                        account TEXT NOT NULL,
                        normalized_account TEXT NOT NULL,
                        start_url TEXT,
                        normalized_start_url TEXT NOT NULL,
                        region TEXT NOT NULL,
                        initial_password_ciphertext BLOB,
                        current_password_ciphertext BLOB,
                        login_status TEXT NOT NULL,
                        credential_status TEXT NOT NULL,
                        lifecycle_status TEXT NOT NULL,
                        note TEXT NOT NULL DEFAULT '',
                        last_error_code TEXT,
                        last_error_stage TEXT,
                        last_login_at TEXT,
                        last_exported_at TEXT,
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL,
                        UNIQUE(login_mode, normalized_account, normalized_start_url)
                    );
                    CREATE TABLE operation_history (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        account_id INTEGER NOT NULL,
                        operation TEXT NOT NULL,
                        detail TEXT NOT NULL,
                        created_at TEXT NOT NULL,
                        FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
                    );
                    CREATE TABLE credentials (
                        account_id INTEGER PRIMARY KEY,
                        credential_ciphertext BLOB NOT NULL,
                        updated_at TEXT NOT NULL,
                        FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
                    );
                    """
                )
        except AccountRepositoryError:
            raise
        except (OSError, sqlite3.Error) as error:
            raise AccountRepositoryError("账号数据库初始化失败") from error

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
        connection = sqlite3.connect(self.path, timeout=5)
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA foreign_keys = ON")
        connection.execute("PRAGMA busy_timeout = 5000")
        return connection

    @staticmethod
    def _append_history(
        connection: sqlite3.Connection,
        account_id: int,
        operation: str,
        detail: dict[str, object],
        timestamp: str,
    ) -> None:
        connection.execute(
            """
            INSERT INTO operation_history(account_id, operation, detail, created_at)
            VALUES (?, ?, ?, ?)
            """,
            (account_id, operation, json.dumps(detail, ensure_ascii=False), timestamp),
        )

    @staticmethod
    def _require_ids(connection: sqlite3.Connection, ids: Sequence[int]) -> None:
        if not ids:
            raise AccountRepositoryError("必须选择至少一个账号")
        placeholders = ",".join("?" for _ in ids)
        count = connection.execute(
            f"SELECT COUNT(*) FROM accounts WHERE id IN ({placeholders})",
            tuple(ids),
        ).fetchone()[0]
        if int(count) != len(ids):
            raise AccountRepositoryError("部分账号不存在，批量操作已取消")

    @staticmethod
    def _unique_ids(account_ids: Sequence[int]) -> list[int]:
        return list(dict.fromkeys(int(item) for item in account_ids))

    @staticmethod
    def _normalize_account(account: str) -> str:
        return account.strip().casefold()

    @staticmethod
    def _normalize_scope(start_url: str | None) -> str:
        return (start_url or "").strip().rstrip("/").casefold()

    @staticmethod
    def _utc_now() -> str:
        return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
