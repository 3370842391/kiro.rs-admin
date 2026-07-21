from __future__ import annotations

import asyncio
import base64
import struct
from dataclasses import dataclass
from urllib.parse import unquote, urlsplit

# 链式代理:curl_cffi → 本地 SOCKS5 中继 → 系统代理(7890) → 家宽(socks5) → 目标。
#
# 为什么要本地中继:libcurl 原生 PRE_PROXY 只能 SOCKS→HTTP 链式,SOCKS→SOCKS 会静默
# 丢弃第二跳(实测退出 IP 仍是系统代理)。本地中继自己在一条到系统代理的 TCP 上依次做
# 「系统代理 CONNECT 家宽入口 → 家宽 CONNECT 目标」两段握手,curl 的 Chrome-TLS 指纹
# 作为不透明字节全程穿透,不受影响。
#
# 每账号分配一个固定家宽出口(round-robin),同账号所有请求同 IP,像一个真实家宽用户。


class ProxyChainError(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class ProxyEndpoint:
    scheme: str  # socks5 | http
    host: str
    port: int
    username: str | None = None
    password: str | None = None

    @property
    def is_socks(self) -> bool:
        return self.scheme in {"socks5", "socks5h"}

    def display(self) -> str:
        auth = "***@" if self.username else ""
        return f"{self.scheme}://{auth}{self.host}:{self.port}"


def parse_proxy_url(url: str) -> ProxyEndpoint:
    """解析 socks5:// / socks5h:// / http:// 代理串;非法抛 ProxyChainError。"""
    raw = (url or "").strip()
    if not raw:
        raise ProxyChainError("代理地址为空")
    if "://" not in raw:
        raw = "socks5://" + raw  # 裸 host:port 视为 socks5
    parts = urlsplit(raw)
    scheme = (parts.scheme or "").lower()
    if scheme in {"socks5", "socks5h", "socks", "socksv5"}:
        scheme = "socks5"
    elif scheme in {"http", "https"}:
        scheme = "http"
    else:
        raise ProxyChainError(f"不支持的代理协议:{parts.scheme}")
    if not parts.hostname or not parts.port:
        raise ProxyChainError(f"代理地址缺少主机或端口:{url}")
    username = unquote(parts.username) if parts.username else None
    password = unquote(parts.password) if parts.password else None
    return ProxyEndpoint(
        scheme=scheme,
        host=parts.hostname,
        port=int(parts.port),
        username=username,
        password=password,
    )


def home_proxy_options(text: str) -> list[tuple[str, ProxyEndpoint]]:
    """按行解析,保留(原始行, 端点),用于下拉选出口;跳过空行/注释,去重。"""
    seen: set[str] = set()
    result: list[tuple[str, ProxyEndpoint]] = []
    for line in (text or "").splitlines():
        item = line.strip()
        if not item or item.startswith("#"):
            continue
        endpoint = parse_proxy_url(item)
        key = f"{endpoint.scheme}://{endpoint.username or ''}@{endpoint.host}:{endpoint.port}"
        if key in seen:
            continue
        seen.add(key)
        result.append((item, endpoint))
    return result


def parse_home_proxies(text: str) -> list[ProxyEndpoint]:
    """按行解析家宽代理清单;跳过空行/`#` 注释,去重(按 display)。"""
    return [endpoint for _line, endpoint in home_proxy_options(text)]


# ---- 纯字节构造 / 解析(便於单测,不碰网络)----


def socks5_greeting(with_userpass: bool) -> bytes:
    """客户端问候:总是提供 no-auth;需要时追加 username/password。"""
    if with_userpass:
        return b"\x05\x02\x00\x02"
    return b"\x05\x01\x00"


def socks5_userpass(username: str, password: str) -> bytes:
    user = username.encode("utf-8")
    pw = password.encode("utf-8")
    if len(user) > 255 or len(pw) > 255:
        raise ProxyChainError("代理用户名/密码过长")
    return b"\x01" + bytes([len(user)]) + user + bytes([len(pw)]) + pw


def socks5_connect_request(host: str, port: int) -> bytes:
    """CONNECT 请求,域名寻址(ATYP=3),把解析交给上游代理。"""
    host_bytes = host.encode("idna") if host else b""
    if len(host_bytes) > 255:
        raise ProxyChainError("目标域名过长")
    return (
        b"\x05\x01\x00\x03"
        + bytes([len(host_bytes)])
        + host_bytes
        + struct.pack("!H", port)
    )


async def _read_socks5_reply(reader: asyncio.StreamReader, *, stage: str) -> None:
    ver, rep, _rsv, atyp = await reader.readexactly(4)
    if ver != 5 or rep != 0:
        raise ProxyChainError(f"{stage} SOCKS5 CONNECT 失败(rep={rep})")
    if atyp == 1:
        await reader.readexactly(4)
    elif atyp == 3:
        length = (await reader.readexactly(1))[0]
        await reader.readexactly(length)
    elif atyp == 4:
        await reader.readexactly(16)
    else:
        raise ProxyChainError(f"{stage} SOCKS5 回复地址类型无效")
    await reader.readexactly(2)  # bound port


async def _socks5_handshake(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    proxy: ProxyEndpoint,
    dst_host: str,
    dst_port: int,
    *,
    stage: str,
) -> None:
    want_userpass = bool(proxy.username)
    writer.write(socks5_greeting(want_userpass))
    await writer.drain()
    ver, method = await reader.readexactly(2)
    if ver != 5:
        raise ProxyChainError(f"{stage} 不是 SOCKS5 代理")
    if method == 2:
        if not proxy.username:
            raise ProxyChainError(f"{stage} 需要用户名密码但未配置")
        writer.write(socks5_userpass(proxy.username, proxy.password or ""))
        await writer.drain()
        _av, status = await reader.readexactly(2)
        if status != 0:
            raise ProxyChainError(f"{stage} 代理认证被拒绝")
    elif method == 0xFF:
        raise ProxyChainError(f"{stage} 代理拒绝所有认证方式")
    elif method != 0:
        raise ProxyChainError(f"{stage} 代理要求不支持的认证方式({method})")
    writer.write(socks5_connect_request(dst_host, dst_port))
    await writer.drain()
    await _read_socks5_reply(reader, stage=stage)


async def _http_connect_handshake(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    proxy: ProxyEndpoint,
    dst_host: str,
    dst_port: int,
    *,
    stage: str,
) -> None:
    target = f"{dst_host}:{dst_port}"
    lines = [f"CONNECT {target} HTTP/1.1", f"Host: {target}"]
    if proxy.username:
        token = base64.b64encode(
            f"{proxy.username}:{proxy.password or ''}".encode("utf-8")
        ).decode("ascii")
        lines.append(f"Proxy-Authorization: Basic {token}")
    request = ("\r\n".join(lines) + "\r\n\r\n").encode("ascii")
    writer.write(request)
    await writer.drain()
    status_line = await reader.readline()
    if not status_line:
        raise ProxyChainError(f"{stage} HTTP 代理无响应")
    fields = status_line.decode("latin-1").split(" ", 2)
    if len(fields) < 2 or not fields[1].startswith("2"):
        raise ProxyChainError(f"{stage} HTTP CONNECT 失败:{status_line.decode('latin-1').strip()}")
    # 丢弃剩余响应头
    while True:
        header = await reader.readline()
        if header in (b"\r\n", b"\n", b""):
            break


async def _connect_through(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    proxy: ProxyEndpoint,
    dst_host: str,
    dst_port: int,
    *,
    stage: str,
) -> None:
    if proxy.is_socks:
        await _socks5_handshake(reader, writer, proxy, dst_host, dst_port, stage=stage)
    else:
        await _http_connect_handshake(reader, writer, proxy, dst_host, dst_port, stage=stage)


async def _read_client_target(
    reader: asyncio.StreamReader, writer: asyncio.StreamWriter
) -> tuple[str, int]:
    """处理 curl 侧的 SOCKS5 问候(no-auth)+ CONNECT,返回目标 host:port。"""
    ver, nmethods = await reader.readexactly(2)
    if ver != 5:
        raise ProxyChainError("本地客户端不是 SOCKS5")
    await reader.readexactly(nmethods)
    # 对 curl 只提供 no-auth(本地环回,无需认证);必须先回方法选择,curl 才会发 CONNECT
    writer.write(b"\x05\x00")
    await writer.drain()
    hdr = await reader.readexactly(4)
    _ver, cmd, _rsv, atyp = hdr
    if cmd != 1:
        raise ProxyChainError("仅支持 CONNECT")
    if atyp == 1:
        host = ".".join(str(b) for b in await reader.readexactly(4))
    elif atyp == 3:
        length = (await reader.readexactly(1))[0]
        raw_host = await reader.readexactly(length)
        try:
            host = raw_host.decode("idna")
        except (UnicodeError, ValueError):
            host = raw_host.decode("latin-1")
    elif atyp == 4:
        raw = await reader.readexactly(16)
        host = ":".join(f"{raw[i]:02x}{raw[i + 1]:02x}" for i in range(0, 16, 2))
    else:
        raise ProxyChainError("客户端地址类型无效")
    port = struct.unpack("!H", await reader.readexactly(2))[0]
    return host, port


async def _pipe(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    try:
        while True:
            data = await reader.read(65536)
            if not data:
                break
            writer.write(data)
            await writer.drain()
    except Exception:  # noqa: BLE001 - 断流即收尾
        pass
    finally:
        try:
            writer.close()
        except Exception:  # noqa: BLE001
            pass


class ProxyRelay:
    """本机 127.0.0.1 上的 SOCKS5 中继:每连接链式 系统代理→家宽→目标。

    system 为 None 时直连家宽(不在墙内的场景)。connector 可注入(测试用)。
    """

    def __init__(
        self,
        *,
        system: ProxyEndpoint | None,
        home: ProxyEndpoint,
        connector=asyncio.open_connection,
    ):
        self.system = system
        self.home = home
        self._connector = connector
        self._server: asyncio.AbstractServer | None = None
        self.port = 0

    async def start(self) -> int:
        self._server = await asyncio.start_server(
            self._handle_client, "127.0.0.1", 0
        )
        self.port = self._server.sockets[0].getsockname()[1]
        return self.port

    @property
    def local_url(self) -> str:
        return f"socks5://127.0.0.1:{self.port}"

    async def _handle_client(
        self, c_reader: asyncio.StreamReader, c_writer: asyncio.StreamWriter
    ) -> None:
        u_writer: asyncio.StreamWriter | None = None
        try:
            host, port = await _read_client_target(c_reader, c_writer)
            u_reader, u_writer = await self._open_chain(host, port)
            c_writer.write(b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00")
            await c_writer.drain()
            await asyncio.gather(
                _pipe(c_reader, u_writer),
                _pipe(u_reader, c_writer),
            )
        except Exception:  # noqa: BLE001 - 单连接失败不影响中继
            try:
                # 尽力回一个失败响应(若还没回过)
                c_writer.write(b"\x05\x01\x00\x01\x00\x00\x00\x00\x00\x00")
                await c_writer.drain()
            except Exception:  # noqa: BLE001
                pass
            try:
                c_writer.close()
            except Exception:  # noqa: BLE001
                pass
            if u_writer is not None:
                try:
                    u_writer.close()
                except Exception:  # noqa: BLE001
                    pass

    async def _open_chain(
        self, dst_host: str, dst_port: int
    ) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        if self.system is not None:
            reader, writer = await self._connector(self.system.host, self.system.port)
            # 第一跳:系统代理 CONNECT 家宽入口
            await _connect_through(
                reader, writer, self.system, self.home.host, self.home.port,
                stage="系统代理",
            )
            # 第二跳:在同一隧道上,家宽 CONNECT 目标
            await _connect_through(
                reader, writer, self.home, dst_host, dst_port, stage="家宽",
            )
            return reader, writer
        # 无系统代理:直连家宽
        reader, writer = await self._connector(self.home.host, self.home.port)
        await _connect_through(
            reader, writer, self.home, dst_host, dst_port, stage="家宽",
        )
        return reader, writer

    async def stop(self) -> None:
        server = self._server
        self._server = None
        if server is not None:
            server.close()
            try:
                await server.wait_closed()
            except Exception:  # noqa: BLE001
                pass


class ChainedTransport:
    """懒启动的代理链式 transport,接口对齐 CurlCffiTransport(request/close/cookies)。"""

    def __init__(
        self,
        *,
        system: ProxyEndpoint | None,
        home: ProxyEndpoint,
        timeout: float = 45,
        transport_factory=None,
        relay_factory=None,
    ):
        self.system = system
        self.home = home
        self.timeout = timeout
        self._transport_factory = transport_factory
        self._relay_factory = relay_factory or (
            lambda: ProxyRelay(system=system, home=home)
        )
        self._relay: ProxyRelay | None = None
        self._transport = None

    async def _ensure(self):
        if self._transport is not None:
            return self._transport
        self._relay = self._relay_factory()
        await self._relay.start()
        if self._transport_factory is not None:
            self._transport = self._transport_factory(self._relay.local_url)
        else:
            from .enterprise_http import CurlCffiTransport

            self._transport = CurlCffiTransport(
                timeout=self.timeout, proxy=self._relay.local_url
            )
        return self._transport

    @property
    def cookies(self):
        return self._transport.cookies if self._transport is not None else None

    async def request(self, method: str, url: str, **kwargs):
        transport = await self._ensure()
        return await transport.request(method, url, **kwargs)

    async def close(self) -> None:
        first_error: BaseException | None = None
        if self._transport is not None:
            try:
                await self._transport.close()
            except BaseException as error:  # noqa: BLE001
                first_error = error
            self._transport = None
        if self._relay is not None:
            try:
                await self._relay.stop()
            except BaseException as error:  # noqa: BLE001
                if first_error is None:
                    first_error = error
            self._relay = None
        if first_error is not None:
            raise first_error


class ProxyChain:
    """系统代理 + 家宽池;transport_factory() 每次 round-robin 取下一个家宽出口。"""

    def __init__(
        self,
        *,
        system: ProxyEndpoint | None,
        homes: list[ProxyEndpoint],
        timeout: float = 45,
        transport_factory=None,
        relay_factory=None,
    ):
        if not homes:
            raise ProxyChainError("家宽代理池为空")
        self.system = system
        self.homes = list(homes)
        self.timeout = timeout
        self._transport_factory = transport_factory
        self._relay_factory = relay_factory
        self._cursor = 0

    def _next_home(self) -> ProxyEndpoint:
        home = self.homes[self._cursor % len(self.homes)]
        self._cursor += 1
        return home

    def transport_factory(self):
        home = self._next_home()
        relay_factory = None
        if self._relay_factory is not None:
            relay_factory = lambda: self._relay_factory(self.system, home)  # noqa: E731
        return ChainedTransport(
            system=self.system,
            home=home,
            timeout=self.timeout,
            transport_factory=self._transport_factory,
            relay_factory=relay_factory,
        )

    @classmethod
    def from_settings(
        cls,
        *,
        system_proxy: str,
        home_proxies_text: str,
        timeout: float = 45,
    ) -> "ProxyChain | None":
        """启用时从设置构建;家宽为空返回 None(视为未启用)。"""
        homes = parse_home_proxies(home_proxies_text)
        if not homes:
            return None
        system = None
        if (system_proxy or "").strip():
            system = parse_proxy_url(system_proxy)
        return cls(system=system, homes=homes, timeout=timeout)
