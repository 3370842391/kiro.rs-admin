import base64
import json
import sys
import unittest
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.enterprise_http import (
    EnterpriseHttpClient,
    EnterpriseHttpError,
    EnterpriseHttpSettings,
    HttpResponse,
)


def d2c_token(visitor_id="visitor-123"):
    payload = base64.urlsafe_b64encode(
        json.dumps({"vid": visitor_id}).encode()
    ).decode().rstrip("=")
    return f"header.{payload}.signature"


class FakeTransport:
    def __init__(self, responses, *, fail_at=None):
        self.responses = list(responses)
        self.requests = []
        self.fail_at = fail_at
        self.cookies = {}

    async def request(self, method, url, **kwargs):
        self.requests.append((method, url, kwargs))
        if self.fail_at == len(self.requests):
            raise OSError("network result unknown")
        if not self.responses:
            raise AssertionError(f"unexpected request: {method} {url}")
        return self.responses.pop(0)


@dataclass
class FakeAttempt:
    operation_id: str
    password: str


class FakeVault:
    def __init__(self, password="Generated-Password-42!", unresolved=False):
        self.password = password
        self.events = []
        self.unresolved_attempt = (
            FakeAttempt("op-1", password) if unresolved else None
        )

    def unresolved(self, account, *, scope=""):
        return self.unresolved_attempt

    def prepare(self, account, **metadata):
        metadata["account"] = account
        self.events.append(("prepared", metadata))
        return FakeAttempt("op-1", self.password)

    def mark_confirmed(self, operation_id):
        self.events.append(("confirmed", operation_id))

    def mark_rejected(self, operation_id, reason):
        self.events.append(("rejected", operation_id, reason))

    def mark_uncertain(self, operation_id, reason):
        self.events.append(("uncertain", operation_id, reason))


ENC_CTX = {
    "workflowResponseData": {
        "encryptionContextResponse": {
            "publicKey": {"kty": "RSA", "kid": "kid", "n": "n", "e": "AQAB"},
            "issuer": "signin",
            "audience": "AWSPasswordService",
            "region": "us-east-1",
        }
    }
}


def response(payload, status=200):
    return HttpResponse(status, {}, payload)


def base_responses(password_result):
    return [
        response({"clientId": "client", "clientSecret": "secret"}),
        response({"deviceCode": "device", "userCode": "ABCD-EFGH"}),
        response(
            {
                "redirectUrl": (
                    "https://us-east-1.signin.aws/platform/d-123/login"
                    "?workflowStateHandle=wh-1"
                ),
                "csrfToken": "csrf",
            }
        ),
        response({"token": d2c_token()}),
        response({"stepId": "get-identity-user", "workflowStateHandle": "wh-2"}),
        response({"stepId": "get-password", "workflowStateHandle": "wh-3", **ENC_CTX}),
        response(password_result),
    ]


def tail_responses():
    return [
        response({"token": "sso-session"}),
        response({"deviceContext": "device-context"}),
        response({}),
        response({"accessToken": "access", "refreshToken": "refresh", "expiresIn": 3600}),
    ]


class EnterpriseHttpTests(unittest.IsolatedAsyncioTestCase):
    def make_client(self, responses, vault=None, fail_at=None):
        transport = FakeTransport(responses, fail_at=fail_at)
        client = EnterpriseHttpClient(
            transport,
            vault=vault or FakeVault(),
            fingerprint=lambda *_args, **_kwargs: "fingerprint",
            password_encryptor=lambda password, *_args, **_kwargs: "jwe:" + password,
            app_js_config_loader=lambda _body: None,
        )
        return client, transport

    async def test_existing_password_completes_without_password_change(self):
        redirect = {
            "url": "https://d-123.awsapps.com/start/?workflowResultHandle=auth&state=state"
        }
        vault = FakeVault()
        client, transport = self.make_client(
            base_responses({"stepId": "end-of-workflow-success", "redirect": redirect})
            + tail_responses(),
            vault,
        )

        result = await client.login(
            "admin-user",
            "existing-password",
            EnterpriseHttpSettings("https://d-123.awsapps.com/start", "us-east-1"),
        )

        self.assertEqual("refresh", result.refresh_token)
        self.assertEqual([], vault.events)
        self.assertEqual("client", result.client_id)
        self.assertEqual("d-123", result.directory_id)
        self.assertEqual([], transport.responses)

    async def test_new_sso_instance_portal_discovers_directory_and_signin_endpoint(self):
        portal_url = (
            "https://ssoins-7223a15405d7b4ec.portal.us-east-1.app.aws/"
        )
        responses = base_responses(
            {
                "stepId": "end-of-workflow-success",
                "redirect": {
                    "url": portal_url
                    + "?workflowResultHandle=auth&state=state"
                },
            }
        )
        responses[2] = response(
            {
                "redirectUrl": (
                    "https://us-east-1.sso.signin.aws/platform/d-9066772d19/login"
                    "?workflowStateHandle=wh-1"
                ),
                "csrfToken": "csrf",
            }
        )
        client, transport = self.make_client(responses + tail_responses())

        result = await client.login(
            "admin-user",
            "existing-password",
            EnterpriseHttpSettings(portal_url, "us-east-1"),
        )

        self.assertEqual("d-9066772d19", result.directory_id)
        self.assertEqual(
            "https://oidc.us-east-1.api.aws/client/register",
            transport.requests[0][1],
        )
        self.assertIn(
            "portal.sso.us-east-1.api.aws/login",
            transport.requests[2][1],
        )
        self.assertIn(
            "idc_instance_id=ssoins-7223a15405d7b4ec",
            transport.requests[2][1],
        )
        self.assertIn(
            "us-east-1.sso.signin.aws/platform/d-9066772d19/api/execute",
            transport.requests[4][1],
        )

    async def test_resume_tries_saved_unresolved_password_before_input_password(self):
        redirect = {
            "url": "https://d-123.awsapps.com/start/?workflowResultHandle=auth&state=state"
        }
        vault = FakeVault(unresolved=True)
        client, transport = self.make_client(
            base_responses({"stepId": "end-of-workflow-success", "redirect": redirect})
            + tail_responses(),
            vault,
        )

        await client.login(
            "admin-user",
            "one-time",
            EnterpriseHttpSettings("https://d-123.awsapps.com/start", "us-east-1"),
        )

        password_request = transport.requests[6][2]["json"]
        self.assertEqual(
            "jwe:Generated-Password-42!",
            password_request["inputs"][0]["password"],
        )
        self.assertEqual(("confirmed", "op-1"), vault.events[-1])

    async def test_resume_falls_back_to_input_password_when_saved_candidate_is_rejected(self):
        redirect = {
            "url": "https://d-123.awsapps.com/start/?workflowResultHandle=auth&state=state"
        }
        first = base_responses({})[:6]
        first.append(response({"message": "invalid credentials"}, status=400))
        second_attempt = base_responses(
            {"stepId": "end-of-workflow-success", "redirect": redirect}
        )[2:]
        vault = FakeVault(unresolved=True)
        client, transport = self.make_client(
            first + second_attempt + tail_responses(), vault
        )

        await client.login(
            "admin-user",
            "one-time",
            EnterpriseHttpSettings("https://d-123.awsapps.com/start", "us-east-1"),
        )

        first_password = transport.requests[6][2]["json"]["inputs"][0]["password"]
        second_password = transport.requests[11][2]["json"]["inputs"][0]["password"]
        self.assertEqual("jwe:Generated-Password-42!", first_password)
        self.assertEqual("jwe:one-time", second_password)
        self.assertEqual(("rejected", "op-1", "candidate_not_active"), vault.events[-1])

    async def test_first_login_persists_generated_password_before_change(self):
        change_redirect = {
            "url": "https://d-123.awsapps.com/start/?workflowResultHandle=auth&state=state"
        }
        vault = FakeVault()
        responses = base_responses(
            {"stepId": "get-new-password-for-change-password", "workflowStateHandle": "wh-4"}
        )
        responses += [response({"stepId": "end-of-workflow-success", "redirect": change_redirect})]
        responses += tail_responses()
        client, transport = self.make_client(responses, vault)

        await client.login(
            "admin-user",
            "one-time",
            EnterpriseHttpSettings("https://d-123.awsapps.com/start", "us-east-1"),
        )

        self.assertEqual("prepared", vault.events[0][0])
        self.assertEqual(("confirmed", "op-1"), vault.events[1])
        change_request = transport.requests[7][2]["json"]
        self.assertEqual(
            "jwe:Generated-Password-42!",
            change_request["inputs"][0]["newPassword"],
        )

    async def test_unknown_change_result_marks_password_uncertain(self):
        vault = FakeVault()
        responses = base_responses(
            {"stepId": "get-new-password-for-change-password", "workflowStateHandle": "wh-4"}
        )
        client, _transport = self.make_client(responses, vault, fail_at=8)

        with self.assertRaises(EnterpriseHttpError) as raised:
            await client.login(
                "admin-user",
                "one-time",
                EnterpriseHttpSettings("https://d-123.awsapps.com/start", "us-east-1"),
            )

        self.assertEqual("password_change_uncertain", raised.exception.code)
        self.assertEqual("prepared", vault.events[0][0])
        self.assertEqual("uncertain", vault.events[1][0])

    async def test_change_http_200_confirms_saved_password_before_redirect_followup(self):
        vault = FakeVault()
        responses = base_responses(
            {"stepId": "get-new-password-for-change-password", "workflowStateHandle": "wh-4"}
        )
        responses += [response({"stepId": "end-of-workflow-success"})]
        client, _transport = self.make_client(responses, vault, fail_at=9)

        with self.assertRaises(EnterpriseHttpError):
            await client.login(
                "admin-user",
                "one-time",
                EnterpriseHttpSettings("https://d-123.awsapps.com/start", "us-east-1"),
            )

        self.assertEqual(["prepared", "confirmed"], [event[0] for event in vault.events])

    async def test_unknown_step_after_password_is_rejected(self):
        client, _transport = self.make_client(
            base_responses({"stepId": "mfa-challenge"})
        )

        with self.assertRaises(EnterpriseHttpError) as raised:
            await client.login(
                "admin-user",
                "one-time",
                EnterpriseHttpSettings("https://d-123.awsapps.com/start", "us-east-1"),
            )

        self.assertEqual("unsupported_signin_step", raised.exception.code)
        self.assertNotIn("one-time", str(raised.exception))

    async def test_oidc_token_poll_retries_authorization_pending(self):
        redirect = {
            "url": "https://d-123.awsapps.com/start/?workflowResultHandle=auth&state=state"
        }
        responses = base_responses(
            {"stepId": "end-of-workflow-success", "redirect": redirect}
        )
        responses += tail_responses()[:-1]
        responses += [
            response({"error": "authorization_pending"}, status=400),
            response({"accessToken": "access", "refreshToken": "refresh"}),
        ]
        client, transport = self.make_client(responses)
        client.sleep = lambda _seconds: _completed()

        result = await client.login(
            "admin-user",
            "existing-password",
            EnterpriseHttpSettings("https://d-123.awsapps.com/start", "us-east-1"),
        )

        self.assertEqual("refresh", result.refresh_token)
        self.assertEqual([], transport.responses)


async def _completed():
    return None


if __name__ == "__main__":
    unittest.main()
