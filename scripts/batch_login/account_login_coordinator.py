from __future__ import annotations

import asyncio
import re
import tempfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlsplit

from .account_repository import (
    AccountRepository,
    CredentialStatus,
    LifecycleStatus,
)
from .api_key_client import ApiKeyError, ensure_api_key, resolve_profile_arn
from .api_key_refresh import refresh_access_token
from .usage_client import UsageError, get_usage_limits
from .credential_models import CredentialRecord
from .credential_store import CredentialStore
from .gui_controller import GuiFormState
from .gui_runtime import GuiRuntime
from .gui_settings import GuiSavedSettings, GuiSettingsStore
from .models import AccountEntry, LoginMode
from .api_key_exporter import ApiKeyExporter
from .oidc_exporter import OidcCredentialExporter, OidcExportMode
from .password_vault import PasswordStatus, PasswordVault
from .redaction import mask_account, redact_text
from .worker_events import ResultMode, WorkerEvent


# access_token 距过期不足这个秒数就先刷新(留足建 key 的时间)。
_API_KEY_REFRESH_SKEW_SECONDS = 300


def _default_api_key_transport_factory():
    from .enterprise_http import CurlCffiTransport

    return CurlCffiTransport(timeout=45)


@dataclass(frozen=True, slots=True)
class LoginExportReport:
    selected: int
    reused: int
    logged_in: int
    failed: int
    exported: int


@dataclass(frozen=True, slots=True)
class ApiKeyExtractionReport:
    selected: int
    created: int
    reused: int
    refreshed: int
    failed: int
    skipped: int
    export_path: str | None


@dataclass(frozen=True, slots=True)
class PipelineReport:
    """并发「边登边提」流水线结果:登录阶段 + API Key 阶段合并统计。"""
    selected: int
    logged_in: int
    reused: int
    login_failed: int
    keys_created: int
    keys_reused: int
    keys_refreshed: int
    keys_failed: int
    export_path: str | None


@dataclass(frozen=True, slots=True)
class QuotaRefreshReport:
    selected: int
    updated: int
    refreshed: int
    failed: int
    skipped: int


@dataclass(frozen=True, slots=True)
class LoginProgressEvent:
    account_id: int
    index: int
    total: int
    completed: int
    status: str
    account_masked: str
    code: str | None = None
    stage: str | None = None


def form_from_saved_settings(
    saved: GuiSavedSettings,
    *,
    mode: LoginMode,
    credential_path: Path,
    checkpoint_path: Path,
    home_proxies_override: str | None = None,
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
        create_api_key=saved.create_api_key,
        api_key_skip_if_exists=saved.api_key_skip_if_exists,
        proxy_enabled=saved.proxy_enabled,
        system_proxy=saved.system_proxy,
        home_proxies=(
            home_proxies_override
            if home_proxies_override is not None
            else saved.home_proxies
        ),
    )


class AccountLoginCoordinator:
    def __init__(
        self,
        repository: AccountRepository,
        settings_store: GuiSettingsStore,
        *,
        exporter=None,
        api_key_exporter=None,
        runtime_factory=GuiRuntime,
        emit=lambda _event: None,
        api_key_transport_factory=None,
        token_refresher=refresh_access_token,
        now=lambda: datetime.now(timezone.utc),
    ):
        self.repository = repository
        self.settings_store = settings_store
        self.exporter = exporter or OidcCredentialExporter()
        self.api_key_exporter = api_key_exporter or ApiKeyExporter()
        self.runtime_factory = runtime_factory
        self.emit = emit
        self.api_key_transport_factory = (
            api_key_transport_factory or _default_api_key_transport_factory
        )
        self.token_refresher = token_refresher
        self.now = now

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
        self,
        account_ids: list[int],
        *,
        force_relogin: bool = False,
        concurrency: int = 5,
        progress=None,
        event_sink=None,
        home_proxies_override: str | None = None,
        export_files: bool = True,
    ) -> LoginExportReport:
        """并发登录并把凭据落库(Semaphore(concurrency) 控并发)。

        export_files=True(默认):登录后导出 OIDC JSON + apikey 文本文件。
        export_files=False(独立「登录」动作):只登录 + 存库(状态置 VALID),
        不写任何文件;之后取 JSON / 提取 API Key 会复用库存凭据,不再二次登录。
        企业号纯 HTTP 真并发;微软号(单浏览器)由 runtime.login_one 内部锁串行。
        """
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

        progress_sink = progress or (lambda _event: None)
        runtime_event_sink = event_sink or self.emit
        total = len(accounts)
        completed = 0
        positions = {
            account.id: index
            for index, account in enumerate(accounts, start=1)
        }

        def notify(account, status, *, code=None, stage=None, terminal=False):
            nonlocal completed
            if terminal:
                completed += 1
            progress_sink(
                LoginProgressEvent(
                    account_id=account.id,
                    index=positions[account.id],
                    total=total,
                    completed=completed,
                    status=status,
                    account_masked=mask_account(account.account),
                    code=code,
                    stage=stage,
                )
            )

        for account in accounts:
            notify(account, "waiting")

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
                notify(account, "reused", terminal=True)
            else:
                pending.append(account)

        new_credentials: list = []
        stats = {"failed": 0}
        lock = asyncio.Lock()
        limiter = asyncio.Semaphore(max(1, int(concurrency)))
        cancelled = False

        async def handle_login(account, runtime):
            async with limiter:
                notify(account, "running")
                entry = AccountEntry(
                    positions[account.id], account.account,
                    account.current_password or account.initial_password or "",
                    account.start_url,
                )
                try:
                    credential = await runtime.login_one(entry)
                except asyncio.CancelledError:
                    raise
                except Exception as error:  # noqa: BLE001
                    self.repository.mark_login_failed(
                        account.id, "login_failed", "automatic_login"
                    )
                    async with lock:
                        stats["failed"] += 1
                    notify(account, "failed", code="login_failed",
                           stage="automatic_login", terminal=True)
                    return
                self.repository.save_credential(account.id, credential)
                async with lock:
                    new_credentials.append((account, credential))
                notify(account, "success", terminal=True)

        runtimes: list = []
        tasks: list = []
        try:
            with tempfile.TemporaryDirectory(prefix="kiro-login-") as tmp:
                for mode in LoginMode:
                    batch = [item for item in pending if item.login_mode is mode]
                    if not batch:
                        continue
                    self.repository.mark_login_running([item.id for item in batch])
                    for item in batch:
                        notify(item, "running")
                    form = form_from_saved_settings(
                        saved, mode=mode,
                        credential_path=Path(tmp) / f"cred-{mode.value}.json",
                        checkpoint_path=Path(tmp) / f"ckpt-{mode.value}.jsonl",
                        home_proxies_override=home_proxies_override,
                    )
                    runtime = self.runtime_factory(form, runtime_event_sink)
                    await runtime.open_for_concurrent()
                    runtimes.append((runtime, form, batch))
                    tasks.extend(
                        asyncio.ensure_future(handle_login(a, runtime)) for a in batch
                    )
                try:
                    await asyncio.gather(*tasks)
                except asyncio.CancelledError:
                    cancelled = True
                    for t in tasks:
                        t.cancel()
                    await asyncio.gather(*tasks, return_exceptions=True)
        finally:
            for runtime, form, batch in runtimes:
                try:
                    await runtime.close()
                except Exception:  # noqa: BLE001
                    pass
                try:
                    self._sync_confirmed_passwords(form, batch)
                except Exception:  # noqa: BLE001
                    pass
        # 取消时把没登成功的号标 cancelled(已成功的已即时落库)
        if cancelled:
            done_ids = {a.id for a, _c in new_credentials}
            for account in pending:
                if account.id in done_ids:
                    continue
                self.repository.mark_login_failed(account.id, "cancelled", "automatic_login")
        failed = stats["failed"]
        all_credentials = [item[1] for item in reusable + new_credentials]
        if export_files and all_credentials:
            output_directory = Path(saved.oidc_export_directory) if saved.oidc_export_directory else Path(saved.credential_path).resolve().parent
            self.exporter.export(
                all_credentials,
                output_directory=output_directory,
                mode=OidcExportMode(saved.oidc_export_mode),
            )
            self.api_key_exporter.export(
                all_credentials,
                output_directory=output_directory,
            )
        if cancelled:
            # 已成功的号已即时落库、也已导出;向 UI 报「已终止」
            raise asyncio.CancelledError
        return LoginExportReport(
            selected=len(accounts),
            reused=len(reusable),
            logged_in=len(new_credentials),
            failed=failed,
            exported=len(all_credentials) if export_files else 0,
        )

    @staticmethod
    def _load_credential_for(store_path: Path, item):
        """从(可能正在被 runner 追加写入的)凭据文件里,按账号 + start_url 取该号凭据。"""
        if not store_path.exists():
            return None
        try:
            records = CredentialStore(store_path).load()
        except Exception:  # noqa: BLE001 - 半写/瞬时读失败:交由收尾对账兜底
            return None
        key = (
            item.account.casefold(),
            (item.start_url or "").rstrip("/").casefold(),
        )
        for record in records:
            record_key = (
                record.email.casefold(),
                (record.start_url or "").rstrip("/").casefold(),
            )
            if record_key == key:
                return record
        return None

    async def refresh_quota(
        self,
        account_ids: list[int],
        *,
        progress=None,
        event_sink=None,
        home_proxies_override: str | None = None,
    ) -> QuotaRefreshReport:
        """手动刷新剩余额度:载凭据→近过期先刷 token→缺 profileArn 先解析→查额度→存库。

        只读查询,不改账号状态。单账号失败不中断批次。走链式代理(与提取 apikey 同源)。
        """
        saved = self.settings_store.load()
        ids = list(dict.fromkeys(int(item) for item in account_ids))
        if not ids:
            raise ValueError("请先选择账号")
        accounts = [self.repository.get(item) for item in ids]
        if any(item.lifecycle_status is LifecycleStatus.SOLD for item in accounts):
            raise ValueError("已售出账号请先恢复管理")

        emit = event_sink or self.emit
        transport_factory = self._resolve_api_key_transport_factory(
            saved, emit, home_proxies_override=home_proxies_override
        )
        progress_sink = progress or (lambda _event: None)
        total = len(accounts)
        completed = 0
        positions = {account.id: index for index, account in enumerate(accounts, start=1)}

        updated = refreshed = failed = skipped = 0

        def notify(account, status, *, code=None, terminal=False):
            nonlocal completed
            if terminal:
                completed += 1
            progress_sink(
                LoginProgressEvent(
                    account_id=account.id,
                    index=positions[account.id],
                    total=total,
                    completed=completed,
                    status=status,
                    account_masked=mask_account(account.account),
                    code=code,
                )
            )

        for account in accounts:
            notify(account, "waiting")

        for account in accounts:
            notify(account, "running")
            credential = self.repository.load_credential(account.id)
            if credential is None:
                skipped += 1
                emit(
                    WorkerEvent(
                        "quota_failed",
                        {
                            "accountMasked": mask_account(account.account),
                            "code": "no_credential",
                            "message": "账号还没有登录凭据,请先登录",
                        },
                    )
                )
                notify(account, "failed", code="no_credential", terminal=True)
                continue

            transport = transport_factory()
            try:
                if await self._maybe_refresh_token(account, credential, transport, saved, emit):
                    refreshed += 1
                snapshot = await self._fetch_quota(credential, account, transport, saved)
            except (UsageError, ApiKeyError) as error:
                failed += 1
                emit(
                    WorkerEvent(
                        "quota_failed",
                        {
                            "accountMasked": mask_account(account.account),
                            "code": error.code,
                            "stage": error.stage,
                            "message": redact_text(str(error)),
                        },
                    )
                )
                notify(account, "failed", code=error.code, terminal=True)
                continue
            finally:
                try:
                    await transport.close()
                except Exception:  # noqa: BLE001 - 关闭失败不阻断
                    pass

            self.repository.save_quota(
                account.id,
                remaining=snapshot.remaining,
                total=snapshot.total,
                used=snapshot.used,
                subscription=snapshot.subscription,
                free_trial=snapshot.free_trial,
                next_reset=snapshot.next_reset,
            )
            updated += 1
            emit(
                WorkerEvent(
                    "quota_updated",
                    {
                        "accountMasked": mask_account(account.account),
                        "display": snapshot.display(),
                        "subscription": snapshot.subscription,
                    },
                )
            )
            notify(account, "success", terminal=True)

        return QuotaRefreshReport(
            selected=total,
            updated=updated,
            refreshed=refreshed,
            failed=failed,
            skipped=skipped,
        )

    async def _fetch_quota(self, credential, account, transport, saved):
        token = (credential.access_token or "").strip()
        if not token:
            raise UsageError("missing_token", "usage_auth", False, "凭据缺少 access_token")
        token_type = (
            "EXTERNAL_IDP"
            if credential.auth_method.casefold() == "external_idp"
            else None
        )
        region = credential.region or (saved.region if saved else None) or "us-east-1"
        profile_arn = credential.profile_arn
        if not profile_arn:
            profile_arn = await resolve_profile_arn(
                transport, token=token, region=region, token_type=token_type
            )
            if profile_arn:
                credential.profile_arn = profile_arn
                self.repository.save_credential(account.id, credential)
        return await get_usage_limits(
            transport,
            token=token,
            profile_arn=profile_arn,
            region=region,
            token_type=token_type,
        )

    async def login_and_extract_api_keys(
        self,
        account_ids: list[int],
        *,
        force_relogin: bool = False,
        progress=None,
        event_sink=None,
        home_proxies_override: str | None = None,
    ) -> ApiKeyExtractionReport:
        """一步到位:缺 JSON 的账号先自动登录取凭据,再统一提取 API Key。

        登录复用现有 run()(自带取 JSON + 存库 + 走链式代理);登录后立即提取。
        没配自动登录设置时跳过登录、直接对已有凭据提取(缺凭据的账号会被标注跳过)。
        """
        emit = event_sink or self.emit
        ids = list(dict.fromkeys(int(item) for item in account_ids))
        if not ids:
            raise ValueError("请先选择账号")

        need_login = ids
        if not force_relogin:
            need_login = [
                account_id
                for account_id in ids
                if self.repository.get(account_id).credential_status
                is not CredentialStatus.VALID
                or self.repository.load_credential(account_id) is None
            ]
        if need_login:
            emit(WorkerEvent("api_key_phase", {"phase": "login", "count": len(need_login)}))
            try:
                await self.run(
                    need_login,
                    force_relogin=force_relogin,
                    progress=progress,
                    event_sink=event_sink,
                    home_proxies_override=home_proxies_override,
                )
            except ValueError as error:
                # 未配置自动登录设置:跳过登录,后续提取会标注缺凭据账号。
                emit(
                    WorkerEvent(
                        "security_warning",
                        {"message": f"自动登录已跳过：{error}"},
                    )
                )
        emit(WorkerEvent("api_key_phase", {"phase": "extract", "count": len(ids)}))
        return await self.extract_api_keys(
            ids,
            progress=progress,
            event_sink=event_sink,
            home_proxies_override=home_proxies_override,
        )

    async def login_and_extract_pipeline(
        self,
        account_ids: list[int],
        *,
        concurrency: int = 5,
        force_relogin: bool = False,
        progress=None,
        event_sink=None,
        home_proxies_override: str | None = None,
    ) -> PipelineReport:
        """并发「边登边提」流水线:每号 登录→存库→立刻提它的 API Key,N 条链并发。

        进度事件带 stage="login"/"apikey" 供 UI 双栏分显。有效凭据的号跳过登录直接提取。
        企业号纯 HTTP 真并发;微软号(单浏览器)由 runtime.login_one 内部锁串行。
        单号失败不影响其他号;终止时已完成的号已即时落库。
        """
        saved = self.settings_store.load()
        if saved is None:
            raise ValueError("请先打开自动登录设置并保存配置")
        ids = list(dict.fromkeys(int(item) for item in account_ids))
        if not ids:
            raise ValueError("请先选择账号")
        accounts = [self.repository.get(item, include_secrets=True) for item in ids]
        if any(item.lifecycle_status is LifecycleStatus.SOLD for item in accounts):
            raise ValueError("已售出账号请先恢复管理")

        progress_sink = progress or (lambda _event: None)
        emit = event_sink or self.emit
        total = len(accounts)
        positions = {a.id: i for i, a in enumerate(accounts, start=1)}
        counters = {"login": 0, "apikey": 0}
        # 计数(用锁护住,gather 并发下也安全;单事件循环其实原子,加锁更稳妥)
        stats = {"logged_in": 0, "reused": 0, "login_failed": 0,
                 "created": 0, "keys_reused": 0, "refreshed": 0, "keys_failed": 0}
        lock = asyncio.Lock()
        limiter = asyncio.Semaphore(max(1, int(concurrency)))
        transport_factory = self._resolve_api_key_transport_factory(
            saved, emit, home_proxies_override=home_proxies_override
        )

        def notify(account, status, *, stage, code=None, terminal=False):
            if terminal:
                counters[stage] += 1
            progress_sink(
                LoginProgressEvent(
                    account_id=account.id, index=positions[account.id], total=total,
                    completed=counters[stage], status=status,
                    account_masked=mask_account(account.account), code=code, stage=stage,
                )
            )

        for account in accounts:
            notify(account, "waiting", stage="login")
            notify(account, "waiting", stage="apikey")

        # 分类:有效凭据直接复用(跳过登录),其余待登录
        reusable: dict[int, object] = {}
        pending: list = []
        for account in accounts:
            credential = self.repository.load_credential(account.id)
            if (not force_relogin
                    and account.credential_status is CredentialStatus.VALID
                    and credential is not None):
                reusable[account.id] = credential
            else:
                pending.append(account)

        updated_records: list[CredentialRecord] = []

        async def extract_stage(account, credential):
            """提取该号 API Key(近过期先刷 token),发 apikey 阶段事件。"""
            transport = transport_factory()
            try:
                if await self._maybe_refresh_token(account, credential, transport, saved, emit):
                    async with lock:
                        stats["refreshed"] += 1
                result = await self._ensure_key_for_credential(
                    credential, account, transport, saved
                )
            except ApiKeyError as error:
                async with lock:
                    stats["keys_failed"] += 1
                emit(WorkerEvent("api_key_failed", {
                    "accountMasked": mask_account(account.account),
                    "code": error.code, "stage": error.stage,
                    "message": redact_text(str(error))}))
                notify(account, "failed", stage="apikey", code=error.code, terminal=True)
                return
            finally:
                try:
                    await transport.close()
                except Exception:  # noqa: BLE001
                    pass
            if result.profile_arn and not credential.profile_arn:
                credential.profile_arn = result.profile_arn
            if result.raw_key:
                credential.kiro_api_key = result.raw_key
                async with lock:
                    stats["created"] += 1
                emit(WorkerEvent("api_key_created", {
                    "accountMasked": mask_account(account.account),
                    "keyPrefix": result.raw_key[:12]}))
                notify(account, "success", stage="apikey", terminal=True)
            else:
                async with lock:
                    stats["keys_reused"] += 1
                emit(WorkerEvent("api_key_reused", {
                    "accountMasked": mask_account(account.account),
                    "hasStoredKey": bool(credential.kiro_api_key)}))
                notify(account, "reused", stage="apikey", terminal=True)
            self.repository.save_credential(account.id, credential)
            async with lock:
                updated_records.append(credential)

        async def handle_reused(account, credential):
            async with limiter:
                notify(account, "reused", stage="login", terminal=True)
                async with lock:
                    stats["reused"] += 1
                await extract_stage(account, credential)

        async def handle_login(account, runtime):
            async with limiter:
                notify(account, "running", stage="login")
                entry = AccountEntry(
                    positions[account.id], account.account,
                    account.current_password or account.initial_password or "",
                    account.start_url,
                )
                try:
                    credential = await runtime.login_one(entry)
                except asyncio.CancelledError:
                    raise
                except Exception as error:  # noqa: BLE001
                    self.repository.mark_login_failed(
                        account.id, "login_failed", "automatic_login"
                    )
                    async with lock:
                        stats["login_failed"] += 1
                    emit(WorkerEvent("account_finished", {
                        "status": "failed", "code": "login_failed",
                        "message": redact_text(str(error))}))
                    notify(account, "failed", stage="login", code="login_failed", terminal=True)
                    # 登录失败:apikey 阶段也标 skipped,让右栏计数也走到 N
                    notify(account, "failed", stage="apikey", code="no_credential", terminal=True)
                    return
                self.repository.save_credential(account.id, credential)
                async with lock:
                    stats["logged_in"] += 1
                notify(account, "success", stage="login", terminal=True)
                await extract_stage(account, credential)

        tasks = [
            asyncio.ensure_future(handle_reused(a, reusable[a.id]))
            for a in accounts if a.id in reusable
        ]
        cancelled = False
        # 待登录按 mode 分组,各开一个 runtime(微软号共享 browser,内部锁串行)
        runtimes: list = []
        try:
            with tempfile.TemporaryDirectory(prefix="kiro-pipeline-") as tmp:
                for mode in LoginMode:
                    batch = [a for a in pending if a.login_mode is mode]
                    if not batch:
                        continue
                    self.repository.mark_login_running([a.id for a in batch])
                    form = form_from_saved_settings(
                        saved, mode=mode,
                        credential_path=Path(tmp) / f"cred-{mode.value}.json",
                        checkpoint_path=Path(tmp) / f"ckpt-{mode.value}.jsonl",
                        home_proxies_override=home_proxies_override,
                    )
                    runtime = self.runtime_factory(form, emit)
                    await runtime.open_for_concurrent()
                    runtimes.append((runtime, form, batch))
                    tasks.extend(
                        asyncio.ensure_future(handle_login(a, runtime)) for a in batch
                    )
                try:
                    await asyncio.gather(*tasks)
                except asyncio.CancelledError:
                    cancelled = True
                    for t in tasks:
                        t.cancel()
                    await asyncio.gather(*tasks, return_exceptions=True)
        finally:
            for runtime, form, batch in runtimes:
                try:
                    await runtime.close()
                except Exception:  # noqa: BLE001
                    pass
                try:
                    self._sync_confirmed_passwords(form, batch)
                except Exception:  # noqa: BLE001
                    pass

        export_path = None
        # OIDC JSON:导所有拿到凭据的号(复用的 + 本次登录成功的,不论 key 是否建成)
        oidc_creds = list(reusable.values())
        for account in accounts:
            if account.id in reusable:
                continue
            credential = self.repository.load_credential(account.id)
            if credential is not None:
                oidc_creds.append(credential)
        if oidc_creds:
            output_directory = (
                Path(saved.oidc_export_directory) if saved.oidc_export_directory
                else Path(saved.credential_path).resolve().parent
            )
            try:
                self.exporter.export(
                    oidc_creds, output_directory=output_directory,
                    mode=OidcExportMode(saved.oidc_export_mode),
                )
            except Exception:  # noqa: BLE001 - 导出失败不掩盖已建 key
                pass
            report_out = self.api_key_exporter.export(
                updated_records, output_directory=output_directory
            )
            if report_out is not None:
                export_path = str(report_out.path)
                emit(WorkerEvent("api_key_exported",
                                 {"path": export_path, "count": report_out.with_key}))

        if cancelled:
            emit(WorkerEvent("security_warning", {"message": "并发流水线已终止：已完成的号已保存。"}))
            raise asyncio.CancelledError

        report = PipelineReport(
            selected=total, logged_in=stats["logged_in"], reused=stats["reused"],
            login_failed=stats["login_failed"], keys_created=stats["created"],
            keys_reused=stats["keys_reused"], keys_refreshed=stats["refreshed"],
            keys_failed=stats["keys_failed"], export_path=export_path,
        )
        return report

    async def extract_api_keys(
        self,
        account_ids: list[int],
        *,
        concurrency: int = 5,
        progress=None,
        event_sink=None,
        home_proxies_override: str | None = None,
    ) -> ApiKeyExtractionReport:
        """对已登录账号用库存凭据并发提取 ksk_ API Key(必要时先刷新 token)。

        每号:载凭据 → token 近过期则刷新并回写 → ensure_api_key → 回写 kiroApiKey
        并 save_credential → 发事件。Semaphore(concurrency) 控并发。单号失败不中断批次。
        """
        saved = self.settings_store.load()
        ids = list(dict.fromkeys(int(item) for item in account_ids))
        if not ids:
            raise ValueError("请先选择账号")
        accounts = [self.repository.get(item) for item in ids]
        if any(item.lifecycle_status is LifecycleStatus.SOLD for item in accounts):
            raise ValueError("已售出账号请先恢复管理")

        transport_factory = self._resolve_api_key_transport_factory(
            saved, event_sink or self.emit, home_proxies_override=home_proxies_override
        )

        progress_sink = progress or (lambda _event: None)
        emit = event_sink or self.emit
        total = len(accounts)
        completed = 0
        positions = {account.id: index for index, account in enumerate(accounts, start=1)}

        stats = {"created": 0, "reused": 0, "refreshed": 0, "failed": 0, "skipped": 0}
        updated_records: list[CredentialRecord] = []
        lock = asyncio.Lock()
        limiter = asyncio.Semaphore(max(1, int(concurrency)))

        def notify(account, status, *, code=None, stage=None, terminal=False):
            nonlocal completed
            if terminal:
                completed += 1
            progress_sink(
                LoginProgressEvent(
                    account_id=account.id,
                    index=positions[account.id],
                    total=total,
                    completed=completed,
                    status=status,
                    account_masked=mask_account(account.account),
                    code=code,
                    stage=stage,
                )
            )

        for account in accounts:
            notify(account, "waiting")

        async def process(account):
            async with limiter:
                notify(account, "running")
                credential = self.repository.load_credential(account.id)
                if credential is None:
                    async with lock:
                        stats["skipped"] += 1
                    emit(WorkerEvent("api_key_failed", {
                        "accountMasked": mask_account(account.account),
                        "code": "no_credential",
                        "message": "账号还没有登录凭据,请先获取 JSON"}))
                    notify(account, "failed", code="no_credential", terminal=True)
                    return
                transport = transport_factory()
                try:
                    if await self._maybe_refresh_token(account, credential, transport, saved, emit):
                        async with lock:
                            stats["refreshed"] += 1
                    result = await self._ensure_key_for_credential(
                        credential, account, transport, saved
                    )
                except ApiKeyError as error:
                    async with lock:
                        stats["failed"] += 1
                    emit(WorkerEvent("api_key_failed", {
                        "accountMasked": mask_account(account.account),
                        "code": error.code, "stage": error.stage,
                        "message": redact_text(str(error))}))
                    notify(account, "failed", code=error.code, stage=error.stage, terminal=True)
                    return
                finally:
                    try:
                        await transport.close()
                    except Exception:  # noqa: BLE001 - 关闭失败不阻断
                        pass

                if result.profile_arn and not credential.profile_arn:
                    credential.profile_arn = result.profile_arn
                if result.raw_key:
                    credential.kiro_api_key = result.raw_key
                    async with lock:
                        stats["created"] += 1
                    emit(WorkerEvent("api_key_created", {
                        "accountMasked": mask_account(account.account),
                        "keyPrefix": result.raw_key[:12]}))
                    notify(account, "success", terminal=True)
                else:
                    async with lock:
                        stats["reused"] += 1
                    emit(WorkerEvent("api_key_reused", {
                        "accountMasked": mask_account(account.account),
                        "hasStoredKey": bool(credential.kiro_api_key)}))
                    notify(account, "reused", terminal=True)
                self.repository.save_credential(account.id, credential)
                async with lock:
                    updated_records.append(credential)

        await asyncio.gather(*(process(a) for a in accounts))
        created = stats["created"]; reused = stats["reused"]
        refreshed = stats["refreshed"]; failed = stats["failed"]; skipped = stats["skipped"]

        export_path = None
        if updated_records:
            output_directory = (
                Path(saved.oidc_export_directory)
                if saved is not None and saved.oidc_export_directory
                else self._api_key_output_directory(saved)
            )
            report = self.api_key_exporter.export(
                updated_records, output_directory=output_directory
            )
            if report is not None:
                export_path = str(report.path)
                emit(
                    WorkerEvent(
                        "api_key_exported",
                        {"path": export_path, "count": report.with_key},
                    )
                )

        return ApiKeyExtractionReport(
            selected=total,
            created=created,
            reused=reused,
            refreshed=refreshed,
            failed=failed,
            skipped=skipped,
            export_path=export_path,
        )

    def _resolve_api_key_transport_factory(self, saved, emit, *, home_proxies_override=None):
        """启用链式代理时用 ProxyChain 工厂;否则用默认(直连)工厂。"""
        if saved is None or not getattr(saved, "proxy_enabled", False):
            return self.api_key_transport_factory
        from .proxy_chain import ProxyChain

        chain = ProxyChain.from_settings(
            system_proxy=saved.system_proxy,
            home_proxies_text=(
                home_proxies_override
                if home_proxies_override is not None
                else saved.home_proxies
            ),
        )
        if chain is None:
            return self.api_key_transport_factory
        emit(
            WorkerEvent(
                "security_warning",
                {"message": "API Key 提取已启用链式代理：系统代理 → 家宽出口"},
            )
        )
        return chain.transport_factory

    async def _maybe_refresh_token(
        self, account, credential: CredentialRecord, transport, saved, emit
    ) -> bool:
        """token 近过期且具备刷新材料时刷新并回写;返回是否真的刷新过。"""
        if not self._token_needs_refresh(credential.expires_at):
            return False
        if not (
            credential.client_id
            and credential.client_secret
            and credential.refresh_token
            and credential.start_url
        ):
            # 缺刷新材料:交由后续 ensure_api_key 用库存 token 尝试,失败如实上报。
            return False
        region = credential.region or (saved.region if saved else None) or "us-east-1"
        result = await self.token_refresher(
            transport,
            client_id=credential.client_id,
            client_secret=credential.client_secret,
            refresh_token=credential.refresh_token,
            start_url=credential.start_url,
            region=region,
        )
        credential.access_token = result.access_token
        if result.refresh_token:
            credential.refresh_token = result.refresh_token
        credential.expires_at = self._expires_at_from(result.expires_in)
        self.repository.save_credential(account.id, credential)
        emit(
            WorkerEvent(
                "api_key_refreshed",
                {"accountMasked": mask_account(account.account)},
            )
        )
        return True

    async def _ensure_key_for_credential(
        self, credential: CredentialRecord, account, transport, saved
    ):
        token = (credential.access_token or "").strip()
        if not token:
            raise ApiKeyError(
                "missing_token", "api_key_auth", False, "凭据缺少 access_token"
            )
        token_type = (
            "EXTERNAL_IDP"
            if credential.auth_method.casefold() == "external_idp"
            else None
        )
        region = credential.region or (saved.region if saved else None) or "us-east-1"
        skip = bool(saved.api_key_skip_if_exists) if saved is not None else False
        return await ensure_api_key(
            transport,
            token=token,
            label=account.account,
            region=region,
            profile_arn=credential.profile_arn,
            token_type=token_type,
            skip_if_labeled_exists=skip,
        )

    def _token_needs_refresh(self, expires_at: str | None) -> bool:
        if not expires_at:
            return False  # 无过期信息:不主动刷新,直接用库存 token。
        try:
            deadline = datetime.fromisoformat(expires_at.replace("Z", "+00:00"))
        except ValueError:
            return False
        if deadline.tzinfo is None:
            deadline = deadline.replace(tzinfo=timezone.utc)
        remaining = (deadline - self.now()).total_seconds()
        return remaining < _API_KEY_REFRESH_SKEW_SECONDS

    def _expires_at_from(self, expires_in: int | None) -> str | None:
        if expires_in is None:
            return None
        from datetime import timedelta

        return (
            (self.now() + timedelta(seconds=expires_in))
            .isoformat()
            .replace("+00:00", "Z")
        )

    def _api_key_output_directory(self, saved) -> Path:
        if saved is not None and saved.credential_path:
            return Path(saved.credential_path).resolve().parent
        return Path.cwd()

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
