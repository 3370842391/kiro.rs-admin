import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.usage_client import UsageError, get_usage_limits
from batch_login.enterprise_http import HttpResponse


class FakeTransport:
    def __init__(self, responses):
        self.responses = list(responses)
        self.calls = []

    async def request(self, method, url, *, headers=None, json=None, **kwargs):
        self.calls.append({"method": method, "url": url, "headers": headers or {}})
        value = self.responses.pop(0)
        if isinstance(value, BaseException):
            raise value
        return value


def ok(data):
    return HttpResponse(200, {"content-type": "application/json"}, data)


CREDIT_BODY = {
    "usageBreakdownList": [
        {"resourceType": "CREDIT", "usageLimit": 500, "currentUsage": 120,
         "freeTrialInfo": {"freeTrialStatus": "ACTIVE", "usageLimit": 50, "currentUsage": 5}}
    ],
    "subscriptionInfo": {"subscriptionTitle": "Kiro Pro"},
    "nextDateReset": 1754006400,
}


class GetUsageLimitsTests(unittest.IsolatedAsyncioTestCase):
    async def test_parses_credit_and_builds_correct_request(self):
        transport = FakeTransport([ok(CREDIT_BODY)])
        snap = await get_usage_limits(
            transport, token="tok", profile_arn="arn:x", region="us-east-1"
        )
        # 550 total (500+50), 125 used (120+5), remaining 425
        self.assertEqual(550.0, snap.total)
        self.assertEqual(125.0, snap.used)
        self.assertEqual(425.0, snap.remaining)
        self.assertEqual("Kiro Pro", snap.subscription)
        self.assertTrue(snap.free_trial)
        self.assertTrue(snap.next_reset.startswith("2025-08-01"))
        call = transport.calls[0]
        self.assertEqual("GET", call["method"])
        self.assertTrue(call["url"].startswith("https://q.us-east-1.amazonaws.com/getUsageLimits?"))
        self.assertIn("origin=AI_EDITOR", call["url"])
        self.assertIn("resourceType=AGENTIC_REQUEST", call["url"])
        self.assertIn("profileArn=arn%3Ax", call["url"])
        self.assertEqual("Bearer tok", call["headers"]["Authorization"])
        self.assertIn("KiroIDE", call["headers"]["User-Agent"])
        self.assertNotIn("TokenType", call["headers"])

    async def test_external_idp_sends_tokentype(self):
        transport = FakeTransport([ok(CREDIT_BODY)])
        await get_usage_limits(
            transport, token="tok", profile_arn="arn:x", region="us-east-1",
            token_type="EXTERNAL_IDP",
        )
        self.assertEqual("EXTERNAL_IDP", transport.calls[0]["headers"]["TokenType"])

    async def test_403_falls_back_to_other_region(self):
        transport = FakeTransport([HttpResponse(403, {}, {}), ok(CREDIT_BODY)])
        snap = await get_usage_limits(transport, token="tok", region="us-east-1")
        self.assertEqual(425.0, snap.remaining)
        self.assertEqual(2, len(transport.calls))
        self.assertTrue(transport.calls[0]["url"].startswith("https://q.us-east-1"))
        self.assertTrue(transport.calls[1]["url"].startswith("https://q.eu-central-1"))

    async def test_http_error_classified(self):
        transport = FakeTransport([HttpResponse(500, {}, {"message": "boom"}), HttpResponse(500, {}, {})])
        with self.assertRaises(UsageError) as ctx:
            await get_usage_limits(transport, token="tok", region="us-east-1")
        self.assertEqual("http_error", ctx.exception.code)
        self.assertTrue(ctx.exception.retryable)

    async def test_missing_token_raises_before_request(self):
        transport = FakeTransport([])
        with self.assertRaises(UsageError) as ctx:
            await get_usage_limits(transport, token="  ", region="us-east-1")
        self.assertEqual("missing_token", ctx.exception.code)
        self.assertEqual([], transport.calls)

    async def test_no_credit_breakdown_yields_zero(self):
        transport = FakeTransport([ok({"usageBreakdownList": []})])
        snap = await get_usage_limits(transport, token="tok", region="us-east-1")
        self.assertEqual(0.0, snap.total)
        self.assertEqual(0.0, snap.remaining)
        self.assertFalse(snap.free_trial)


if __name__ == "__main__":
    unittest.main()
