from __future__ import annotations

from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass

from .account_repository import (
    AccountRepository,
    AccountRepositoryError,
    CredentialStatus,
    LifecycleStatus,
    LoginStatus,
    ManagedAccount,
)
from .input_parser import compile_format, parse_accounts, render_accounts
from .models import AccountEntry, LoginMode, ParseIssue


class AccountManagerServiceError(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class ImportPreview:
    entries: list[AccountEntry]
    issues: list[ParseIssue]
    login_mode: LoginMode


@dataclass(frozen=True, slots=True)
class ImportReport:
    saved: int
    accounts: tuple[ManagedAccount, ...]


@dataclass(frozen=True, slots=True)
class TextExportReport:
    exported: int
    marked_sold: bool


class AccountManagerService:
    def __init__(self, repository: AccountRepository):
        self.repository = repository
        self._selected_ids: set[int] = set()

    @property
    def selected_ids(self) -> set[int]:
        return set(self._selected_ids)

    def preview_import(
        self,
        text: str,
        template: str,
        mode: LoginMode,
    ) -> ImportPreview:
        result = parse_accounts(text, template, mode)
        return ImportPreview(result.entries, result.issues, mode)

    def confirm_import(
        self,
        preview: ImportPreview,
        *,
        region: str = "us-east-1",
    ) -> ImportReport:
        if not isinstance(preview, ImportPreview):
            raise AccountManagerServiceError("导入预览已失效，请重新解析")
        if not preview.entries:
            raise AccountManagerServiceError("没有可保存的有效账号")
        try:
            accounts = self.repository.upsert_entries(
                preview.entries,
                login_mode=preview.login_mode,
                region=region,
            )
        except AccountRepositoryError as error:
            raise AccountManagerServiceError(str(error)) from error
        return ImportReport(len(accounts), tuple(accounts))

    def list_accounts(
        self,
        *,
        query: str = "",
        status: str = "managed",
    ) -> list[ManagedAccount]:
        accounts = self.repository.list_accounts()
        normalized_query = query.strip().casefold()
        if normalized_query:
            accounts = [
                item
                for item in accounts
                if normalized_query in item.account.casefold()
                or normalized_query in (item.start_url or "").casefold()
                or normalized_query in item.note.casefold()
            ]
        filters = {
            "all": lambda _item: True,
            "managed": lambda item: item.lifecycle_status is LifecycleStatus.MANAGED,
            "sold": lambda item: item.lifecycle_status is LifecycleStatus.SOLD,
            "pending": lambda item: (
                item.lifecycle_status is LifecycleStatus.MANAGED
                and item.login_status is LoginStatus.PENDING
            ),
            "exportable": lambda item: (
                item.lifecycle_status is LifecycleStatus.MANAGED
                and item.credential_status is CredentialStatus.VALID
            ),
            "failed": lambda item: (
                item.lifecycle_status is LifecycleStatus.MANAGED
                and item.login_status is LoginStatus.FAILED
            ),
        }
        predicate = filters.get(status)
        if predicate is None:
            raise AccountManagerServiceError("账号状态筛选无效")
        return [item for item in accounts if predicate(item)]

    def set_selected(self, ids: Iterable[int]) -> None:
        self._selected_ids = {int(item) for item in ids}

    def toggle_selected(self, account_id: int) -> None:
        account_id = int(account_id)
        if account_id in self._selected_ids:
            self._selected_ids.remove(account_id)
        else:
            self._selected_ids.add(account_id)

    def select_visible(self, visible_ids: Iterable[int]) -> None:
        self._selected_ids.update(int(item) for item in visible_ids)

    def invert_visible(self, visible_ids: Iterable[int]) -> None:
        for account_id in (int(item) for item in visible_ids):
            self.toggle_selected(account_id)

    def clear_selected(self) -> None:
        self._selected_ids.clear()

    def update_password(self, ids: Sequence[int], password: str) -> int:
        accounts = self._load_managed(ids, include_secrets=False)
        if not password:
            raise AccountManagerServiceError("当前密码不能为空")
        try:
            return self.repository.update_current_passwords(
                [item.id for item in accounts], password
            )
        except AccountRepositoryError as error:
            raise AccountManagerServiceError(str(error)) from error

    def mark_sold(self, ids: Sequence[int], note: str) -> int:
        accounts = self._load_managed(ids, include_secrets=False)
        try:
            return self.repository.mark_sold(
                [item.id for item in accounts], note
            )
        except AccountRepositoryError as error:
            raise AccountManagerServiceError(str(error)) from error

    def restore_managed(self, ids: Sequence[int]) -> int:
        unique_ids = list(dict.fromkeys(int(item) for item in ids))
        if not unique_ids:
            raise AccountManagerServiceError("必须选择至少一个账号")
        try:
            return self.repository.restore_managed(unique_ids)
        except AccountRepositoryError as error:
            raise AccountManagerServiceError(str(error)) from error

    def render_text(self, ids: Sequence[int], template: str) -> str:
        self._validate_export_template(template)
        accounts = self._load_managed(ids, include_secrets=True)
        missing = [item.account for item in accounts if not item.current_password]
        if missing:
            raise AccountManagerServiceError(
                f"{len(missing)} 个账号缺少当前密码，无法导出"
            )
        entries = [
            AccountEntry(
                line_number=index,
                account=item.account,
                password=item.current_password or "",
                start_url=item.start_url,
            )
            for index, item in enumerate(accounts, start=1)
        ]
        return render_accounts(entries, template)

    def export_text(
        self,
        ids: Sequence[int],
        *,
        template: str,
        writer: Callable[[str], object],
        note: str,
        mark_sold: bool,
    ) -> TextExportReport:
        accounts = self._load_managed(ids, include_secrets=False)
        text = self.render_text(ids, template)
        try:
            writer(text)
        except Exception as error:
            raise AccountManagerServiceError("账号文本写出失败，状态未改变") from error
        try:
            self.repository.record_exported(
                [item.id for item in accounts],
                note=note,
                mark_sold=mark_sold,
            )
        except AccountRepositoryError as error:
            raise AccountManagerServiceError(str(error)) from error
        return TextExportReport(len(accounts), mark_sold)

    def _load_managed(
        self,
        ids: Sequence[int],
        *,
        include_secrets: bool,
    ) -> list[ManagedAccount]:
        unique_ids = list(dict.fromkeys(int(item) for item in ids))
        if not unique_ids:
            raise AccountManagerServiceError("必须选择至少一个账号")
        try:
            accounts = [
                self.repository.get(item, include_secrets=include_secrets)
                for item in unique_ids
            ]
        except AccountRepositoryError as error:
            raise AccountManagerServiceError(str(error)) from error
        if any(
            item.lifecycle_status is LifecycleStatus.SOLD for item in accounts
        ):
            raise AccountManagerServiceError("已售出账号请先恢复管理")
        return accounts

    @staticmethod
    def _validate_export_template(template: str) -> None:
        try:
            compile_format(template)
        except ValueError as error:
            raise AccountManagerServiceError(str(error)) from error
        if template.count("{start_url}") != 1:
            raise AccountManagerServiceError(
                "账号文本模板必须包含一次 {start_url}"
            )
