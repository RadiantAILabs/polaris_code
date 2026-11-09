"""A local socket that plays the Bedrock Responses endpoint.

The agent under test reaches its model with one signed ``POST
/openai/v1/responses`` per model call. This server answers each of those with
the next scripted reply and keeps the body of every request exactly as the
agent sent it.

A request that carries no tool definitions is not a turn of the agent's loop
(for example a summary or a title). It is answered with a fixed text, does not
use up a scripted reply, and is listed apart so that a build making such a call
is visible rather than shifting every later reply.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

RESPONSES_PATH = "/openai/v1/responses"

#: What an exhausted script answers with, so that a run still ends.
EXHAUSTED_TEXT = "No more scripted replies."

#: What a request without tool definitions is answered with.
AUXILIARY_TEXT = "Scripted reply to a call outside the agent loop."


def _usage() -> dict:
    return {
        "input_tokens": 100,
        "output_tokens": 10,
        "total_tokens": 110,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens_details": {"reasoning_tokens": 0},
    }


def message_item(text: str, number: int) -> dict:
    """An assistant message output item."""
    return {
        "type": "message",
        "id": f"msg_{number}",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    }


def call_item(number: int, name: str, arguments: dict) -> dict:
    """A function call with the two identifiers the endpoint gives it."""
    return {
        "type": "function_call",
        "id": f"fc_{number}",
        "call_id": f"call_{number}",
        "name": name,
        "arguments": json.dumps(arguments),
        "status": "completed",
    }


def reasoning_item(number: int, text: str) -> dict:
    """A reasoning output item with one summary."""
    return {
        "type": "reasoning",
        "id": f"rs_{number}",
        "summary": [{"type": "summary_text", "text": text}],
        "status": "completed",
    }


def response_body(output: list[dict]) -> dict:
    """A completed Responses reply carrying ``output``."""
    return {
        "id": "resp_scripted",
        "created_at": 0,
        "model": "openai.gpt-5.4",
        "object": "response",
        "output": output,
        "parallel_tool_calls": True,
        "status": "completed",
        "usage": _usage(),
    }


def render_reply(reply: dict, first_call_number: int) -> tuple[dict, int]:
    """Turn one scripted reply into a Responses body.

    A reply may have ``reasoning`` (text), ``text`` (a message) and ``calls`` (a
    list of ``{"tool": name, "arguments": {...}}``), in that order. Calls are
    numbered from ``first_call_number``; the next free number is returned too.
    """
    output: list[dict] = []
    number = first_call_number
    if reply.get("reasoning"):
        output.append(reasoning_item(number, reply["reasoning"]))
    if reply.get("text") is not None:
        output.append(message_item(reply["text"], number))
    for call in reply.get("calls", []):
        output.append(call_item(number, call["tool"], call.get("arguments", {})))
        number += 1
    return response_body(output), number


class Endpoint:
    """Serves scripted replies and records every request."""

    def __init__(self, script: list[dict]):
        self._script = list(script)
        self._lock = threading.Lock()
        self._next_call = 1
        #: Every request in arrival order: ``{"path", "body", "auxiliary"}``.
        self.requests: list[dict] = []
        #: How many requests arrived after the script ran out.
        self.beyond_script = 0
        endpoint = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802 (name fixed by the base class)
                endpoint._handle(self)

            def log_message(self, *args):  # silence the default stderr log
                pass

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._server.daemon_threads = True
        self.port = self._server.server_address[1]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def start(self) -> "Endpoint":
        self._thread.start()
        return self

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)

    def __enter__(self) -> "Endpoint":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.stop()

    # -- one request ---------------------------------------------------------

    @staticmethod
    def _read_body(handler: BaseHTTPRequestHandler) -> bytes:
        if handler.headers.get("transfer-encoding", "").lower() == "chunked":
            data = b""
            while True:
                size = int(handler.rfile.readline().split(b";")[0].strip() or b"0", 16)
                if size == 0:
                    handler.rfile.readline()
                    return data
                data += handler.rfile.read(size)
                handler.rfile.readline()
        length = int(handler.headers.get("content-length") or 0)
        return handler.rfile.read(length)

    def _handle(self, handler: BaseHTTPRequestHandler) -> None:
        raw = self._read_body(handler)
        try:
            body = json.loads(raw)
        except ValueError:
            body = None
        path = handler.path
        if not path.split("?")[0].endswith(RESPONSES_PATH):
            with self._lock:
                self.requests.append(
                    {"path": path, "body": body, "auxiliary": False, "unexpected": True}
                )
            self._send(handler, 404, {"error": {"message": f"no route for {path}"}})
            return
        auxiliary = not (isinstance(body, dict) and body.get("tools"))
        with self._lock:
            self.requests.append({"path": path, "body": body, "auxiliary": auxiliary})
            if auxiliary:
                reply = {"text": AUXILIARY_TEXT}
            elif self._script:
                reply = self._script.pop(0)
            else:
                self.beyond_script += 1
                reply = {"text": EXHAUSTED_TEXT}
            payload, self._next_call = render_reply(reply, self._next_call)
        self._send(handler, 200, payload)

    @staticmethod
    def _send(handler: BaseHTTPRequestHandler, status: int, payload: dict) -> None:
        data = json.dumps(payload).encode()
        handler.send_response(status)
        handler.send_header("content-type", "application/json")
        handler.send_header("content-length", str(len(data)))
        handler.send_header("connection", "close")
        handler.end_headers()
        handler.wfile.write(data)
        handler.close_connection = True
