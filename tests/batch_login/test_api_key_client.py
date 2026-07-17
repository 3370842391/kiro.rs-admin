import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.api_key_client import (
    ApiKeyError,
    create_api_key,
    ensure_api_key,
    list_api_keys,
    resolve_profile_arn,
)
from batch_login.enterprise_http import HttpResponse


class FakeTransport:
    """记录每次请求;按 (url, target) 顺序返回预置响应。"""

    def __init__(self, responses):
        self.responses = list(responses)
        self.calls = []
        self.closed = False

    async def request(self, method, url, *, headers=None, json=None, **kwargs):
        self.calls.append(
            {"method": method, "url": url, "headers": headers or {}, "json": json}
        )
        value = self.responses.pop(0)
        if isinstance(value, BaseException):
            raise value
        return value

    async def close(self):
        self.closed = True


def ok(data):
    return HttpResponse(200, {"content-type": "application/json"}, data)


ARN = "arn:aws:codewhisperer:us-east-1:389192036452:profile/HG4CUCMEN7QV"


class ResolveProfileArnTests(unittest.IsolatedAsyncioTestCase):
    async def test_resolves_first_arn_and_sends_correct_target(self):
        transport = FakeTransport([ok({"profiles": [{"arn": ARN}]})])
        arn = await resolve_profile_arn(transport, token="tok", region="us-east-1")
        self.assertEqual(ARN, arn)
        call = transport.calls[0]
        self.assertEqual("https://q.us-east-1.amazonaws.com/", call["url"])
        self.assertEqual(
            "AmazonCodeWhispererService.ListAvailableProfiles",
            call["headers"]["x-amz-target"],
        )
        self.assertEqual("Bearer tok", call["headers"]["authorization"])
        self.assertNotIn("tokentype", call["headers"])

    async def test_external_idp_sends_tokentype_header(self):
        transport = FakeTransport([ok({"profiles": [{"arn": ARN}]})])
        await resolve_profile_arn(
            transport, token="tok", region="us-east-1", token_type="EXTERNAL_IDP"
        )
        self.assertEqual("EXTERNAL_IDP", transport.calls[0]["headers"]["tokentype"])

    async def test_empty_profiles_returns_none(self):
        transport = FakeTransport([ok({"profiles": []})])
        self.assertIsNone(
            await resolve_profile_arn(transport, token="tok", region="us-east-1")
        )


class CreateApiKeyTests(unittest.IsolatedAsyncioTestCase):
    async def test_returns_raw_key_and_sends_profile_and_label(self):
        transport = FakeTransport(
            [ok({"keyId": "kskid_x", "keyPrefix": "ksk_ab", "rawKey": "ksk_abcdef123"})]
        )
        raw = await create_api_key(
            transport, token="tok", profile_arn=ARN, label="codeflow2-7"
        )
        self.assertEqual("ksk_abcdef123", raw)
        call = transport.calls[0]
        self.assertEqual("https://management.us-east-1.kiro.dev/", call["url"])
        self.assertEqual(
            "KiroControlPlaneBearerService.CreateApiKey",
            call["headers"]["x-amz-target"],
        )
        self.assertEqual(
            {"profileArn": ARN, "label": "codeflow2-7"}, call["json"]
        )

    async def test_missing_raw_key_raises(self):
        transport = FakeTransport([ok({"keyId": "kskid_x", "keyPrefix": "ksk_ab"})])
        with self.assertRaises(ApiKeyError) as ctx:
            await create_api_key(transport, token="tok", profile_arn=ARN, label="l")
        self.assertEqual("missing_raw_key", ctx.exception.code)

    async def test_http_error_classified_retryable_on_5xx(self):
        transport = FakeTransport(
            [HttpResponse(503, {}, {"message": "boom"})]
        )
        with self.assertRaises(ApiKeyError) as ctx:
            await create_api_key(transport, token="tok", profile_arn=ARN, label="l")
        self.assertTrue(ctx.exception.retryable)
        self.assertEqual(503, ctx.exception.status_code)

    async def test_missing_token_raises_before_request(self):
        transport = FakeTransport([])
        with self.assertRaises(ApiKeyError) as ctx:
            await create_api_key(transport, token="  ", profile_arn=ARN, label="l")
        self.assertEqual("missing_token", ctx.exception.code)
        self.assertEqual([], transport.calls)


class ListApiKeysTests(unittest.IsolatedAsyncioTestCase):
    async def test_lists_keys(self):
        transport = FakeTransport(
            [ok({"keys": [{"keyId": "k1", "label": "codeflow2-7"}]})]
        )
        keys = await list_api_keys(transport, token="tok", profile_arn=ARN)
        self.assertEqual("codeflow2-7", keys[0]["label"])
        self.assertEqual(
            "KiroControlPlaneBearerService.ListApiKeys",
            transport.calls[0]["headers"]["x-amz-target"],
        )


class EnsureApiKeyTests(unittest.IsolatedAsyncioTestCase):
    async def test_resolves_profile_then_creates(self):
        transport = FakeTransport(
            [
                ok({"profiles": [{"arn": ARN}]}),
                ok({"rawKey": "ksk_new"}),
            ]
        )
        result = await ensure_api_key(transport, token="tok", label="codeflow2-7")
        self.assertEqual("ksk_new", result.raw_key)
        self.assertEqual(ARN, result.profile_arn)
        self.assertFalse(result.reused)

    async def test_uses_supplied_profile_arn_without_resolving(self):
        transport = FakeTransport([ok({"rawKey": "ksk_new"})])
        result = await ensure_api_key(
            transport, token="tok", label="l", profile_arn=ARN
        )
        self.assertEqual("ksk_new", result.raw_key)
        self.assertEqual("https://management.us-east-1.kiro.dev/", transport.calls[0]["url"])

    async def test_skip_if_labeled_exists_returns_reused(self):
        transport = FakeTransport(
            [ok({"keys": [{"keyId": "k1", "label": "codeflow2-7"}]})]
        )
        result = await ensure_api_key(
            transport,
            token="tok",
            label="codeflow2-7",
            profile_arn=ARN,
            skip_if_labeled_exists=True,
        )
        self.assertIsNone(result.raw_key)
        self.assertTrue(result.reused)

    async def test_no_profile_arn_raises(self):
        transport = FakeTransport([ok({"profiles": []})])
        with self.assertRaises(ApiKeyError) as ctx:
            await ensure_api_key(transport, token="tok", label="l")
        self.assertEqual("no_profile_arn", ctx.exception.code)


if __name__ == "__main__":
    unittest.main()
