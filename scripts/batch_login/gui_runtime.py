from __future__ import annotations

from pathlib import Path

import httpx
from playwright.async_api import async_playwright

from .browser_flows import BrowserFlows
from .credential_store import CredentialStore
from .gui_controller import GuiController, GuiFormState
from .local_auth import LocalEnterpriseAuth, LocalMicrosoftAuth
from .local_checkpoint import LocalCheckpointStore
from .local_idc import LocalIdcClient
from .local_microsoft import MicrosoftProtocol
from .local_runner import LocalBatchRunner
from .models import AccountEntry
from .rs_import import RsImportClient
from .ssh_tunnel import SshTunnel, SshTunnelSettings
from .worker_events import ResultMode, WorkerEvent


class _DeferredImporter:
    def __init__(self, runtime: "GuiRuntime"):
        self.runtime = runtime

    async def batch_import(self, credentials, on_event):
        importer = await self.runtime._connect_importer()
        if importer is None:
            raise RuntimeError("保存并导入模式缺少 RS 导入客户端")
        return await importer.batch_import(credentials, on_event)


class GuiRuntime:
    def __init__(self, form: GuiFormState, emit):
        self.form = form
        self.emit = emit
        self.http: httpx.AsyncClient | None = None
        self.playwright = None
        self.browser = None
        self.importer: RsImportClient | None = None
        self.tunnel: SshTunnel | None = None

    async def _connect_importer(self) -> RsImportClient | None:
        if self.form.result_mode is ResultMode.SAVE_ONLY:
            return None
        if self.importer is not None:
            return self.importer
        base_url = self.form.rs_url.strip()
        if self.form.use_ssh:
            settings = SshTunnelSettings(
                host=self.form.ssh_host,
                user=self.form.ssh_user,
                ssh_port=self.form.ssh_port,
                remote_host=self.form.remote_host,
                remote_port=self.form.remote_port,
                local_port=self.form.local_port,
                identity_file=self.form.identity_file or None,
            )
            self.tunnel = SshTunnel(settings)
            base_url = await self.tunnel.start()
        self.importer = RsImportClient(base_url, self.form.admin_key)
        await self.importer.preflight()
        return self.importer

    def runner_importer(self):
        if self.form.result_mode is ResultMode.SAVE_ONLY:
            return None
        return _DeferredImporter(self)

    async def run(self, entries: list[AccountEntry]):
        store = CredentialStore(
            Path(self.form.credential_path),
            warning_sink=lambda message: self.emit(
                WorkerEvent("security_warning", {"message": message})
            ),
        )
        checkpoint = LocalCheckpointStore(
            self.form.to_run_settings().checkpoint_path
        )
        self.http = httpx.AsyncClient(timeout=30)
        self.playwright = await async_playwright().start()
        self.browser = await self.playwright.chromium.launch(
            headless=self.form.headless
        )

        def emit_browser_event(raw_event):
            event = dict(raw_event)
            kind = str(event.pop("kind", "browser_event"))
            self.emit(WorkerEvent(kind, event))

        browser_flows = BrowserFlows(
            self.browser,
            timeout_seconds=self.form.timeout_seconds,
            mfa_timeout_seconds=self.form.mfa_timeout_seconds,
            event_sink=emit_browser_event,
        )
        runner = LocalBatchRunner(
            enterprise=LocalEnterpriseAuth(
                LocalIdcClient(self.http),
                browser_flows,
            ),
            microsoft=LocalMicrosoftAuth(
                MicrosoftProtocol(self.http),
                browser_flows,
            ),
            store=store,
            checkpoint=checkpoint,
            importer=self.runner_importer(),
            emit=self.emit,
        )
        return await runner.run(entries, self.form.to_run_settings())

    async def import_existing(self):
        records = CredentialStore(Path(self.form.credential_path)).load()
        if not records:
            raise ValueError("完整凭据 JSON 中没有可导入账号")
        importer = await self._connect_importer()
        if importer is None:
            raise ValueError("导入已有 JSON 必须选择 RS 导入模式")
        self.emit(
            WorkerEvent(
                "batch_started",
                {"total": len(records), "importOnly": True},
            )
        )
        summary = await importer.batch_import(
            [record.as_add_request() for record in records],
            lambda event: self.emit(WorkerEvent("import_event", event)),
        )
        self.emit(
            WorkerEvent(
                "batch_finished",
                {"importOnly": True, **summary},
            )
        )
        return summary

    async def close(self) -> None:
        first_error: BaseException | None = None
        resources = (
            ("browser", "close"),
            ("playwright", "stop"),
            ("http", "aclose"),
            ("importer", "aclose"),
            ("tunnel", "stop"),
        )
        for attribute, method_name in resources:
            resource = getattr(self, attribute)
            setattr(self, attribute, None)
            if resource is None:
                continue
            try:
                await getattr(resource, method_name)()
            except BaseException as error:
                if first_error is None:
                    first_error = error
        if first_error is not None:
            raise first_error


def build_default_controller() -> GuiController:
    return GuiController(runtime_factory=GuiRuntime)
