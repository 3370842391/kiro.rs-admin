import asyncio
import json
import sys
import unittest
from pathlib import Path

import httpx


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.rs_client import RsApiError
from batch_login.rs_import import RsImportClient


class ChunkedStream(httpx.AsyncByteStream):
    def __init__(self, chunks):
        self.chunks = chunks
        self.closed = False

    async def __aiter__(self):
        for chunk in self.chunks:
            yield chunk

    async def aclose(self):
        self.closed = True


class BlockingStream(httpx.AsyncByteStream):
    def __init__(self):
        self.started = asyncio.Event()
        self.closed = False
        self._never = asyncio.Event()

    async def __aiter__(self):
        self.started.set()
        await self._never.wait()
        yield b""

    async def aclose(self):
        self.closed = True


class RsImportTests(unittest.IsolatedAsyncioTestCase):
    async def test_405_batch_endpoint_falls_back_to_legacy_single_add(self):
        requests = []
        credentials = [
            {"email": "first@example.com", "refreshToken": "first-secret"},
            {"email": "second@example.com", "refreshToken": "second-secret"},
        ]

        async def handler(request):
            requests.append(request)
            if request.method == "GET":
                return httpx.Response(200, json={"credentials": []})
            if request.url.path.endswith("/credentials/batch-import"):
                return httpx.Response(405, text="method not allowed")
            body = json.loads(request.content)
            if body["email"] == "first@example.com":
                return httpx.Response(
                    200,
                    json={
                        "success": True,
                        "credentialId": 41,
                        "email": "first@example.com",
                    },
                )
            return httpx.Response(409, json={"error": "duplicate"})

        events = []
        async with RsImportClient(
            "https://rs.example",
            "admin-key",
            transport=httpx.MockTransport(handler),
        ) as client:
            await client.preflight()
            try:
                summary = await client.batch_import(credentials, events.append)
            except RsApiError as error:
                self.fail(f"旧版 RS 的 405 必须自动回退逐条导入：{error}")

        self.assertEqual(
            [
                ("GET", "/api/admin/credentials"),
                ("POST", "/api/admin/credentials/batch-import"),
                ("POST", "/api/admin/credentials"),
                ("POST", "/api/admin/credentials"),
            ],
            [(request.method, request.url.path) for request in requests],
        )
        self.assertEqual(
            {
                "total": 2,
                "imported": 1,
                "verified": 0,
                "duplicate": 1,
                "failed": 0,
                "rolledBack": 0,
            },
            summary,
        )
        self.assertEqual(["imported", "duplicate"], [event["status"] for event in events])
        self.assertEqual(41, events[0]["credentialId"])
        self.assertEqual("fi***@example.com", events[0]["email"])
        self.assertNotIn("first-secret", repr(events))
        self.assertNotIn("second-secret", repr(events))

    async def test_preflight_then_batch_import_parses_split_sse_as_dict_events(self):
        requests = []
        stream = ChunkedStream(
            [
                b'data: {"index":0,"status":"ver',
                b'ified","credentialId":9,"email":"user@example.com",',
                b'"token":"event-token","admin_key":"event-admin-key",',
                b'"error":"refresh_token=leaked-secret user@example.com"}\r',
                b'\n\r\ndata: {"status":"summary","summary":{"total":1,',
                b'"imported":0,"verified":1,"duplicate":0,"failed":0,'
                b'"rolledBack":0}}\r\n\r\n',
            ]
        )

        async def handler(request):
            requests.append(request)
            self.assertEqual("admin-key", request.headers["x-api-key"])
            if request.method == "GET":
                return httpx.Response(200, json={"credentials": []})
            return httpx.Response(
                200,
                headers={"content-type": "text/event-stream"},
                stream=stream,
            )

        events = []
        credentials = [{"email": "user@example.com", "refreshToken": "secret"}]
        async with RsImportClient(
            "https://rs.example/prefix/",
            "admin-key",
            transport=httpx.MockTransport(handler),
        ) as client:
            await client.preflight()
            summary = await client.batch_import(credentials, events.append)

        self.assertEqual(
            [
                ("GET", "/prefix/api/admin/credentials"),
                ("POST", "/prefix/api/admin/credentials/batch-import"),
            ],
            [(request.method, request.url.path) for request in requests],
        )
        self.assertEqual(
            {"credentials": credentials, "verify": True, "concurrency": 8},
            json.loads(requests[1].content),
        )
        self.assertEqual(1, len(events))
        self.assertIsInstance(events[0], dict)
        self.assertEqual("verified", events[0]["status"])
        self.assertEqual("us***@example.com", events[0]["email"])
        self.assertNotIn("user@example.com", repr(events[0]))
        self.assertNotIn("leaked-secret", repr(events[0]))
        self.assertNotIn("event-token", repr(events[0]))
        self.assertNotIn("event-admin-key", repr(events[0]))
        self.assertEqual(1, summary["verified"])
        self.assertTrue(stream.closed)

    async def test_batch_import_requires_a_summary_event(self):
        secret = "refresh-secret"

        async def handler(_request):
            return httpx.Response(
                200,
                stream=ChunkedStream(
                    [b'data: {"index":0,"status":"imported","email":"a@example.com"}\n\n']
                ),
            )

        async with RsImportClient(
            "https://rs.example",
            "admin-secret",
            transport=httpx.MockTransport(handler),
        ) as client:
            with self.assertRaises(RsApiError) as raised:
                await client.batch_import(
                    [{"email": "a@example.com", "refreshToken": secret}],
                    lambda _event: None,
                )

        self.assertEqual("invalid_rs_response", raised.exception.code)
        self.assertEqual("batch_import", raised.exception.stage)
        self.assertNotIn(secret, str(raised.exception))
        self.assertNotIn("admin-secret", str(raised.exception))

    async def test_http_errors_are_mapped_without_response_or_request_secrets(self):
        async def auth_failure(_request):
            return httpx.Response(
                401,
                json={"error": {"message": "admin-secret response-secret"}},
            )

        async with RsImportClient(
            "https://rs.example",
            "admin-secret",
            transport=httpx.MockTransport(auth_failure),
        ) as client:
            with self.assertRaises(RsApiError) as raised:
                await client.preflight()

        self.assertEqual("rs_auth_failed", raised.exception.code)
        self.assertEqual(401, raised.exception.status_code)
        self.assertNotIn("admin-secret", str(raised.exception))
        self.assertNotIn("response-secret", str(raised.exception))

        async def import_failure(request):
            if request.method == "GET":
                return httpx.Response(200, json={})
            return httpx.Response(502, text="refresh-secret response-secret")

        async with RsImportClient(
            "https://rs.example",
            "admin-secret",
            transport=httpx.MockTransport(import_failure),
        ) as client:
            await client.preflight()
            with self.assertRaises(RsApiError) as raised:
                await client.batch_import(
                    [{"email": "a@example.com", "refreshToken": "refresh-secret"}],
                    lambda _event: None,
                )

        self.assertEqual("upstream_error", raised.exception.code)
        rendered = str(raised.exception)
        self.assertNotIn("admin-secret", rendered)
        self.assertNotIn("refresh-secret", rendered)
        self.assertNotIn("response-secret", rendered)

    async def test_cancellation_closes_stream_and_context_closes_client(self):
        stream = BlockingStream()

        async def handler(request):
            if request.method == "GET":
                return httpx.Response(200, json={})
            return httpx.Response(200, stream=stream)

        client = RsImportClient(
            "https://rs.example",
            "admin-key",
            transport=httpx.MockTransport(handler),
        )
        async with client:
            await client.preflight()
            task = asyncio.create_task(
                client.batch_import(
                    [{"email": "a@example.com", "refreshToken": "secret"}],
                    lambda _event: None,
                )
            )
            await stream.started.wait()
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task
            self.assertTrue(stream.closed)
            self.assertFalse(client.client.is_closed)

        self.assertTrue(client.client.is_closed)


if __name__ == "__main__":
    unittest.main()
