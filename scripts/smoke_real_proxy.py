#!/usr/bin/env python3
"""Small, dependency-free smoke test for a running ox-sse-proxy instance."""

from __future__ import annotations

import argparse
import codecs
import json
import os
import sys
import time
from collections import Counter
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Smoke test a real ox-sse-proxy gateway")
    parser.add_argument("--host", default="127.0.0.1", help="proxy host (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=18899, help="proxy port (default: 18899)")
    parser.add_argument("--model", default="ox-alpha-free", help="Responses model")
    parser.add_argument("--timeout", type=float, default=60.0, help="socket timeout seconds")
    return parser.parse_args()


def safe_host(host: str) -> str:
    return f"[{host}]" if ":" in host and not host.startswith("[") else host


class SSEInspector:
    """Parse complete SSE lines while the response is still being read."""

    def __init__(self) -> None:
        self.counts: Counter[str] = Counter()
        self.delta_chars = 0
        self._line_buffer = b""
        self._decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        self._event_name: str | None = None
        self._data_lines: list[str] = []

    def _dispatch(self) -> None:
        if self._event_name is None:
            self._data_lines.clear()
            return
        self.counts[self._event_name] += 1
        if self._event_name == "response.output_text.delta" and self._data_lines:
            try:
                data = json.loads("\n".join(self._data_lines))
            except json.JSONDecodeError as error:
                raise ValueError("output text delta data is not JSON") from error
            delta = data.get("delta") if isinstance(data, dict) else None
            if not isinstance(delta, str) or not delta:
                raise ValueError("output text delta is empty")
            self.delta_chars += len(delta)
        self._event_name = None
        self._data_lines.clear()

    def _line(self, raw_line: bytes) -> None:
        line = self._decoder.decode(raw_line, final=False)
        if line.endswith("\r"):
            line = line[:-1]
        if not line:
            self._dispatch()
        elif line.startswith("event:"):
            self._event_name = line[6:].strip()
        elif line.startswith("data:"):
            self._data_lines.append(line[5:].lstrip())

    def feed(self, chunk: bytes) -> None:
        self._line_buffer += chunk
        while True:
            newline = self._line_buffer.find(b"\n")
            if newline < 0:
                return
            self._line(self._line_buffer[:newline])
            self._line_buffer = self._line_buffer[newline + 1 :]

    def finish(self) -> tuple[Counter[str], int]:
        if self._line_buffer:
            self._line(self._line_buffer)
            self._line_buffer = b""
        # A server may close immediately after the final data line without an
        # extra blank line. Treat that complete final event as observable too.
        if self._event_name is not None:
            self._dispatch()
        return self.counts, self.delta_chars


def smoke(args: argparse.Namespace) -> int:
    if not 1 <= args.port <= 65535:
        print("SMOKE_FAIL reason=invalid_port", file=sys.stderr)
        return 1
    if args.timeout <= 0:
        print("SMOKE_FAIL reason=invalid_timeout", file=sys.stderr)
        return 1
    api_key = os.environ.get("OPENCODE_GO_API_KEY")
    if not api_key:
        print("SMOKE_FAIL reason=OPENCODE_GO_API_KEY_missing", file=sys.stderr)
        return 1

    payload = {
        "model": args.model,
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Reply with exactly SMOKE_OK",
                    }
                ],
            }
        ],
        "stream": True,
    }
    request = Request(
        f"http://{safe_host(args.host)}:{args.port}/responses",
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Accept": "text/event-stream",
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
            "User-Agent": (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) "
                "AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36"
            ),
        },
        method="POST",
    )

    started = time.monotonic()
    ttfb_ms: float | None = None
    inspector = SSEInspector()
    status: int | str = "NA"
    try:
        with urlopen(request, timeout=args.timeout) as response:
            status = response.status
            content_type = response.headers.get("Content-Type", "")
            while True:
                chunk = response.read(8192)
                if not chunk:
                    break
                if ttfb_ms is None:
                    ttfb_ms = (time.monotonic() - started) * 1000
                inspector.feed(chunk)
    except HTTPError as error:
        elapsed_ms = (time.monotonic() - started) * 1000
        print(
            f"SMOKE_FAIL status={error.code} ttfb_ms=NA total_ms={elapsed_ms:.1f} reason=http_error",
            file=sys.stderr,
        )
        return 1
    except (URLError, TimeoutError, OSError):
        elapsed_ms = (time.monotonic() - started) * 1000
        print(
            f"SMOKE_FAIL status=NA ttfb_ms=NA total_ms={elapsed_ms:.1f} reason=connection_failed",
            file=sys.stderr,
        )
        return 1
    except ValueError as error:
        elapsed_ms = (time.monotonic() - started) * 1000
        ttfb_label = f"{ttfb_ms:.1f}" if ttfb_ms is not None else "NA"
        print(
            f"SMOKE_FAIL status={status} ttfb_ms={ttfb_label} total_ms={elapsed_ms:.1f} reason={error}",
            file=sys.stderr,
        )
        return 1

    total_ms = (time.monotonic() - started) * 1000
    ttfb_label = f"{ttfb_ms:.1f}" if ttfb_ms is not None else "NA"
    if status != 200:
        print(
            f"SMOKE_FAIL status={status} ttfb_ms={ttfb_label} total_ms={total_ms:.1f} reason=http_status",
            file=sys.stderr,
        )
        return 1
    if "text/event-stream" not in content_type.lower():
        print(
            f"SMOKE_FAIL status={status} ttfb_ms={ttfb_label} total_ms={total_ms:.1f} reason=not_sse",
            file=sys.stderr,
        )
        return 1

    try:
        counts, delta_chars = inspector.finish()
    except ValueError as error:
        print(
            f"SMOKE_FAIL status={status} ttfb_ms={ttfb_label} total_ms={total_ms:.1f} reason={error}",
            file=sys.stderr,
        )
        return 1
    required = ("response.created", "response.completed", "response.output_text.delta")
    missing = [name for name in required if counts[name] == 0]
    if missing or delta_chars == 0:
        reason = "missing=" + ",".join(missing) if missing else "empty_text_delta"
        print(
            f"SMOKE_FAIL status={status} ttfb_ms={ttfb_label} total_ms={total_ms:.1f} reason={reason}",
            file=sys.stderr,
        )
        return 1

    event_summary = ",".join(f"{name}:{counts[name]}" for name in sorted(counts))
    print(
        f"SMOKE_OK status={status} ttfb_ms={ttfb_label} total_ms={total_ms:.1f} "
        f"events={event_summary} text_delta_chars={delta_chars}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(smoke(parse_args()))
