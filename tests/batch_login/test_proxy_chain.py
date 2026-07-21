import asyncio
import struct
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.proxy_chain import (
    ChainedTransport,
    ProxyChain,
    ProxyChainError,
    ProxyEndpoint,
    ProxyRelay,
    parse_home_proxies,
    parse_proxy_url,
    socks5_connect_request,
    socks5_greeting,
    socks5_userpass,
)


class ParseTests(unittest.TestCase):
    def test_socks5_with_auth(self):
        ep = parse_proxy_url("socks5://user:p%40ss@1.2.3.4:1080")
        self.assertEqual("socks5", ep.scheme)
        self.assertEqual("1.2.3.4", ep.host)
        self.assertEqual(1080, ep.port)
        self.assertEqual("user", ep.username)
        self.assertEqual("p@ss", ep.password)  # url-decoded
        self.assertTrue(ep.is_socks)

    def test_bare_hostport_defaults_socks5(self):
        ep = parse_proxy_url("127.0.0.1:7890")
        self.assertEqual("socks5", ep.scheme)
        self.assertEqual(7890, ep.port)

    def test_http_scheme(self):
        ep = parse_proxy_url("http://10.0.0.1:8080")
        self.assertEqual("http", ep.scheme)
        self.assertFalse(ep.is_socks)

    def test_invalid_scheme_and_missing_port(self):
        with self.assertRaises(ProxyChainError):
            parse_proxy_url("ftp://x:1")
        with self.assertRaises(ProxyChainError):
            parse_proxy_url("socks5://host-without-port")

    def test_home_proxy_options_keeps_raw_line_and_endpoint(self):
        from batch_login.proxy_chain import home_proxy_options

        options = home_proxy_options(
            "socks5://u:p@1.1.1.1:1080\n# c\nsocks5://u:p@1.1.1.1:1080\nhttp://2.2.2.2:8080"
        )
        self.assertEqual(2, len(options))
        self.assertEqual("socks5://u:p@1.1.1.1:1080", options[0][0])  # raw line kept
        self.assertEqual("1.1.1.1", options[0][1].host)
        self.assertEqual("http", options[1][1].scheme)

    def test_parse_home_proxies_skips_comments_and_dedupes(self):
        text = """
        socks5://a:b@1.1.1.1:1080
        # comment
        socks5://a:b@1.1.1.1:1080
        socks5://c:d@2.2.2.2:1080
        """
        homes = parse_home_proxies(text)
        self.assertEqual(2, len(homes))
        self.assertEqual("1.1.1.1", homes[0].host)
        self.assertEqual("2.2.2.2", homes[1].host)


class ByteBuilderTests(unittest.TestCase):
    def test_greeting(self):
        self.assertEqual(b"\x05\x01\x00", socks5_greeting(False))
        self.assertEqual(b"\x05\x02\x00\x02", socks5_greeting(True))

    def test_userpass(self):
        self.assertEqual(b"\x01\x02ab\x02cd", socks5_userpass("ab", "cd"))

    def test_connect_request_domain(self):
        req = socks5_connect_request("example.com", 443)
        self.assertEqual(b"\x05\x01\x00\x03", req[:4])
        self.assertEqual(len("example.com"), req[4])
        self.assertEqual(b"example.com", req[5:5 + 11])
        self.assertEqual(443, struct.unpack("!H", req[-2:])[0])


class RoundRobinTests(unittest.TestCase):
    def test_transport_factory_cycles_homes(self):
        homes = [
            ProxyEndpoint("socks5", "1.1.1.1", 1080),
            ProxyEndpoint("socks5", "2.2.2.2", 1080),
        ]
        picked = []
        chain = ProxyChain(
            system=None,
            homes=homes,
            relay_factory=lambda system, home: picked.append(home) or _NoopRelay(),
            transport_factory=lambda url: _NoopTransport(),
        )
        # 3 accounts -> homes cycle 0,1,0
        for _ in range(3):
            t = chain.transport_factory()
            asyncio.run(t._ensure())
        self.assertEqual(["1.1.1.1", "2.2.2.2", "1.1.1.1"], [h.host for h in picked])

    def test_empty_homes_raises(self):
        with self.assertRaises(ProxyChainError):
            ProxyChain(system=None, homes=[])

    def test_from_settings_none_when_no_homes(self):
        self.assertIsNone(
            ProxyChain.from_settings(system_proxy="socks5://127.0.0.1:7890", home_proxies_text="")
        )


class _NoopRelay:
    port = 1
    local_url = "socks5://127.0.0.1:1"

    async def start(self):
        return self.port

    async def stop(self):
        pass


class _NoopTransport:
    cookies = None

    async def request(self, *a, **k):
        raise AssertionError("not used")

    async def close(self):
        pass


# ---- 进程内假 SOCKS5 上游,端到端验证链式逻辑(不碰真网络) ----


class FakeSocks5Server:
    """最小 SOCKS5 CONNECT 服务器,把目标连接转成回显/记录。"""

    def __init__(self, *, require_userpass=None, next_hop=None, label=""):
        self.require_userpass = require_userpass  # (user, pass) or None
        self.next_hop = next_hop  # (host, port) it should have been asked to reach
        self.label = label
        self.seen_target = None
        self.seen_auth = None
        self.server = None
        self.port = 0

    async def start(self):
        self.server = await asyncio.start_server(self._handle, "127.0.0.1", 0)
        self.port = self.server.sockets[0].getsockname()[1]
        return self.port

    async def stop(self):
        if self.server:
            self.server.close()
            await self.server.wait_closed()

    async def _handle(self, reader, writer):
        try:
            ver, n = await reader.readexactly(2)
            methods = await reader.readexactly(n)
            if self.require_userpass:
                writer.write(b"\x05\x02")
                await writer.drain()
                await reader.readexactly(1)  # auth ver
                ulen = (await reader.readexactly(1))[0]
                user = (await reader.readexactly(ulen)).decode()
                plen = (await reader.readexactly(1))[0]
                pw = (await reader.readexactly(plen)).decode()
                self.seen_auth = (user, pw)
                ok = (user, pw) == self.require_userpass
                writer.write(b"\x01" + (b"\x00" if ok else b"\x01"))
                await writer.drain()
                if not ok:
                    writer.close()
                    return
            else:
                writer.write(b"\x05\x00")
                await writer.drain()
            ver, cmd, rsv, atyp = await reader.readexactly(4)
            if atyp == 1:
                host = ".".join(str(b) for b in await reader.readexactly(4))
            elif atyp == 3:
                ln = (await reader.readexactly(1))[0]
                host = (await reader.readexactly(ln)).decode()
            else:
                await reader.readexactly(16)
                host = "ipv6"
            port = struct.unpack("!H", await reader.readexactly(2))[0]
            self.seen_target = (host, port)
            writer.write(b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00")
            await writer.drain()

            if self.next_hop is not None:
                # 充当中间代理:连到下一跳并双向转发
                nr, nw = await asyncio.open_connection(*self.next_hop)

                async def pipe(r, w):
                    try:
                        while True:
                            data = await r.read(65536)
                            if not data:
                                break
                            w.write(data)
                            await w.drain()
                    except Exception:
                        pass
                    finally:
                        try:
                            w.close()
                        except Exception:
                            pass

                await asyncio.gather(pipe(reader, nw), pipe(nr, writer))
            else:
                # 终端:回显收到的字节(用于验证隧道打通)
                while True:
                    data = await reader.read(65536)
                    if not data:
                        break
                    writer.write(b"ECHO:" + data)
                    await writer.drain()
        except Exception:
            try:
                writer.close()
            except Exception:
                pass


class TargetEchoServer:
    def __init__(self):
        self.server = None
        self.port = 0
        self.received = b""

    async def start(self):
        self.server = await asyncio.start_server(self._handle, "127.0.0.1", 0)
        self.port = self.server.sockets[0].getsockname()[1]
        return self.port

    async def stop(self):
        if self.server:
            self.server.close()
            await self.server.wait_closed()

    async def _handle(self, reader, writer):
        data = await reader.read(1024)
        self.received = data
        writer.write(b"PONG:" + data)
        await writer.drain()
        writer.close()


class ChainIntegrationTests(unittest.IsolatedAsyncioTestCase):
    async def test_relay_chains_system_then_home_to_target(self):
        target = TargetEchoServer()
        await target.start()
        # home proxy requires auth, connects to target
        home = FakeSocks5Server(
            require_userpass=("huser", "hpass"),
            next_hop=("127.0.0.1", target.port),
            label="home",
        )
        await home.start()
        # system proxy no auth, connects to home ingress
        system = FakeSocks5Server(next_hop=("127.0.0.1", home.port), label="system")
        await system.start()

        relay = ProxyRelay(
            system=ProxyEndpoint("socks5", "127.0.0.1", system.port),
            home=ProxyEndpoint("socks5", "127.0.0.1", home.port, "huser", "hpass"),
        )
        await relay.start()
        try:
            # act as curl: SOCKS5 to relay, CONNECT to target host:port
            reader, writer = await asyncio.open_connection("127.0.0.1", relay.port)
            writer.write(b"\x05\x01\x00")
            await writer.drain()
            self.assertEqual(b"\x05\x00", await reader.readexactly(2))
            writer.write(socks5_connect_request("final.example", target.port))
            await writer.drain()
            rep = await reader.readexactly(10)
            self.assertEqual(0, rep[1])  # success
            writer.write(b"hello")
            await writer.drain()
            echoed = await reader.read(64)
            writer.close()
        finally:
            await relay.stop()
            await system.stop()
            await home.stop()
            await target.stop()

        # system saw it was asked to reach home ingress
        self.assertEqual(("127.0.0.1", home.port), system.seen_target)
        # home authed and was asked to reach the final target
        self.assertEqual(("huser", "hpass"), home.seen_auth)
        self.assertEqual(("final.example", target.port), home.seen_target)
        # target received our bytes (through both hops)
        self.assertEqual(b"hello", target.received)
        self.assertIn(b"PONG:hello", echoed)

    async def test_relay_direct_home_when_no_system(self):
        target = TargetEchoServer()
        await target.start()
        home = FakeSocks5Server(next_hop=("127.0.0.1", target.port))
        await home.start()

        relay = ProxyRelay(
            system=None,
            home=ProxyEndpoint("socks5", "127.0.0.1", home.port),
        )
        await relay.start()
        try:
            reader, writer = await asyncio.open_connection("127.0.0.1", relay.port)
            writer.write(b"\x05\x01\x00")
            await writer.drain()
            await reader.readexactly(2)
            writer.write(socks5_connect_request("t.example", target.port))
            await writer.drain()
            rep = await reader.readexactly(10)
            self.assertEqual(0, rep[1])
            writer.write(b"ping")
            await writer.drain()
            await reader.read(64)
            writer.close()
        finally:
            await relay.stop()
            await home.stop()
            await target.stop()

        self.assertEqual(("t.example", target.port), home.seen_target)
        self.assertEqual(b"ping", target.received)

    async def test_chained_transport_close_stops_relay(self):
        started = {"relay": 0}

        class Relay:
            local_url = "socks5://127.0.0.1:1"

            async def start(self):
                started["relay"] += 1

            async def stop(self):
                started["stopped"] = True

        class Transport:
            cookies = None

            def __init__(self):
                self.closed = False

            async def request(self, *a, **k):
                return "resp"

            async def close(self):
                self.closed = True

        t = ChainedTransport(
            system=None,
            home=ProxyEndpoint("socks5", "h", 1),
            transport_factory=lambda url: Transport(),
            relay_factory=lambda: Relay(),
        )
        out = await t.request("GET", "http://x")
        self.assertEqual("resp", out)
        self.assertEqual(1, started["relay"])
        await t.close()
        self.assertTrue(started.get("stopped"))


if __name__ == "__main__":
    unittest.main()
