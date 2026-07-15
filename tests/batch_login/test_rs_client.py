import json
import sys
import unittest
from pathlib import Path

import httpx


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.rs_client import RsApiError, RsClient, parse_callback_url


class CallbackParserTests(unittest.TestCase):
    def test_parses_external_idp_descriptor_from_query(self):
        callback = parse_callback_url(
            "http://127.0.0.1/signin/callback"
            "?login_option=external_idp"
            "&issuer_url=https%3A%2F%2Flogin.microsoftonline.com%2Ftenant%2Fv2.0"
            "&client_id=client-123"
            "&scopes=openid+offline_access"
            "&login_hint=user%40example.com"
        )

        self.assertEqual(
            {
                "loginOption": "external_idp",
                "path": "/signin/callback",
                "issuerUrl": "https://login.microsoftonline.com/tenant/v2.0",
                "clientId": "client-123",
                "scopes": "openid offline_access",
                "loginHint": "user@example.com",
            },
            callback,
        )

    def test_parses_final_callback_from_query(self):
        callback = parse_callback_url(
            "http://127.0.0.1/oauth/callback?code=final-code&state=final-state"
        )

        self.assertEqual("final-code", callback["code"])
        self.assertEqual("final-state", callback["state"])
        self.assertEqual("/oauth/callback", callback["path"])

    def test_parses_callback_from_fragment_and_accepts_camel_case_aliases(self):
        callback = parse_callback_url(
            "http://127.0.0.1/oauth/callback"
            "#code=fragment-code&state=fragment-state&loginOption=external_idp"
            "&issuerUrl=https%3A%2F%2Flogin.microsoftonline.com%2Ft%2Fv2.0"
            "&clientId=client-456&scope=openid&loginHint=hint%40example.com"
        )

        self.assertEqual("fragment-code", callback["code"])
        self.assertEqual("fragment-state", callback["state"])
        self.assertEqual("external_idp", callback["loginOption"])
        self.assertEqual("client-456", callback["clientId"])
        self.assertEqual("openid", callback["scopes"])
        self.assertEqual("hint@example.com", callback["loginHint"])

    def test_query_values_take_precedence_over_fragment_values(self):
        callback = parse_callback_url(
            "http://127.0.0.1/oauth/callback?code=query-code&state=query-state"
            "#code=fragment-code&state=fragment-state"
        )

        self.assertEqual("query-code", callback["code"])
        self.assertEqual("query-state", callback["state"])

    def test_rejects_callback_without_code_or_complete_descriptor(self):
        malformed = (
            "not a callback URL",
            "http://127.0.0.1/oauth/callback?state=only-state",
            "http://127.0.0.1/signin/callback?issuer_url=https%3A%2F%2Fexample.com",
            "http://127.0.0.1/signin/callback?client_id=client-only",
        )

        for raw_url in malformed:
            with self.subTest(raw_url=raw_url), self.assertRaises(ValueError):
                parse_callback_url(raw_url)


class RsClientTests(unittest.IsolatedAsyncioTestCase):
    async def test_side_effecting_post_does_not_retry_ambiguous_network_failure(self):
        calls = 0

        def handler(request: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            raise httpx.ReadError("response lost after submit", request=request)

        async with RsClient(
            "https://rs.example",
            "key",
            transport=httpx.MockTransport(handler),
            retry_delays=(0, 0),
        ) as client:
            with self.assertRaises(RsApiError) as raised:
                await client.start_social(email="user@example.com")

        self.assertEqual(1, calls)
        self.assertFalse(raised.exception.retryable)

    async def test_retry_policy_honors_structured_retryable(self):
        calls = 0

        def handler(_request: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            return httpx.Response(
                500,
                json={"error": {"code": "terminal", "retryable": False}},
            )

        async with RsClient(
            "https://rs.example",
            "key",
            transport=httpx.MockTransport(handler),
            retry_delays=(0, 0),
        ) as client:
            with self.assertRaises(RsApiError):
                await client.preflight()

        self.assertEqual(1, calls)

    async def test_structured_retryable_response_can_retry_post(self):
        calls = 0

        def handler(_request: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            if calls == 1:
                return httpx.Response(
                    429,
                    json={"error": {"code": "busy", "retryable": True}},
                )
            return httpx.Response(200, json={"sessionId": "created"})

        async with RsClient(
            "https://rs.example",
            "key",
            transport=httpx.MockTransport(handler),
            retry_delays=(0, 0),
        ) as client:
            result = await client.start_social(email="user@example.com")

        self.assertEqual("created", result["sessionId"])
        self.assertEqual(2, calls)

    async def test_rejects_redirect_and_non_object_success_responses(self):
        replies = iter(
            [
                httpx.Response(302, headers={"location": "/login"}),
                httpx.Response(200, content=b""),
                httpx.Response(200, text="<html>not json</html>"),
                httpx.Response(200, json=None),
                httpx.Response(200, json=["not", "an", "object"]),
            ]
        )

        def handler(_request: httpx.Request) -> httpx.Response:
            return next(replies)

        async with RsClient(
            "https://rs.example",
            "key",
            transport=httpx.MockTransport(handler),
            retry_delays=(),
        ) as client:
            for _ in range(5):
                with self.assertRaises(RsApiError) as raised:
                    await client._request("GET", "/invalid")
                self.assertEqual("invalid_rs_response", raised.exception.code)

    async def test_server_error_message_cannot_echo_sensitive_values(self):
        secret = "oauth-secret-code"

        def handler(_request: httpx.Request) -> httpx.Response:
            return httpx.Response(
                400,
                json={
                    "error": {
                        "code": "invalid_code",
                        "stage": "social_callback",
                        "message": f"authorization code {secret} rejected",
                    }
                },
            )

        async with RsClient(
            "https://rs.example",
            "admin-secret",
            transport=httpx.MockTransport(handler),
        ) as client:
            with self.assertRaises(RsApiError) as raised:
                await client._request("POST", "/callback", {"code": secret})

        self.assertNotIn(secret, str(raised.exception))
        self.assertNotIn(secret, raised.exception.message)
        self.assertNotIn("admin-secret", str(raised.exception))

    async def test_structured_error_fields_are_normalized(self):
        def handler(_request: httpx.Request) -> httpx.Response:
            return httpx.Response(
                400,
                json={
                    "error": {
                        "type": "invalid_request",
                        "message": "state did not match",
                        "code": "state_mismatch",
                        "stage": "social_callback",
                        "retryable": False,
                    }
                },
            )

        async with RsClient(
            "https://rs.example",
            "admin-secret",
            transport=httpx.MockTransport(handler),
        ) as client:
            with self.assertRaises(RsApiError) as raised:
                await client._request("GET", "/structured")

        error = raised.exception
        self.assertEqual("state_mismatch", error.code)
        self.assertEqual("social_callback", error.stage)
        self.assertFalse(error.retryable)
        self.assertEqual(400, error.status_code)
        self.assertEqual("RS 请求失败", error.message)
        self.assertIn("state_mismatch", str(error))
        self.assertIn("social_callback", str(error))
        self.assertNotIn("admin-secret", str(error))

    async def test_legacy_auth_errors_are_normalized(self):
        def handler(_request: httpx.Request) -> httpx.Response:
            return httpx.Response(
                401,
                json={"error": {"type": "authentication_error", "message": "Invalid key"}},
            )

        async with RsClient(
            "https://rs.example/api/admin/",
            "admin-secret",
            transport=httpx.MockTransport(handler),
        ) as client:
            with self.assertRaises(RsApiError) as raised:
                await client._request("GET", "/credentials")

        self.assertEqual("rs_auth_failed", raised.exception.code)
        self.assertEqual("rs_request", raised.exception.stage)
        self.assertFalse(raised.exception.retryable)
        self.assertEqual(401, raised.exception.status_code)
        self.assertNotIn("admin-secret", str(raised.exception))

    async def test_final_legacy_5xx_is_upstream_error(self):
        calls = 0

        def handler(_request: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            return httpx.Response(502, text="proxy failure")

        async with RsClient(
            "https://rs.example",
            "key",
            transport=httpx.MockTransport(handler),
            retry_delays=(0, 0),
        ) as client:
            with self.assertRaises(RsApiError) as raised:
                await client._request("GET", "/legacy-5xx")

        self.assertEqual(3, calls)
        self.assertEqual("upstream_error", raised.exception.code)
        self.assertTrue(raised.exception.retryable)

    async def test_503_twice_then_success_makes_three_calls(self):
        calls = 0

        def handler(_request: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            if calls < 3:
                return httpx.Response(503, json={})
            return httpx.Response(200, json={"ok": True})

        async with RsClient(
            "https://rs.example",
            "key",
            transport=httpx.MockTransport(handler),
            retry_delays=(0, 0, 0),
        ) as client:
            result = await client._request("GET", "/retry")

        self.assertEqual({"ok": True}, result)
        self.assertEqual(3, calls)

    async def test_request_exception_retries_then_raises_network_error(self):
        calls = 0

        def handler(request: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            raise httpx.ConnectError("connection unavailable", request=request)

        async with RsClient(
            "https://rs.example",
            "admin-secret",
            transport=httpx.MockTransport(handler),
            retry_delays=(0, 0),
        ) as client:
            with self.assertRaises(RsApiError) as raised:
                await client._request("GET", "/network", {"callback": "secret-callback"})

        error = raised.exception
        self.assertEqual(3, calls)
        self.assertEqual("network_error", error.code)
        self.assertEqual("rs_request", error.stage)
        self.assertTrue(error.retryable)
        self.assertEqual(0, error.status_code)
        self.assertNotIn("admin-secret", str(error))
        self.assertNotIn("secret-callback", str(error))

    async def test_methods_send_expected_paths_headers_and_payloads(self):
        requests: list[httpx.Request] = []

        def handler(request: httpx.Request) -> httpx.Response:
            requests.append(request)
            return httpx.Response(200, json={"ok": True})

        admin_key = "top-secret-admin-key"
        callback_url = (
            "http://127.0.0.1/signin/callback?login_option=external_idp"
            "&issuer_url=https%3A%2F%2Flogin.microsoftonline.com%2Ft%2Fv2.0"
            "&client_id=client-123&scopes=openid&login_hint=user%40example.com"
        )
        async with RsClient(
            "https://rs.example/",
            admin_key,
            transport=httpx.MockTransport(handler),
        ) as client:
            self.assertIsNone(await client.preflight())
            await client.start_idc(
                region="us-east-1",
                start_url="https://example.awsapps.com/start",
                email="enterprise@example.com",
            )
            await client.poll_idc("idc-session")
            await client.start_social(email="social@example.com")
            await client.complete_social("social-session", callback_url)
            await client.cancel_idc("idc-session")
            await client.cancel_social("social-session")

        self.assertEqual(
            [
                ("GET", "/api/admin/credentials"),
                ("POST", "/api/admin/auth/idc/start"),
                ("POST", "/api/admin/auth/idc/poll/idc-session"),
                ("POST", "/api/admin/auth/social/start"),
                ("POST", "/api/admin/auth/social/complete/social-session"),
                ("DELETE", "/api/admin/auth/idc/idc-session"),
                ("DELETE", "/api/admin/auth/social/social-session"),
            ],
            [(request.method, request.url.path) for request in requests],
        )
        for request in requests:
            self.assertEqual(admin_key, request.headers["x-api-key"])
            self.assertEqual("application/json", request.headers["accept"])

        self.assertEqual(
            {
                "region": "us-east-1",
                "startUrl": "https://example.awsapps.com/start",
                "email": "enterprise@example.com",
            },
            json.loads(requests[1].content),
        )
        self.assertEqual({"email": "social@example.com"}, json.loads(requests[3].content))
        callback_payload = json.loads(requests[4].content)
        self.assertEqual("external_idp", callback_payload["loginOption"])
        self.assertEqual("client-123", callback_payload["clientId"])
        self.assertEqual(
            "https://login.microsoftonline.com/t/v2.0", callback_payload["issuerUrl"]
        )
        self.assertNotIn("login_option", callback_payload)
        self.assertNotIn(admin_key, repr(callback_payload))
        self.assertNotIn(callback_url, repr(callback_payload))

    async def test_base_url_validation_and_prefix_normalization(self):
        invalid = [
            "ftp://rs.example",
            "https://user:pass@rs.example",
            "https://rs.example?tenant=a",
            "https://rs.example#fragment",
            "rs.example",
        ]
        for value in invalid:
            with self.subTest(value=value), self.assertRaises(ValueError):
                RsClient(value, "key")

        requests: list[httpx.Request] = []

        def handler(request: httpx.Request) -> httpx.Response:
            requests.append(request)
            return httpx.Response(200, json={})

        async with RsClient(
            "https://rs.example/prefix/",
            "key",
            transport=httpx.MockTransport(handler),
        ) as client:
            await client.preflight()
        self.assertEqual("/prefix/api/admin/credentials", requests[0].url.path)

    async def test_session_id_cannot_escape_path_segment(self):
        async with RsClient(
            "https://rs.example",
            "key",
            transport=httpx.MockTransport(
                lambda _request: httpx.Response(200, json={})
            ),
        ) as client:
            for session_id in ("../credentials", "id?dump=1", "id#fragment", ""):
                with self.subTest(session_id=session_id), self.assertRaises(ValueError):
                    await client.cancel_social(session_id)


if __name__ == "__main__":
    unittest.main()
