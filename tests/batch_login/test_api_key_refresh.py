import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.api_key_client import ApiKeyError
from batch_login.api_key_refresh import refresh_access_token
from batch_login.enterprise_http import HttpResponse


class FakeTransport:
    def __init__(self, responses):
        self.responses = list(responses)
        self.calls = []
        self.closed = False

    async def request(self, method, url, *, headers=None, json=None, **kwargs):
        self.calls.append({"method": method, "url": url, "headers": headers or {}, "json": json})
        value = self.responses.pop(0)
        if isinstance(value, BaseException):
            raise value
        return value

    async def close(self):
        self.closed = True


def ok(data):
    return HttpResponse(200, {"content-type": "application/json"}, data)


OLD_URL = "https://d-9067123456.awsapps.com/start"
NEW_URL = "https://ssoins-abc123.portal.us-west-2.app.aws/"


class RefreshAccessTokenTests(unittest.IsolatedAsyncioTestCase):
    async def test_old_portal_uses_amazonaws_oidc_and_refresh_grant(self):
        transport = FakeTransport(
            [ok({"accessToken": "new-access", "refreshToken": "new-refresh", "expiresIn": 3600})]
        )
        result = await refresh_access_token(
            transport,
            client_id="cid",
            client_secret="csecret",
            refresh_token="old-refresh",
            start_url=OLD_URL,
            region="us-east-1",
        )
        self.assertEqual("new-access", result.access_token)
        self.assertEqual("new-refresh", result.refresh_token)
        self.assertEqual(3600, result.expires_in)
        call = transport.calls[0]
        self.assertEqual("https://oidc.us-east-1.amazonaws.com/token", call["url"])
        self.assertEqual(
            {
                "clientId": "cid",
                "clientSecret": "csecret",
                "refreshToken": "old-refresh",
                "grantType": "refresh_token",
            },
            call["json"],
        )

    async def test_new_portal_uses_api_aws_oidc_and_region_from_url(self):
        transport = FakeTransport([ok({"accessToken": "a2"})])
        result = await refresh_access_token(
            transport,
            client_id="cid",
            client_secret="csecret",
            refresh_token="r",
            start_url=NEW_URL,
            region="us-east-1",
        )
        self.assertEqual("a2", result.access_token)
        self.assertIsNone(result.refresh_token)
        self.assertIsNone(result.expires_in)
        self.assertEqual("https://oidc.us-west-2.api.aws/token", transport.calls[0]["url"])

    async def test_missing_refresh_token_raises_before_request(self):
        transport = FakeTransport([])
        with self.assertRaises(ApiKeyError) as ctx:
            await refresh_access_token(
                transport, client_id="cid", client_secret="cs", refresh_token="  ",
                start_url=OLD_URL, region="us-east-1",
            )
        self.assertEqual("missing_refresh_token", ctx.exception.code)
        self.assertEqual([], transport.calls)

    async def test_missing_client_credentials_raises(self):
        transport = FakeTransport([])
        with self.assertRaises(ApiKeyError) as ctx:
            await refresh_access_token(
                transport, client_id="", client_secret="cs", refresh_token="r",
                start_url=OLD_URL, region="us-east-1",
            )
        self.assertEqual("missing_oidc_client", ctx.exception.code)

    async def test_http_error_classified_retryable_on_5xx(self):
        transport = FakeTransport([HttpResponse(503, {}, {"error": "boom"})])
        with self.assertRaises(ApiKeyError) as ctx:
            await refresh_access_token(
                transport, client_id="c", client_secret="s", refresh_token="r",
                start_url=OLD_URL, region="us-east-1",
            )
        self.assertEqual("refresh_failed", ctx.exception.code)
        self.assertTrue(ctx.exception.retryable)
        self.assertEqual(503, ctx.exception.status_code)

    async def test_invalid_start_url_raises(self):
        transport = FakeTransport([])
        with self.assertRaises(ApiKeyError) as ctx:
            await refresh_access_token(
                transport, client_id="c", client_secret="s", refresh_token="r",
                start_url="not-a-url", region="us-east-1",
            )
        self.assertEqual("invalid_start_url", ctx.exception.code)

    async def test_missing_access_token_in_response_raises(self):
        transport = FakeTransport([ok({"refreshToken": "only-refresh"})])
        with self.assertRaises(ApiKeyError) as ctx:
            await refresh_access_token(
                transport, client_id="c", client_secret="s", refresh_token="r",
                start_url=OLD_URL, region="us-east-1",
            )
        self.assertEqual("refresh_missing_token", ctx.exception.code)

    async def test_network_error_wrapped_retryable(self):
        transport = FakeTransport([RuntimeError("boom")])
        with self.assertRaises(ApiKeyError) as ctx:
            await refresh_access_token(
                transport, client_id="c", client_secret="s", refresh_token="r",
                start_url=OLD_URL, region="us-east-1",
            )
        self.assertEqual("network_error", ctx.exception.code)
        self.assertTrue(ctx.exception.retryable)


if __name__ == "__main__":
    unittest.main()
