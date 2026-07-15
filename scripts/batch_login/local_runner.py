from __future__ import annotations

import asyncio
from dataclasses import asdict
from uuid import uuid4

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
    ):
        self.enterprise = enterprise
        self.microsoft = microsoft
        self.store = store
        self.checkpoint = checkpoint
        self.importer = importer
        self.emit = emit
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
                scope = self._scope(settings)
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
                    settings.start_url or "",
                    settings.region,
                ),
            )
        return await self.microsoft.login(
            entry,
            MicrosoftSettings(settings.region),
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
            scope=self._scope(settings),
            status=status,
            stage=error.stage,
            retryable=error.retryable,
            credential_saved=False,
            code=error.code,
            message=redact_text(str(error)),
        )
        self.checkpoint.append(record)
        self.emit(
            WorkerEvent(
                "account_finished",
                {
                    "status": status,
                    "code": error.code,
                    "credentialSaved": False,
                },
            )
        )

    @staticmethod
    def _scope(settings: LocalRunSettings) -> str:
        if settings.mode is LoginMode.MICROSOFT:
            return "microsoft"
        return settings.start_url or ""
