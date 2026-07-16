from __future__ import annotations

import re
import tempfile
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlsplit

from .account_repository import (
    AccountRepository,
    CredentialStatus,
    LifecycleStatus,
)
from .credential_store import CredentialStore
from .gui_controller import GuiFormState
from .gui_runtime import GuiRuntime
from .gui_settings import GuiSavedSettings, GuiSettingsStore
from .models import AccountEntry, LoginMode
from .oidc_exporter import OidcCredentialExporter, OidcExportMode
from .password_vault import PasswordStatus, PasswordVault
from .worker_events import ResultMode


@dataclass(frozen=True, slots=True)
class LoginExportReport:
    selected: int
    reused: int
    logged_in: int
    failed: int
    exported: int


def form_from_saved_settings(
    saved: GuiSavedSettings,
    *,
    mode: LoginMode,
    credential_path: Path,
    checkpoint_path: Path,
) -> GuiFormState:
    def integer(value: str, default: int) -> int:
        return int(value) if value.strip() else default

    return GuiFormState(
        mode=mode,
        input_template=saved.input_template,
        output_template=saved.output_template,
        start_url=saved.start_url,
        password_vault_path=saved.password_vault_path or (
            saved.credential_path + ".passwords.sqlite3"
        ),
        region=saved.region,
        headless=saved.headless,
        timeout_seconds=saved.timeout_seconds,
        mfa_timeout_seconds=saved.mfa_timeout_seconds,
        result_mode=ResultMode(saved.result_mode),
        credential_path=str(credential_path),
        checkpoint_path=str(checkpoint_path),
        resume=False,
        rs_url=saved.rs_url,
        admin_key=saved.admin_key,
        use_ssh=saved.use_ssh,
        ssh_host=saved.ssh_host,
        ssh_user=saved.ssh_user,
        ssh_port=integer(saved.ssh_port, 22),
        identity_file=saved.identity_file,
        remote_host=saved.remote_host,
        remote_port=integer(saved.remote_port, 8990),
        local_port=integer(saved.local_port, 0) or None,
        oidc_export_mode=OidcExportMode(saved.oidc_export_mode),
        oidc_export_directory=saved.oidc_export_directory,
    )


class AccountLoginCoordinator:
    def __init__(
        self,
        repository: AccountRepository,
        settings_store: GuiSettingsStore,
        *,
        exporter=None,
        runtime_factory=GuiRuntime,
        emit=lambda _event: None,
    ):
        self.repository = repository
        self.settings_store = settings_store
        self.exporter = exporter or OidcCredentialExporter()
        self.runtime_factory = runtime_factory
        self.emit = emit

    def sync_saved_passwords(self, account_ids: list[int]) -> int:
        saved = self.settings_store.load()
        if saved is None:
            return 0
        vault_path = saved.password_vault_path
        if not vault_path and saved.credential_path:
            vault_path = saved.credential_path + ".passwords.sqlite3"
        if not vault_path:
            return 0
        accounts = [
            self.repository.get(int(item), include_secrets=False)
            for item in dict.fromkeys(account_ids)
        ]
        return self._sync_confirmed_passwords(Path(vault_path), accounts)

    async def run(
        self, account_ids: list[int], *, force_relogin: bool = False
    ) -> LoginExportReport:
        saved = self.settings_store.load()
        if saved is None:
            raise ValueError("请先打开自动登录设置并保存配置")
        ids = list(dict.fromkeys(int(item) for item in account_ids))
        if not ids:
            raise ValueError("请先选择账号")
        accounts = [
            self.repository.get(item, include_secrets=True) for item in ids
        ]
        if any(item.lifecycle_status is LifecycleStatus.SOLD for item in accounts):
            raise ValueError("已售出账号请先恢复管理")

        reusable = []
        pending = []
        for account in accounts:
            credential = self.repository.load_credential(account.id)
            if (
                not force_relogin
                and account.credential_status is CredentialStatus.VALID
                and credential is not None
            ):
                reusable.append((account, credential))
            else:
                pending.append(account)

        new_credentials = []
        failed = 0
        for mode in LoginMode:
            batch = [item for item in pending if item.login_mode is mode]
            if not batch:
                continue
            self.repository.mark_login_running([item.id for item in batch])
            with tempfile.TemporaryDirectory(prefix="kiro-login-") as tmp:
                root = Path(tmp)
                form = form_from_saved_settings(
                    saved,
                    mode=mode,
                    credential_path=root / "credentials.json",
                    checkpoint_path=root / "checkpoint.jsonl",
                )
                entries = [
                    AccountEntry(
                        index,
                        item.account,
                        item.current_password or item.initial_password or "",
                        item.start_url,
                    )
                    for index, item in enumerate(batch, start=1)
                ]
                runtime = self.runtime_factory(form, self.emit)
                runtime_error = None
                try:
                    await runtime.run(entries)
                except Exception as error:
                    runtime_error = error
                finally:
                    try:
                        await runtime.close()
                    except Exception as error:
                        if runtime_error is None:
                            runtime_error = error
                    try:
                        self._sync_confirmed_passwords(form, batch)
                    except Exception as error:
                        if runtime_error is None:
                            runtime_error = error
                if runtime_error is not None:
                    for item in batch:
                        self.repository.mark_login_failed(
                            item.id, "runtime_failed", "automatic_login"
                        )
                    failed += len(batch)
                    continue
                records = CredentialStore(Path(form.credential_path)).load()
                by_key = {
                    (record.email.casefold(), (record.start_url or "").rstrip("/").casefold()): record
                    for record in records
                }
                for item in batch:
                    key = (
                        item.account.casefold(),
                        (item.start_url or "").rstrip("/").casefold(),
                    )
                    credential = by_key.get(key)
                    if credential is None:
                        self.repository.mark_login_failed(
                            item.id, "login_failed", "automatic_login"
                        )
                        failed += 1
                        continue
                    self.repository.save_credential(item.id, credential)
                    new_credentials.append((item, credential))

        all_credentials = [item[1] for item in reusable + new_credentials]
        output_directory = Path(saved.oidc_export_directory) if saved.oidc_export_directory else Path(saved.credential_path).resolve().parent
        if all_credentials:
            self.exporter.export(
                all_credentials,
                output_directory=output_directory,
                mode=OidcExportMode(saved.oidc_export_mode),
            )
        return LoginExportReport(
            selected=len(accounts),
            reused=len(reusable),
            logged_in=len(new_credentials),
            failed=failed,
            exported=len(all_credentials),
        )

    def _sync_confirmed_passwords(self, form_or_path, accounts) -> int:
        path = Path(
            getattr(form_or_path, "password_vault_path", form_or_path)
        )
        if not path.exists():
            return 0
        records = PasswordVault(path).records()
        confirmed: dict[str, dict[str, str]] = {}
        for item in records:
            if item.status is not PasswordStatus.CONFIRMED:
                continue
            confirmed.setdefault(item.account.casefold(), {})[
                item.scope.strip().casefold()
            ] = item.password
        synced = 0
        for account in accounts:
            candidates = confirmed.get(account.account.casefold(), {})
            expected_scope = self._expected_password_scope(account)
            password = candidates.get(expected_scope) if expected_scope else None
            if password is None and len(candidates) == 1:
                password = next(iter(candidates.values()))
            if password:
                self.repository.sync_confirmed_passwords(
                    [account.id], password
                )
                synced += 1
        return synced

    @staticmethod
    def _expected_password_scope(account) -> str | None:
        try:
            hostname = (urlsplit(account.start_url or "").hostname or "").lower()
        except ValueError:
            return None
        match = re.fullmatch(r"(d-[a-z0-9]+)\.awsapps\.com", hostname)
        if match is None:
            return None
        return f"{account.region.strip().casefold()}/{match.group(1)}"
