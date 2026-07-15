from __future__ import annotations

import asyncio
import os
import socket
import subprocess
from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class SshTunnelSettings:
    host: str
    user: str
    ssh_port: int = 22
    remote_host: str = "127.0.0.1"
    remote_port: int = 8990
    local_port: int | None = None
    identity_file: str | None = None

    def __post_init__(self) -> None:
        if not self.host.strip():
            raise ValueError("SSH host 不能为空")
        if not self.user.strip():
            raise ValueError("SSH user 不能为空")
        if not self.remote_host.strip():
            raise ValueError("远端 host 不能为空")
        _validate_port("ssh_port", self.ssh_port)
        _validate_port("remote_port", self.remote_port)
        if self.local_port is not None:
            _validate_port("local_port", self.local_port)


def _validate_port(name: str, port: int) -> None:
    if isinstance(port, bool) or not isinstance(port, int) or not 1 <= port <= 65535:
        raise ValueError(f"{name} 必须是 1 到 65535 之间的整数")


def build_ssh_command(settings: SshTunnelSettings, local_port: int | None = None) -> list[str]:
    selected_port = settings.local_port if local_port is None else local_port
    if selected_port is None:
        raise ValueError("必须提供本地端口")
    _validate_port("local_port", selected_port)

    command = [
        "ssh",
        "-N",
        "-L",
        f"127.0.0.1:{selected_port}:{settings.remote_host}:{settings.remote_port}",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "BatchMode=yes",
        "-p",
        str(settings.ssh_port),
    ]
    if settings.identity_file is not None:
        command.extend(["-i", settings.identity_file])
    command.append(f"{settings.user}@{settings.host}")
    return command


def choose_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


async def probe_port(port: int, timeout: float = 0.2) -> bool:
    _validate_port("port", port)
    try:
        _reader, writer = await asyncio.wait_for(
            asyncio.open_connection("127.0.0.1", port),
            timeout=timeout,
        )
    except (OSError, TimeoutError):
        return False

    writer.close()
    try:
        await writer.wait_closed()
    except OSError:
        pass
    return True


class SshTunnel:
    def __init__(
        self,
        settings: SshTunnelSettings,
        *,
        ready_timeout: float = 10.0,
        probe_interval: float = 0.1,
    ) -> None:
        self.settings = settings
        self.ready_timeout = ready_timeout
        self.probe_interval = probe_interval
        self._process: asyncio.subprocess.Process | None = None
        self._local_port: int | None = None

    async def start(self) -> str:
        if self._process is not None and self._process.returncode is None:
            return self._url()

        automatic_port = self.settings.local_port is None
        attempts = 3 if automatic_port else 1
        for _attempt in range(attempts):
            local_port = choose_port() if automatic_port else self.settings.local_port
            assert local_port is not None
            try:
                process = await asyncio.create_subprocess_exec(
                    *build_ssh_command(self.settings, local_port),
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
                )
            except (OSError, subprocess.SubprocessError):
                if not automatic_port or _attempt == attempts - 1:
                    raise RuntimeError("SSH 隧道启动失败") from None
                continue

            self._process = process
            self._local_port = local_port
            if await self._wait_until_ready(process, local_port):
                return self._url()

            await self.stop()
            if not automatic_port or _attempt == attempts - 1:
                raise RuntimeError("SSH 隧道在就绪前退出")

        raise RuntimeError("SSH 隧道启动失败")

    async def _wait_until_ready(self, process: asyncio.subprocess.Process, port: int) -> bool:
        loop = asyncio.get_running_loop()
        deadline = loop.time() + self.ready_timeout
        while process.returncode is None:
            if await probe_port(port):
                return True
            if process.returncode is not None or loop.time() >= deadline:
                return False
            await asyncio.sleep(self.probe_interval)
        return False

    async def stop(self) -> None:
        process = self._process
        self._process = None
        self._local_port = None
        if process is None or process.returncode is not None:
            return

        try:
            process.terminate()
        except ProcessLookupError:
            return
        try:
            await asyncio.wait_for(process.wait(), timeout=3.0)
        except TimeoutError:
            try:
                process.kill()
            except ProcessLookupError:
                return
            await process.wait()

    def _url(self) -> str:
        if self._local_port is None:
            raise RuntimeError("SSH 隧道尚未启动")
        return f"http://127.0.0.1:{self._local_port}"
