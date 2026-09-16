import importlib.util
from pathlib import Path
import unittest

path = Path(__file__).resolve().parents[1] / "scripts" / "stress_result.py"
spec = importlib.util.spec_from_file_location("stress_result", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class StressResultTests(unittest.TestCase):
    def test_http_200_error_empty_or_incomplete_is_not_success(self):
        for body in [b"", b'{}', b'{"error":{"message":"bad"}}',
                     b'{"content":[{"type":"text","text":"hello"}]}']:
            self.assertFalse(module.assess(body, 200, 0, False)["complete"])

    def test_network_failure_is_not_success_even_after_200_headers(self):
        body = b'{"content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn"}'
        self.assertFalse(module.assess(body, 200, 28, False)["complete"])
        self.assertTrue(module.assess(body, 200, 0, False)["complete"])

    def test_stream_requires_effective_content_and_message_stop(self):
        delta = b'data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}\n\n'
        stop = b'data: {"type":"message_stop"}\n\n'
        self.assertFalse(module.assess(delta, 200, 0, True)["complete"])
        self.assertFalse(module.assess(stop, 200, 0, True)["complete"])
        self.assertTrue(module.assess(delta + stop, 200, 0, True)["complete"])
        error = b'data: {"type":"error","error":{"message":"fail"}}\n\n'
        self.assertFalse(module.assess(delta + error + stop, 200, 0, True)["complete"])

    def test_valid_tool_call_is_effective_content(self):
        body = b'{"content":[{"type":"tool_use","id":"t1","name":"read","input":{}}],"stop_reason":"tool_use"}'
        self.assertTrue(module.assess(body, 200, 0, False)["complete"])

    def test_null_text_and_unterminated_sse_stop_are_not_success(self):
        self.assertFalse(module.assess(b'{"content":[{"type":"text","text":null}],"stop_reason":"end_turn"}', 200, 0, False)["complete"])
        body = b'data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}\n\ndata: {"type":"message_stop"}'
        self.assertFalse(module.assess(body, 200, 0, True)["complete"])


if __name__ == "__main__":
    unittest.main()
