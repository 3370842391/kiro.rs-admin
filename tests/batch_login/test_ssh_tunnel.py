import asyncio
import os
import socket
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.ssh_tunnel import (
    SshTunnel,
    SshTunnelSettings,
    build_ssh_command,
    choose_port,
    probe_port,
)


class FakeProcess:
    def __init__(self, returncode=None):
        self.returncode = returncode
        self.terminate_calls = 0
        self.kill_calls = 0
        self.wait_calls = 0

    def terminate(self):
        self.terminate_calls += 1

    def kill(self):
        self.kill_calls += 1
        self.returncode = -9

    async def wait(self):
        self.wait_calls += 1
        if self.returncode is None:
            self.returncode = 0
        return self.returncode


class HangingProcess(FakeProcess):
    def wait(self):
        self.wait_calls += 1

        async def wait_for_exit():
            if self.wait_calls == 1:
                await asyncio.Future()
            return self.returncode

        return wait_for_exit()


class SshTunnelSettingsTests(unittest.TestCase):
    def test_build_command_uses_secure_argument_list_and_optional_identity(self):
        settings = SshTunnelSettings(
            host="ssh.example.com",
            user="alice",
            ssh_port=2222,
            remote_host="10.0.0.5",
            remote_port=9000,
            identity_file=r"C:\keys\private key.pem",
        )

        command = build_ssh_command(settings, 4567)

        self.assertEqual(
            [
                "ssh",
                "-N",
                "-L",
                "127.0.0.1:4567:10.0.0.5:9000",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "BatchMode=yes",
                "-p",
                "2222",
                "-i",
                r"C:\keys\private key.pem",
                "alice@ssh.example.com",
            ],
            command,
        )

    def test_build_command_omits_identity_when_not_configured(self):
        settings = SshTunnelSettings(host="ssh.example.com", user="alice")

        command = build_ssh_command(settings, 4567)

        self.assertNotIn("-i", command)

    def test_settings_reject_blank_hosts_users_and_invalid_ports(self):
        invalid_arguments = [
            {"host": "", "user": "alice"},
            {"host": "ssh.example.com", "user": "   "},
            {"host": "ssh.example.com", "user": "alice", "remote_host": ""},
            {"host": "ssh.example.com", "user": "alice", "ssh_port": 0},
            {"host": "ssh.example.com", "user": "alice", "remote_port": 65536},
            {"host": "ssh.example.com", "user": "alice", "local_port": -1},
        ]

        for arguments in invalid_arguments:
            with self.subTest(arguments=arguments):
                with self.assertRaises(ValueError):
                    SshTunnelSettings(**arguments)

        settings = SshTunnelSettings(host="ssh.example.com", user="alice")
        with self.assertRaises(ValueError):
            build_ssh_command(settings, 70000)

    def test_choose_port_returns_a_rebindable_loopback_port(self):
        port = choose_port()

        self.assertGreater(port, 0)
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.bind(("127.0.0.1", port))


class PortProbeTests(unittest.IsolatedAsyncioTestCase):
    async def test_probe_port_detects_open_and_closed_loopback_ports(self):
        server = await asyncio.start_server(lambda _reader, writer: writer.close(), "127.0.0.1", 0)
        port = server.sockets[0].getsockname()[1]
        try:
            self.assertTrue(await probe_port(port, timeout=0.5))
        finally:
            server.close()
            await server.wait_closed()

        self.assertFalse(await probe_port(port, timeout=0.05))


class SshTunnelLifecycleTests(unittest.IsolatedAsyncioTestCase):
    async def test_start_uses_create_subprocess_exec_without_shell_and_returns_loopback_url(self):
        settings = SshTunnelSettings(host="ssh.example.com", user="alice", local_port=4567)
        process = FakeProcess()
        create_process = AsyncMock(return_value=process)
        with (
            patch("batch_login.ssh_tunnel.asyncio.create_subprocess_exec", create_process),
            patch("batch_login.ssh_tunnel.probe_port", AsyncMock(return_value=True)),
        ):
            tunnel = SshTunnel(settings)
            url = await tunnel.start()

        self.assertEqual("http://127.0.0.1:4567", url)
        positional, keywords = create_process.call_args
        self.assertEqual(build_ssh_command(settings, 4567), list(positional))
        self.assertNotIn("shell", keywords)
        expected_flags = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
        self.assertEqual(expected_flags, keywords["creationflags"])

    async def test_automatic_port_retries_after_early_exit(self):
        settings = SshTunnelSettings(host="ssh.example.com", user="alice")
        failed_process = FakeProcess(returncode=255)
        ready_process = FakeProcess()
        create_process = AsyncMock(side_effect=[failed_process, ready_process])
        with (
            patch("batch_login.ssh_tunnel.choose_port", side_effect=[4101, 4102]) as choose,
            patch("batch_login.ssh_tunnel.asyncio.create_subprocess_exec", create_process),
            patch("batch_login.ssh_tunnel.probe_port", AsyncMock(return_value=True)),
        ):
            tunnel = SshTunnel(settings)
            url = await tunnel.start()

        self.assertEqual("http://127.0.0.1:4102", url)
        self.assertEqual(2, choose.call_count)
        self.assertEqual(2, create_process.await_count)
        self.assertIn("127.0.0.1:4101:127.0.0.1:8990", create_process.await_args_list[0].args)
        self.assertIn("127.0.0.1:4102:127.0.0.1:8990", create_process.await_args_list[1].args)

    async def test_automatic_port_attempts_are_limited_to_three(self):
        settings = SshTunnelSettings(host="ssh.example.com", user="alice")
        create_process = AsyncMock(side_effect=[FakeProcess(255), FakeProcess(255), FakeProcess(255)])
        with (
            patch("batch_login.ssh_tunnel.choose_port", side_effect=[4101, 4102, 4103]) as choose,
            patch("batch_login.ssh_tunnel.asyncio.create_subprocess_exec", create_process),
        ):
            with self.assertRaises(RuntimeError):
                await SshTunnel(settings).start()

        self.assertEqual(3, choose.call_count)
        self.assertEqual(3, create_process.await_count)

    async def test_explicit_port_early_exit_is_not_retried_and_error_is_sanitized(self):
        settings = SshTunnelSettings(
            host="secret.example.com",
            user="sensitive-user",
            local_port=4567,
            identity_file=r"C:\secret\identity.pem",
        )
        create_process = AsyncMock(return_value=FakeProcess(returncode=255))
        with patch("batch_login.ssh_tunnel.asyncio.create_subprocess_exec", create_process):
            with self.assertRaises(RuntimeError) as raised:
                await SshTunnel(settings).start()

        message = str(raised.exception)
        self.assertNotIn(settings.host, message)
        self.assertNotIn(settings.user, message)
        self.assertNotIn(settings.identity_file, message)
        self.assertNotIn("ssh -N", message)
        self.assertEqual(1, create_process.await_count)

    async def test_stop_kills_process_after_three_second_terminate_timeout(self):
        settings = SshTunnelSettings(host="ssh.example.com", user="alice", local_port=4567)
        process = HangingProcess()

        async def force_timeout(awaitable, timeout):
            self.assertEqual(3.0, timeout)
            awaitable.close()
            raise TimeoutError

        with (
            patch("batch_login.ssh_tunnel.asyncio.create_subprocess_exec", AsyncMock(return_value=process)),
            patch("batch_login.ssh_tunnel.probe_port", AsyncMock(return_value=True)),
            patch("batch_login.ssh_tunnel.asyncio.wait_for", side_effect=force_timeout),
        ):
            tunnel = SshTunnel(settings)
            await tunnel.start()
            await tunnel.stop()

        self.assertEqual(1, process.terminate_calls)
        self.assertEqual(1, process.kill_calls)
        self.assertEqual(2, process.wait_calls)

    async def test_stop_is_idempotent_and_does_not_touch_another_tunnel_process(self):
        first_process = FakeProcess()
        second_process = FakeProcess()
        create_process = AsyncMock(side_effect=[first_process, second_process])
        with (
            patch("batch_login.ssh_tunnel.asyncio.create_subprocess_exec", create_process),
            patch("batch_login.ssh_tunnel.probe_port", AsyncMock(return_value=True)),
        ):
            first = SshTunnel(SshTunnelSettings(host="one.example.com", user="alice", local_port=4101))
            second = SshTunnel(SshTunnelSettings(host="two.example.com", user="bob", local_port=4102))
            await first.start()
            await second.start()
            await first.stop()
            await first.stop()

        self.assertEqual(1, first_process.terminate_calls)
        self.assertEqual(1, first_process.wait_calls)
        self.assertEqual(0, second_process.terminate_calls)
        self.assertEqual(0, second_process.wait_calls)


if __name__ == "__main__":
    unittest.main()
