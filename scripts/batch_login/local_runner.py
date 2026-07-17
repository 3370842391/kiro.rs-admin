from __future__ import annotations

import asyncio
from dataclasses import asdict
from uuid import uuid4

from .api_key_client import ApiKeyError, ensure_api_key
from .browser_flows import BrowserFlowError
from .credential_models import CredentialRecord
from .enterprise_http import EnterpriseHttpError
from .local_auth import (
    EnterpriseSettings,
    LocalAuthError,
    MicrosoftSettings,
)
from .local_checkpoint import LocalRunRecord
from .models import AccountEntry, LoginMode
from .redaction import mask_account, redact_text
from .worker_events import (
    BatchSummary,
    LocalRunSettings,
    ResultMode,
    WorkerEvent,
)


class LocalBatchRunner:
    def __init__(
        self,
        *,
        enterprise,
        microsoft,
        store,
        checkpoint,
        importer=None,
        emit=lambda _event: None,
        api_key_transport_factory=None,
    ):
        self.enterprise = enterprise
        self.microsoft = microsoft
        self.store = store
        self.checkpoint = checkpoint
        self.importer = importer
        self.emit = emit
        self.api_key_transport_factory = api_key_transport_factory
        self.run_id = uuid4().hex

    async def run(
        self,
        entries: list[AccountEntry],
        settings: LocalRunSettings,
    ) -> BatchSummary:
        summary = BatchSummary(total=len(entries))
        saved_this_run: list[
            tuple[AccountEntry, CredentialRecord, LocalRunRecord]
        ] = []
        self.emit(WorkerEvent("batch_started", {"total": len(entries)}))
        try:
            for index, entry in enumerate(entries, start=1):
                scope = self._scope(settings, entry)
                if not self.checkpoint.should_run(
                    account=entry.account,
                    mode=settings.mode.value,
                    scope=scope,
                    resume=settings.resume,
                ):
                    continue
                self.emit(
                    WorkerEvent(
                        "account_started",
                        {
                            "index": index,
                            "total": len(entries),
                            "accountMasked": mask_account(entry.account),
                            "mode": settings.mode.value,
                        },
                    )
                )
                try:
                    credential = await self._login(entry, settings)
                    if settings.create_api_key:
                        await self._create_api_key(
                            entry, credential, settings, summary
                        )
                    added = self.store.append(credential)
                    if added:
                        summary.succeeded += 1
                    else:
                        summary.duplicate += 1
                    success_record = LocalRunRecord.success(
                        run_id=self.run_id,
                        line_number=entry.line_number,
                        account=entry.account,
                        mode=settings.mode.value,
                        scope=scope,
                        credential_saved=True,
                    )
                    self.checkpoint.append(success_record)
                    if added:
                        saved_this_run.append(
                            (entry, credential, success_record)
                        )
                    self.emit(
                        WorkerEvent(
                            "account_finished",
                            {
                                "status": (
                                    "success"
                                    if added
                                    else "duplicate_credential"
                                ),
                                "credentialSaved": True,
                            },
                        )
                    )
                except (LocalAuthError, BrowserFlowError, EnterpriseHttpError) as error:
                    self._record_failure(
                        summary,
                        entry,
                        settings,
                        error,
                    )

            await self._import_saved(saved_this_run, settings, summary)
            self.emit(WorkerEvent("batch_finished", asdict(summary)))
            return summary
        except asyncio.CancelledError:
            summary.cancelled += 1
            self.emit(WorkerEvent("batch_cancelled", asdict(summary)))
            raise

    async def _login(
        self,
        entry: AccountEntry,
        settings: LocalRunSettings,
    ) -> CredentialRecord:
        if settings.mode is LoginMode.ENTERPRISE:
            return await self.enterprise.login(
                entry,
                EnterpriseSettings(
                    entry.start_url or settings.start_url or "",
                    settings.region,
                ),
            )
        return await self.microsoft.login(
            entry,
            MicrosoftSettings(settings.region),
        )

    async def _create_api_key(
        self,
        entry: AccountEntry,
        credential: CredentialRecord,
        settings: LocalRunSettings,
        summary: BatchSummary,
    ) -> None:
        """登录成功后创建门户 ksk_ API Key,写入 credential;失败不影响登录结果。"""
        token = (credential.access_token or "").strip()
        if not token:
            summary.api_keys_failed += 1
            self.emit(
                WorkerEvent(
                    "api_key_failed",
                    {
                        "accountMasked": mask_account(entry.account),
                        "code": "missing_token",
                        "message": "凭据缺少 access_token,跳过建 Key",
                    },
                )
            )
            return
        # 企业 SSO(external_idp)调 ListAvailableProfiles 必须带 EXTERNAL_IDP;idc/social 不需要。
        token_type = (
            "EXTERNAL_IDP"
            if credential.auth_method.casefold() == "external_idp"
            else None
        )
        region = credential.region or settings.region or "us-east-1"
        if self.api_key_transport_factory is None:
            raise RuntimeError("建 API Key 缺少 transport 工厂")
        transport = self.api_key_transport_factory()
        try:
            result = await ensure_api_key(
                transport,
                token=token,
                label=entry.account,
                region=region,
                profile_arn=credential.profile_arn,
                token_type=token_type,
                skip_if_labeled_exists=settings.api_key_skip_if_exists,
            )
        except ApiKeyError as error:
            summary.api_keys_failed += 1
            self.emit(
                WorkerEvent(
                    "api_key_failed",
                    {
                        "accountMasked": mask_account(entry.account),
                        "code": error.code,
                        "stage": error.stage,
                        "message": redact_text(str(error)),
                    },
                )
            )
            return
        finally:
            try:
                await transport.close()
            except Exception:  # noqa: BLE001 - 关闭失败不阻断
                pass
        # 回填 profileArn(企业号登录流本不返回)
        if result.profile_arn and not credential.profile_arn:
            credential.profile_arn = result.profile_arn
        if result.raw_key:
            credential.kiro_api_key = result.raw_key
            summary.api_keys_created += 1
            self.emit(
                WorkerEvent(
                    "api_key_created",
                    {
                        "accountMasked": mask_account(entry.account),
                        "keyPrefix": result.raw_key[:12],
                    },
                )
            )
        else:
            # reused:同名 key 已存在且未取到 rawKey(库中若有旧值则保留)
            self.emit(
                WorkerEvent(
                    "api_key_reused",
                    {
                        "accountMasked": mask_account(entry.account),
                        "hasStoredKey": bool(credential.kiro_api_key),
                    },
                )
            )

    async def _import_saved(
        self,
        saved: list[tuple[AccountEntry, CredentialRecord, LocalRunRecord]],
        settings: LocalRunSettings,
        summary: BatchSummary,
    ) -> None:
        if settings.result_mode is not ResultMode.SAVE_AND_IMPORT or not saved:
            return
        if self.importer is None:
            raise RuntimeError("保存并导入模式缺少 RS 导入客户端")

        def on_import(event):
            index = event.get("index")
            if isinstance(index, int) and 0 <= index < len(saved):
                previous = saved[index][2]
                self.checkpoint.append_import_result(
                    previous,
                    import_status=str(event.get("status") or "failed"),
                    credential_id=event.get("credentialId"),
                    message=event.get("error"),
                )
            self.emit(WorkerEvent("import_event", event))

        import_summary = await self.importer.batch_import(
            [item[1].as_add_request() for item in saved],
            on_import,
        )
        summary.imported = int(import_summary.get("imported", 0)) + int(
            import_summary.get("verified", 0)
        )

    def _record_failure(
        self,
        summary: BatchSummary,
        entry: AccountEntry,
        settings: LocalRunSettings,
        error: LocalAuthError | BrowserFlowError | EnterpriseHttpError,
    ) -> None:
        manual = error.code in {"mfa_timeout", "captcha_required"}
        status = "manual_required" if manual else "failed"
        if manual:
            summary.manual_required += 1
        else:
            summary.failed += 1
        record = LocalRunRecord.for_account(
            run_id=self.run_id,
            line_number=entry.line_number,
            account=entry.account,
            mode=settings.mode.value,
            scope=self._scope(settings, entry),
            status=status,
            stage=error.stage,
            retryable=error.retryable,
            credential_saved=False,
            code=error.code,
            message=redact_text(str(error)),
        )
        self.checkpoint.append(record)
        payload = {
            "status": status,
            "code": error.code,
            "stage": error.stage,
            "retryable": error.retryable,
            "message": redact_text(str(error)),
            "credentialSaved": False,
        }
        status_code = getattr(error, "status_code", None)
        if isinstance(status_code, int):
            payload["httpStatus"] = status_code
        self.emit(
            WorkerEvent(
                "account_finished",
                payload,
            )
        )

    @staticmethod
    def _scope(
        settings: LocalRunSettings,
        entry: AccountEntry | None = None,
    ) -> str:
        if settings.mode is LoginMode.MICROSOFT:
            return "microsoft"
        if entry is not None and entry.start_url:
            return entry.start_url
        return settings.start_url or ""
