#!/usr/bin/env python3
"""压测响应验真与持久化；HTTP 200、有效输出、完整结束分别记录。"""
import argparse
import collections
import json
from pathlib import Path


def effective(block):
    if not isinstance(block, dict):
        return False
    kind = block.get("type")
    if kind in ("text", "text_delta"):
        return isinstance(block.get("text"), str) and bool(block["text"].strip())
    if kind in ("thinking", "thinking_delta"):
        return isinstance(block.get("thinking"), str) and bool(block["thinking"].strip())
    if kind == "redacted_thinking":
        return bool(block.get("data"))
    if kind == "tool_use":
        return bool(block.get("id") and block.get("name"))
    return False


def assess(body, status, exit_code, stream):
    content = stopped = failed = False
    try:
        if stream:
            # SSE 帧可包含多行 data，不能按 TCP chunk 或单行统计完成。
            normalized = body.decode("utf-8").replace("\r\n", "\n")
            frames = normalized.split("\n\n")
            unfinished = frames.pop()
            failed |= any(line.startswith("data:") for line in unfinished.splitlines())
            for frame in frames:
                data = "\n".join(line[5:].lstrip() for line in frame.splitlines() if line.startswith("data:"))
                if not data:
                    continue
                event = json.loads(data)
                failed |= event.get("type") == "error"
                stopped |= event.get("type") == "message_stop"
                content |= effective(event.get("delta")) or effective(event.get("content_block"))
                message = event.get("message", {})
                content |= any(effective(block) for block in message.get("content", []))
        else:
            message = json.loads(body)
            failed = bool(message.get("error"))
            content = any(effective(block) for block in message.get("content", []))
            stopped = message.get("stop_reason") in ("end_turn", "max_tokens", "tool_use", "stop_sequence", "pause_turn", "refusal")
    except (ValueError, TypeError, AttributeError):
        failed = True
    return {"http_success": status == 200, "effective_output": content,
            "normal_end": stopped, "protocol_error": failed,
            "complete": exit_code == 0 and status == 200 and content and stopped and not failed}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    record = sub.add_parser("record")
    for key in ("body", "metrics", "out", "request-id", "headers"):
        record.add_argument("--" + key, required=True)
    record.add_argument("--exit-code", type=int, required=True)
    record.add_argument("--stream", action="store_true")
    summary = sub.add_parser("summary")
    summary.add_argument("directory")
    args = parser.parse_args()
    if args.command == "record":
        parts = Path(args.metrics).read_text().strip().split()
        try:
            status, elapsed, first_byte = int(parts[0]), float(parts[1]), float(parts[2])
        except (ValueError, IndexError):
            status, elapsed, first_byte = 0, None, None
        body_path = Path(args.body)
        body = body_path.read_bytes() if body_path.exists() else b""
        headers = Path(args.headers).read_text(encoding="utf-8", errors="replace")
        server_id = None
        for line in headers.splitlines():
            if line.upper().startswith("HTTP/"):
                server_id = None
            name, separator, value = line.partition(":")
            if separator and name.lower() == "x-oneapi-request-id":
                server_id = value.strip() or None
        row = {"request_id": server_id, "client_request_id": args.request_id, "server_request_id": server_id,
               "status": status, "curl_exit": args.exit_code,
               "total_seconds": elapsed, "http_first_byte_seconds": first_byte,
               "stream": args.stream, **assess(body, status, args.exit_code, args.stream)}
        with Path(args.out).open("a", encoding="utf-8") as target:
            target.write(json.dumps(row, ensure_ascii=False) + "\n")
    else:
        directory = Path(args.directory)
        rows = [json.loads(line) for path in directory.glob("w*.jsonl") for line in path.read_text(encoding="utf-8").splitlines() if line]
        ok = [row for row in rows if row["complete"]]
        result = {"requests": len(rows), "http_200": sum(row["http_success"] for row in rows),
                  "complete": len(ok), "completion_rate": len(ok) / len(rows) if rows else None,
                  "status_counts": dict(collections.Counter(row["status"] for row in rows))}
        (directory / "summary.json").write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        print(json.dumps(result, indent=2, ensure_ascii=False))
        print("逐请求记录和响应保留在：", directory)
        print("账号、端点、真实发送数需用 request_id 对照服务端 trace / 日志；HTTP 首字节不是首 token。")


if __name__ == "__main__":
    main()
