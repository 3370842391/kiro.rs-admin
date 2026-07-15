from __future__ import annotations

import asyncio
import queue
import threading
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlsplit

from .input_parser import compile_format
from .models import AccountEntry, LoginMode
from .redaction import redact_text
from .worker_events import (
    LocalRunSettings,
    ResultMode,
    WorkerEvent,
)


@dataclass(slots=True)
class GuiFormState:
    mode: LoginMode = LoginMode.ENTERPRISE
    input_template: str = "login = {account} / onetime password = {password}"
    output_template: str = "{account}----{password}"
    start_url: str = ""
    region: str = "us-east-1"
    headless: bool = False
    timeout_seconds: float = 180
    mfa_timeout_seconds: float = 300
    result_mode: ResultMode = ResultMode.SAVE_ONLY
    input_path: str = ""
    credential_path: str = ""
    checkpoint_path: str = ""
    password_vault_path: str = ""
    resume: bool = False
    rs_url: str = ""
    admin_key: str = ""
    use_ssh: bool = False
    ssh_host: str = ""
    ssh_user: str = ""
    ssh_port: int = 22
    identity_file: str = ""
    remote_host: str = "127.0.0.1"
    remote_port: int = 8990
    local_port: int | None = None

    def validate(self) -> list[str]:
        errors: list[str] = []
        try:
            compile_format(self.input_template)
            compile_format(self.output_template)
        except ValueError as error:
            errors.append(str(error))
        if self.mode is LoginMode.ENTERPRISE and not self.start_url.strip():
            errors.append("企业模式必须填写 Start URL")
        if not self.region.strip():
            errors.append("Region 不能为空")
        if self.timeout_seconds <= 0 or self.mfa_timeout_seconds <= 0:
            errors.append("超时必须大于 0")
        if not self.credential_path.strip():
            errors.append("必须选择完整凭据 JSON 路径")
        if self._paths_collide():
            errors.append("完整凭据 JSON 不能覆盖账号输入文件")
        if self._vault_overwrites_credentials():
            errors.append("密码保险库不能覆盖完整凭据 JSON")
        if self.result_mode is ResultMode.SAVE_AND_IMPORT:
            self._validate_import(errors)
        return errors

    def _paths_collide(self) -> bool:
        if not self.input_path.strip() or not self.credential_path.strip():
            return False
        return Path(self.input_path).resolve() == Path(
            self.credential_path
        ).resolve()

    def _vault_overwrites_credentials(self) -> bool:
        if not self.credential_path.strip():
            return False
        vault = self.password_vault_path.strip() or (
            self.credential_path.strip() + ".passwords.sqlite3"
        )
        return Path(vault).resolve() == Path(self.credential_path).resolve()

    def _validate_import(self, errors: list[str]) -> None:
        if not self.admin_key.strip():
            errors.append("导入 RS 必须填写 Admin Key")
        if self.use_ssh:
            if not self.ssh_host.strip() or not self.ssh_user.strip():
                errors.append("SSH 模式必须填写主机和用户")
            return
        if not self.rs_url.strip():
            errors.append("直接连接必须填写 RS URL")
            return
        try:
            parts = urlsplit(self.rs_url.strip())
            _ = parts.port
        except ValueError:
            errors.append("RS URL 无效")
            return
        if parts.scheme not in {"http", "https"} or not parts.hostname:
            errors.append("RS URL 必须是 HTTP(S) 地址")
            return
        if (
            parts.scheme == "http"
            and parts.hostname not in {"127.0.0.1", "::1", "localhost"}
        ):
            errors.append("远程 RS 必须使用 HTTPS")

    def to_run_settings(self) -> LocalRunSettings:
        checkpoint = self.checkpoint_path.strip() or (
            self.credential_path.strip() + ".checkpoint.jsonl"
        )
        password_vault = self.password_vault_path.strip() or (
            self.credential_path.strip() + ".passwords.sqlite3"
        )
        return LocalRunSettings(
            mode=self.mode,
            region=self.region.strip(),
            start_url=self.start_url.strip() or None,
            headless=self.headless,
            timeout_seconds=self.timeout_seconds,
            mfa_timeout_seconds=self.mfa_timeout_seconds,
            result_mode=self.result_mode,
            credential_path=Path(self.credential_path),
            checkpoint_path=Path(checkpoint),
            password_vault_path=Path(password_vault),
            resume=self.resume,
        )


class GuiController:
    def __init__(self, runtime_factory):
        self.runtime_factory = runtime_factory
        self.events: queue.Queue[WorkerEvent] = queue.Queue()
        self.thread: threading.Thread | None = None
        self.loop: asyncio.AbstractEventLoop | None = None
        self.task: asyncio.Task | None = None

    def start(
        self,
        entries: list[AccountEntry],
        form: GuiFormState,
    ) -> None:
        self._validate_start(form)
        self._start_thread("run", entries, form)

    def import_existing(self, form: GuiFormState) -> None:
        self._validate_start(form)
        if form.result_mode is not ResultMode.SAVE_AND_IMPORT:
            raise ValueError("导入已有 JSON 必须选择保存并导入 RS")
        self._start_thread("import", [], form)

    def _validate_start(self, form: GuiFormState) -> None:
        errors = form.validate()
        if errors:
            raise ValueError("\n".join(errors))
        if self.thread is not None and self.thread.is_alive():
            raise RuntimeError("已有任务正在运行")

    def _start_thread(
        self,
        action: str,
        entries: list[AccountEntry],
        form: GuiFormState,
    ) -> None:
        self.thread = threading.Thread(
            target=self._thread_main,
            args=(action, entries, form),
            daemon=False,
            name=(
                "kiro-batch-login-worker"
                if action == "run"
                else "kiro-batch-import-worker"
            ),
        )
        self.thread.start()

    def _thread_main(
        self,
        action: str,
        entries: list[AccountEntry],
        form: GuiFormState,
    ) -> None:
        loop = asyncio.new_event_loop()
        self.loop = loop
        asyncio.set_event_loop(loop)
        runtime = None
        try:
            runtime = self.runtime_factory(form, self.events.put)
            coroutine = (
                runtime.run(entries)
                if action == "run"
                else runtime.import_existing()
            )
            self.task = loop.create_task(coroutine)
            loop.run_until_complete(self.task)
        except asyncio.CancelledError:
            pass
        except Exception as error:
            self.events.put(
                WorkerEvent(
                    "fatal_error",
                    {
                        "code": "runtime_failed",
                        "message": redact_text(str(error)),
                    },
                )
            )
        finally:
            if runtime is not None:
                try:
                    loop.run_until_complete(runtime.close())
                except Exception as error:
                    self.events.put(
                        WorkerEvent(
                            "fatal_error",
                            {
                                "code": "runtime_close_failed",
                                "message": redact_text(str(error)),
                            },
                        )
                    )
            loop.run_until_complete(loop.shutdown_asyncgens())
            loop.close()
            self.task = None
            self.loop = None

    def cancel(self) -> None:
        loop, task = self.loop, self.task
        if loop is not None and task is not None and not task.done():
            loop.call_soon_threadsafe(task.cancel)

    def drain_events(self) -> list[WorkerEvent]:
        items: list[WorkerEvent] = []
        while True:
            try:
                items.append(self.events.get_nowait())
            except queue.Empty:
                return items
