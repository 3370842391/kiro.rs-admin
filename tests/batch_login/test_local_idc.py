import asyncio
import sys
import unittest
from pathlib import Path

import httpx


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.local_idc import LocalAuthError, LocalIdcClient


class LocalIdcTests(unittest.IsolatedAsyncioTestCase):
    async def test_start_and_poll_handles_pending_slow_down_and_success(self):
        requests = []
        sleeps = []
        replies = iter(
            [
                httpx.Response(
                    200,
                    json={"clientId": "client", "clientSecret": "client-secret"},
                ),
                httpx.Response(
                    200,
                    json={
                        "deviceCode": "device-secret",
                        "userCode": "ABCD-EFGH",
                        "verificationUri": "https://device.example/start",
                        "verificationUriComplete": "https://device.example/start?user_code=ABCD-EFGH",
                        "expiresIn": 600,
                        "interval": 0,
                    },
                ),
                httpx.Response(400, json={"error": "authorization_pending"}),
                httpx.Response(400, json={"error": "slow_down"}),
                httpx.Response(
                    200,
                    json={
                        "accessToken": "access-secret",
                        "refreshToken": "refresh-secret",
                        "expiresIn": 3600,
                    },
                ),
            ]
        )

        async def handler(request):
            requests.append(request)
            return next(replies)

        async def fake_sleep(seconds):
            sleeps.append(seconds)

        async with httpx.AsyncClient(
            transport=httpx.MockTransport(handler)
        ) as http:
            client = LocalIdcClient(http, sleep=fake_sleep, now=lambda: 100.0)
            session = await client.start(
                "https://example.awsapps.com/start", "us-east-1"
            )
            token = await client.poll(session)

        self.assertEqual("client", session.client_id)
        self.assertEqual("ABCD-EFGH", session.user_code)
        self.assertEqual("refresh-secret", token.refresh_token)
        self.assertEqual([0.2, 5.2], sleeps)
        self.assertEqual(
            [
                "/client/register",
                "/device_authorization",
                "/token",
                "/token",
                "/token",
            ],
            [request.url.path for request in requests],
        )
        register = requests[0].read().decode("utf-8")
        self.assertIn('"issuerUrl":"https://example.awsapps.com/start"', register)
        self.assertIn("device_code", register)

    async def test_start_falls_back_to_plain_verification_uri(self):
        replies = iter(
            [
                httpx.Response(200, json={"clientId": "client", "clientSecret": "secret"}),
                httpx.Response(
                    200,
                    json={
                        "deviceCode": "device",
                        "userCode": "CODE",
                        "verificationUri": "https://device.example/start",
                        "expiresIn": 60,
                        "interval": 5,
                    },
                ),
            ]
        )

        async def handler(_request):
            return next(replies)

        async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
            session = await LocalIdcClient(http, now=lambda: 10).start(
                "https://example.awsapps.com/start", "us-east-1"
            )

        self.assertEqual("https://device.example/start", session.verification_url)
        self.assertEqual(70, session.expires_at)

    async def test_poll_classifies_expired_and_access_denied(self):
        for error_code, expected in (
            ("expired_token", "session_expired"),
            ("access_denied", "access_denied"),
        ):
            with self.subTest(error_code=error_code):
                async def handler(_request, code=error_code):
                    return httpx.Response(400, json={"error": code})

                async with httpx.AsyncClient(
                    transport=httpx.MockTransport(handler)
                ) as http:
                    client = LocalIdcClient(http, now=lambda: 0)
                    session = self.session(expires_at=10)
                    with self.assertRaises(LocalAuthError) as raised:
                        await client.poll(session)

                self.assertEqual(expected, raised.exception.code)
                self.assertFalse(raised.exception.retryable)

    async def test_invalid_response_and_http_error_never_echo_secrets(self):
        for response in (
            httpx.Response(
                200,
                json={
                    "clientId": "client",
                    "clientSecret": "server-secret",
                    "unexpected": True,
                },
            ),
            httpx.Response(500, text="refreshToken=server-secret"),
        ):
            with self.subTest(status=response.status_code):
                async def handler(_request, result=response):
                    return result

                async with httpx.AsyncClient(
                    transport=httpx.MockTransport(handler)
                ) as http:
                    with self.assertRaises(LocalAuthError) as raised:
                        await LocalIdcClient(http).start(
                            "https://example.awsapps.com/start", "us-east-1"
                        )

                self.assertNotIn("server-secret", str(raised.exception))

    async def test_network_error_is_retryable_and_sanitized(self):
        async def handler(request):
            raise httpx.ConnectError(
                "clientSecret=network-secret", request=request
            )

        async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
            with self.assertRaises(LocalAuthError) as raised:
                await LocalIdcClient(http).start(
                    "https://example.awsapps.com/start", "us-east-1"
                )

        self.assertTrue(raised.exception.retryable)
        self.assertNotIn("network-secret", str(raised.exception))

    async def test_invalid_region_and_start_url_are_rejected_before_network(self):
        async def handler(_request):
            self.fail("network should not be called")

        async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
            client = LocalIdcClient(http)
            for start_url, region, code in (
                ("http://example.awsapps.com/start", "us-east-1", "invalid_start_url"),
                ("https://example.awsapps.com/start", "../region", "invalid_region"),
            ):
                with self.subTest(code=code):
                    with self.assertRaises(LocalAuthError) as raised:
                        await client.start(start_url, region)
                    self.assertEqual(code, raised.exception.code)

    def test_secret_dataclass_repr_is_safe(self):
        session = self.session(expires_at=10)
        representation = repr(session)

        self.assertNotIn("client-secret", representation)
        self.assertNotIn("device-secret", representation)

    @staticmethod
    def session(*, expires_at):
        from batch_login.local_idc import IdcSession

        return IdcSession(
            region="us-east-1",
            start_url="https://example.awsapps.com/start",
            client_id="client",
            client_secret="client-secret",
            device_code="device-secret",
            user_code="CODE",
            verification_url="https://device.example/start",
            expires_at=expires_at,
            interval=0,
        )


if __name__ == "__main__":
    unittest.main()
