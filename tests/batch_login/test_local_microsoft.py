import base64
import json
import sys
import unittest
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import parse_qs, quote, urlsplit

import httpx

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.local_idc import LocalAuthError
from batch_login.local_microsoft import (
    MicrosoftProtocol,
    MicrosoftToken,
    email_from_jwt,
    parse_portal_callback,
    validate_external_endpoint,
)


class MicrosoftProtocolTests(unittest.IsolatedAsyncioTestCase):
    def test_new_session_contains_pkce_and_fixed_redirect(self):
        session = MicrosoftProtocol.new_session("us-east-1")
        parts = urlsplit(session.signin_url)
        query = parse_qs(parts.query)
        self.assertEqual("app.kiro.dev", parts.hostname)
        self.assertEqual([session.state], query["state"])
        self.assertEqual(["http://localhost:3128"], query["redirect_uri"])
        self.assertEqual(["S256"], query["code_challenge_method"])
        self.assertNotIn(session.verifier, session.signin_url)
        self.assertNotIn(session.verifier, repr(session))

    def test_callback_parses_social_and_external_and_validates_state(self):
        social = parse_portal_callback(
            "http://localhost:3128?code=abc&state=expected", "expected"
        )
        external = parse_portal_callback(
            "http://localhost:3128?issuer_url="
            + quote("https://login.microsoftonline.com/tenant/v2.0")
            + "&client_id=client&scopes=openid&login_hint=user%40example.com&state=expected",
            "expected",
        )
        self.assertEqual("social", social.kind)
        self.assertEqual("external_idp", external.kind)
        self.assertEqual("client", external.client_id)
        with self.assertRaises(LocalAuthError) as raised:
            parse_portal_callback(
                "http://localhost:3128?code=abc&state=wrong", "expected"
            )
        self.assertEqual("state_mismatch", raised.exception.code)
        with self.assertRaises(LocalAuthError) as raised:
            parse_portal_callback(
                "https://evil.example/?code=abc&state=expected", "expected"
            )
        self.assertEqual("invalid_callback", raised.exception.code)

    def test_external_endpoint_allowlist_rejects_suffix_confusion_and_ip(self):
        self.assertEqual(
            "https://login.microsoftonline.com/t/oauth2/v2.0/token",
            validate_external_endpoint(
                "https://login.microsoftonline.com/t/oauth2/v2.0/token"
            ),
        )
        for url in (
            "http://login.microsoftonline.com/token",
            "https://login.microsoftonline.com.evil.example/token",
            "https://127.0.0.1/token",
        ):
            with self.subTest(url=url):
                with self.assertRaises(LocalAuthError):
                    validate_external_endpoint(url)

    async def test_social_exchange_and_record(self):
        async def handler(request):
            self.assertEqual("/oauth/token", request.url.path)
            return httpx.Response(
                200,
                json={
                    "accessToken": jwt({"email": "token@example.com"}),
                    "refreshToken": "refresh-secret",
                    "expiresIn": 3600,
                    "profileArn": "arn:profile",
                },
            )
        async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
            protocol = MicrosoftProtocol(http)
            token = await protocol.exchange_social("code", "verifier")
            record = protocol.social_record(
                "input@example.com",
                "us-east-1",
                token,
                datetime(2026, 7, 15, tzinfo=timezone.utc),
            )
        self.assertEqual("token@example.com", record.email)
        self.assertEqual("social", record.auth_method)
        self.assertEqual("refresh-secret", record.refresh_token)
        self.assertEqual("2026-07-15T01:00:00Z", record.expires_at)

    async def test_external_discovery_prepare_exchange_and_record(self):
        requests = []
        async def handler(request):
            requests.append(request)
            if request.method == "GET":
                return httpx.Response(200, json={
                    "authorization_endpoint": "https://login.microsoftonline.com/t/oauth2/v2.0/authorize",
                    "token_endpoint": "https://login.microsoftonline.com/t/oauth2/v2.0/token",
                })
            return httpx.Response(200, json={
                "access_token": jwt({"preferred_username": "entra@example.com"}),
                "refresh_token": "entra-refresh",
                "expires_in": 1800,
            })
        callback = parse_portal_callback(
            "http://localhost:3128?issuer_url="
            + quote("https://login.microsoftonline.com/t")
            + "&client_id=client&scopes=openid%20offline_access&state=s",
            "s",
        )
        async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
            protocol = MicrosoftProtocol(http)
            leg = await protocol.prepare_external(callback)
            token = await protocol.exchange_external(
                leg,
                f"http://localhost:3128/oauth/callback?code=final&state={leg.state}",
            )
            record = protocol.external_record(
                "input@example.com", "us-east-1", leg, token,
                datetime(2026, 7, 15, tzinfo=timezone.utc),
            )
        self.assertIn("code_challenge=", leg.authorize_url)
        self.assertNotIn(leg.verifier, leg.authorize_url)
        self.assertEqual("external_idp", record.auth_method)
        self.assertEqual("entra@example.com", record.email)
        self.assertEqual(2, len(requests))

    async def test_discovery_rejects_malicious_returned_endpoint(self):
        async def handler(_request):
            return httpx.Response(200, json={
                "authorization_endpoint": "https://login.microsoftonline.com.evil.example/auth",
                "token_endpoint": "https://login.microsoftonline.com/token",
            })
        async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
            with self.assertRaises(LocalAuthError) as raised:
                await MicrosoftProtocol(http).discover(
                    "https://login.microsoftonline.com/t"
                )
        self.assertEqual("unsafe_idp_endpoint", raised.exception.code)

    async def test_token_errors_never_echo_response_secret(self):
        async def handler(_request):
            return httpx.Response(500, text="refreshToken=server-secret")
        async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
            with self.assertRaises(LocalAuthError) as raised:
                await MicrosoftProtocol(http).exchange_social("code", "verifier")
        self.assertNotIn("server-secret", str(raised.exception))

    def test_email_from_jwt_is_best_effort(self):
        self.assertEqual("user@example.com", email_from_jwt(jwt({"upn": "user@example.com"})))
        self.assertEqual("", email_from_jwt("not-a-jwt"))


def jwt(claims):
    payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).rstrip(b"=").decode()
    return f"header.{payload}.signature"


if __name__ == "__main__":
    unittest.main()
