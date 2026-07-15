import sys
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread
from urllib.parse import urlsplit

from playwright.async_api import async_playwright


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.browser_flows import BrowserFlowError, BrowserFlows


PAGES = {
    "/device-code": """
      <form action='/enterprise'>
        <label>User code <input name='userCode' autocomplete='one-time-code'></label>
        <button>Continue</button>
      </form>
    """,
    "/mfa": """
      <p>verification code required</p>
      <script>setTimeout(() => document.body.innerText = 'approved', 100)</script>
    """,
    "/enterprise": """
      <form action='/password'>
        <label>用户名 <input name='username'></label>
        <button>下一步</button>
      </form>
    """,
    "/password": """
      <form action='/done'>
        <label>密码 <input name='password' type='password'></label>
        <button>登录</button>
      </form>
    """,
    "/done": "<h1>授权成功</h1>",
    "/portal": """
      <form action='http://127.0.0.1:9/signin/callback'>
        <input type='hidden' name='login_option' value='external_idp'>
        <input type='hidden' name='issuer_url' value='https://login.microsoftonline.com/t/v2.0'>
        <input type='hidden' name='client_id' value='client'>
        <input type='hidden' name='state' value='portal-state'>
        <label>电子邮件 <input name='email' type='email'></label>
        <button>继续</button>
      </form>
    """,
    "/portal-cookie": """
      <script>document.cookie = 'portalPhase=complete; Path=/'</script>
      <form action='http://127.0.0.1:9/signin/callback'>
        <input type='hidden' name='login_option' value='external_idp'>
        <input type='hidden' name='issuer_url' value='https://login.microsoftonline.com/t/v2.0'>
        <input type='hidden' name='client_id' value='client'>
        <input type='hidden' name='state' value='portal-state'>
        <label>电子邮件 <input name='email' type='email'></label>
        <button>继续</button>
      </form>
    """,
    "/microsoft": """
      <form action='http://127.0.0.1:9/oauth/callback'>
        <input type='hidden' name='code' value='final-code'>
        <input type='hidden' name='state' value='final-state'>
        <button>继续</button>
      </form>
    """,
    "/invalid": "<p>incorrect password</p>",
}


def start_fixture_server(pages):
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            path = urlsplit(self.path).path
            self.server.seen_requests.append((path, self.headers.get("cookie", "")))
            body = pages.get(path, "<h1>not found</h1>").encode("utf-8")
            self.send_response(200 if path in pages else 404)
            self.send_header("content-type", "text/html; charset=utf-8")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, _format, *_args):
            return

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.seen_requests = []
    Thread(target=server.serve_forever, daemon=True).start()
    host, port = server.server_address
    return server, f"http://{host}:{port}"


class BrowserContractTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.server, self.base_url = start_fixture_server(PAGES)
        self.playwright = await async_playwright().start()
        self.browser = await self.playwright.chromium.launch(headless=True)
        self.driver = BrowserFlows(
            self.browser,
            timeout_seconds=5,
            mfa_timeout_seconds=1,
        )

    async def asyncTearDown(self):
        await self.browser.close()
        await self.playwright.stop()
        self.server.shutdown()
        self.server.server_close()

    async def test_enterprise_fills_username_then_password(self):
        async with self.driver.account_context() as session:
            await session.complete_enterprise(
                self.base_url + "/enterprise",
                "alice",
                "secret",
            )
            self.assertIn("/done", session.page.url)

    async def test_enterprise_can_fill_device_code_before_credentials(self):
        async with self.driver.account_context() as session:
            await session.complete_enterprise(
                self.base_url + "/device-code",
                "alice",
                "secret",
                "ABCD-EFGH",
            )
            self.assertIn("/done", session.page.url)

    async def test_manual_step_reports_structured_safe_event(self):
        events = []
        driver = BrowserFlows(
            self.browser,
            timeout_seconds=1,
            mfa_timeout_seconds=1,
            event_sink=events.append,
        )
        async with driver.account_context() as session:
            await session.page.goto(self.base_url + "/mfa")
            await session._wait_for_manual_step(
                "verification code required",
                "mfa_timeout",
            )

        self.assertEqual("manual_action_required", events[0]["kind"])
        self.assertEqual("mfa", events[0]["manualKind"])
        self.assertNotIn("verification code required", str(events[0]))

    async def test_loopback_connection_failure_still_yields_callback_url(self):
        async with self.driver.account_context() as session:
            callback = await session.capture_callback(
                self.base_url + "/portal",
                "user@example.com",
                "secret",
                expected_path="/signin/callback",
            )
            self.assertIn("login_option=external_idp", callback)
            self.assertIn("state=portal-state", callback)

    async def test_invalid_credentials_are_classified(self):
        async with self.driver.account_context() as session:
            with self.assertRaises(BrowserFlowError) as raised:
                await session.complete_enterprise(
                    self.base_url + "/invalid",
                    "alice",
                    "wrong",
                )
            self.assertEqual("invalid_credentials", raised.exception.code)
            self.assertFalse(raised.exception.retryable)

    async def test_microsoft_two_stage_flow_reuses_one_browser_context(self):
        async with self.driver.account_context() as session:
            first = await session.capture_callback(
                self.base_url + "/portal-cookie",
                "user@example.com",
                "secret",
                expected_path="/signin/callback",
            )
            second = await session.capture_callback(
                self.base_url + "/microsoft",
                "user@example.com",
                "secret",
                expected_path="/oauth/callback",
            )

        self.assertIn("login_option=external_idp", first)
        self.assertIn("code=final-code", second)
        microsoft_cookies = [
            cookie
            for path, cookie in self.server.seen_requests
            if path == "/microsoft"
        ]
        self.assertTrue(microsoft_cookies)
        self.assertIn("portalPhase=complete", microsoft_cookies[-1])


if __name__ == "__main__":
    unittest.main()
