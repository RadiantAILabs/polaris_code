"""A local proxy that refuses every request and writes down where it was going.

An agent build may reach for the internet on its own, for example to fetch a
table of model prices in the background. A run must not depend on that and must
not send anything outside this machine. Each binary is started with this proxy
named as its HTTP and HTTPS proxy. The model endpoint is on the no-proxy list,
so it is reached directly; every other request ends here with a refusal, and the
target is kept so the report can show what a build tried to reach.

A build that ignores proxy settings is not stopped by this. The report shows
only what went through the proxy.
"""

from __future__ import annotations

import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class NetworkGuard:
    """Refuses every proxied request and records its target."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        #: ``"<method> <target>"`` for every refused request, in arrival order.
        #: A secure request is a ``CONNECT`` to ``host:port``; a plain one names
        #: the host of its URL.
        self.attempts: list[str] = []
        guard = self

        class Handler(BaseHTTPRequestHandler):
            def _refuse(self) -> None:
                guard._record(self.command, self.path)
                self.send_response(502, "Refused by the parity tool")
                self.send_header("content-length", "0")
                self.send_header("connection", "close")
                self.end_headers()
                self.close_connection = True

            do_CONNECT = do_GET = do_HEAD = do_POST = do_PUT = do_DELETE = _refuse  # noqa: N815

            def log_message(self, *args):  # silence the default stderr log
                pass

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._server.daemon_threads = True
        self.port = self._server.server_address[1]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def _record(self, method: str, target: str) -> None:
        if method != "CONNECT":
            # Keep the host only: a path can carry a key or a query.
            rest = target.split("://", 1)[-1]
            target = rest.split("/", 1)[0].split("?", 1)[0]
        with self._lock:
            self.attempts.append(f"{method} {target}")

    def start(self) -> "NetworkGuard":
        self._thread.start()
        return self

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)

    def __enter__(self) -> "NetworkGuard":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.stop()
