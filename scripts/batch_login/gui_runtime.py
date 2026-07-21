from __future__ import annotations

import asyncio
from pathlib import Path

import httpx
from playwright.async_api import async_playwright

from .browser_flows import BrowserFlows
from .credential_store import CredentialStore
from .enterprise_http import CurlCffiTransport, EnterpriseHttpClient
from .gui_controller import GuiController, GuiFormState
from .local_auth import IsolatedEnterpriseAuth, LocalMicrosoftAuth
from .local_checkpoint import LocalCheckpointStore
from .local_microsoft import MicrosoftProtocol
from .local_runner import LocalBatchRunner
from .models import AccountEntry, LoginMode
from .oidc_exporter import OidcCredentialExporter
from .password_vault import PasswordVault
from .proxy_chain import ProxyChain
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
        self.enterprise_transport: CurlCffiTransport | None = None
        self._enterprise = None
        self._microsoft = None
        self._microsoft_lock: asyncio.Lock | None = None

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

    async def _build_auth(self, proxy_chain):
        """构造 enterprise / microsoft 认证组件。run() 与并发登录路径共用。

        返回 (enterprise, microsoft)。微软号会启动共享 browser(self.browser)。
        """
        def emit_browser_event(raw_event):
            event = dict(raw_event)
            kind = str(event.pop("kind", "browser_event"))
            self.emit(WorkerEvent(kind, event))
        if self.form.mode is LoginMode.ENTERPRISE:
            vault_path = self.form.to_run_settings().password_vault_path
            self.emit(
                WorkerEvent(
                    "security_warning",
                    {
                        "message": (
                            "企业新密码会先使用 Windows DPAPI 加密并可靠保存到："
                            f"{vault_path}"
                        )
                    },
                )
            )
            vault = PasswordVault(vault_path)
            if proxy_chain is not None:
                self.emit(
                    WorkerEvent(
                        "security_warning",
                        {"message": "企业登录已启用链式代理：系统代理 → 家宽出口"},
                    )
                )
                enterprise_transport_factory = proxy_chain.transport_factory
            else:
                enterprise_transport_factory = (
                    lambda: CurlCffiTransport(timeout=self.form.timeout_seconds)
                )
            enterprise = IsolatedEnterpriseAuth(
                enterprise_transport_factory,
                lambda transport: EnterpriseHttpClient(
                    transport,
                    vault=vault,
                    event_sink=emit_browser_event,
                ),
            )
            return enterprise, None
        self.http = httpx.AsyncClient(timeout=30)
        self.playwright = await async_playwright().start()
        self.browser = await self.playwright.chromium.launch(
            headless=self.form.headless
        )
        browser_flows = BrowserFlows(
            self.browser,
            timeout_seconds=self.form.timeout_seconds,
            mfa_timeout_seconds=self.form.mfa_timeout_seconds,
            event_sink=emit_browser_event,
        )
        microsoft = LocalMicrosoftAuth(
            MicrosoftProtocol(self.http),
            browser_flows,
        )
        return None, microsoft

    def _api_key_transport_factory(self, proxy_chain):
        if not self.form.create_api_key:
            return None
        if proxy_chain is not None:
            return proxy_chain.transport_factory
        return lambda: CurlCffiTransport(timeout=self.form.timeout_seconds)

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
        proxy_chain = self._build_proxy_chain()
        enterprise, microsoft = await self._build_auth(proxy_chain)
        runner = LocalBatchRunner(
            enterprise=enterprise,
            microsoft=microsoft,
            store=store,
            checkpoint=checkpoint,
            importer=self.runner_importer(),
            emit=self.emit,
            api_key_transport_factory=self._api_key_transport_factory(proxy_chain),
        )
        return await runner.run(entries, self.form.to_run_settings())

    async def open_for_concurrent(self):
        """为并发登录做准备:构造 auth 组件(微软号会启动共享 browser)。

        返回该次登录要用的 EnterpriseSettings/MicrosoftSettings 依赖的 region 等,
        实际 settings 由调用方按每号 start_url 构造。仅初始化一次,之后 login_one 复用。
        """
        proxy_chain = self._build_proxy_chain()
        self._enterprise, self._microsoft = await self._build_auth(proxy_chain)
        # 微软号单浏览器:并发时用锁强制串行(企业号无锁,真并发)
        self._microsoft_lock = asyncio.Lock()
        return self._enterprise is not None

    async def login_one(self, entry: AccountEntry):
        """登录单个号,返回 CredentialRecord。企业号并发安全;微软号内部加锁串行。

        不写共享凭据文件(并发写会互相覆盖),直接返回凭据对象交调用方落库。
        """
        from .local_auth import EnterpriseSettings, MicrosoftSettings

        settings = self.form.to_run_settings()
        if self._enterprise is not None:
            return await self._enterprise.login(
                entry,
                EnterpriseSettings(
                    entry.start_url or settings.start_url or "",
                    settings.region,
                ),
            )
        async with self._microsoft_lock:
            return await self._microsoft.login(
                entry, MicrosoftSettings(settings.region)
            )

    def _build_proxy_chain(self) -> ProxyChain | None:
        if not getattr(self.form, "proxy_enabled", False):
            return None
        return ProxyChain.from_settings(
            system_proxy=self.form.system_proxy,
            home_proxies_text=self.form.home_proxies,
            timeout=self.form.timeout_seconds,
        )

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

    async def export_existing(self):
        records = CredentialStore(Path(self.form.credential_path)).load()
        if not records:
            raise ValueError("完整凭据 JSON 中没有可导出的账号")
        output_directory = self.form.oidc_output_dir()
        report = OidcCredentialExporter(
            warning_sink=lambda message: self.emit(
                WorkerEvent("security_warning", {"message": message})
            )
        ).export(
            records,
            output_directory=output_directory,
            mode=self.form.oidc_export_mode,
        )
        self.emit(
            WorkerEvent(
                "oidc_exported",
                {
                    "count": report.record_count,
                    "fileCount": (
                        int(report.merged_path is not None)
                        + len(report.account_paths)
                    ),
                    "directory": str(output_directory),
                },
            )
        )
        return report

    async def close(self) -> None:
        first_error: BaseException | None = None
        resources = (
            ("browser", "close"),
            ("playwright", "stop"),
            ("http", "aclose"),
            ("enterprise_transport", "close"),
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
