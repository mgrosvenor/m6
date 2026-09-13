#!/usr/bin/env python3
"""An m6 backend in Python, written from docs/m6-backend-protocol.md.

Usage: main.py <socket-path> <status-json-path>

Why Python, per docs/m6-backend-examples.md §4: native compute behind a thin
dispatch layer. The usual framing of "slow but good libraries" is wrong for this
use case. For numerical work the computation does not happen in Python at all:
numpy dispatches into BLAS, scipy into Fortran kernels, torch and onnxruntime
into optimised native code or a GPU. Python is the orchestration layer, and its
cost is per request rather than per element.

So the fit is: small request, large computation, and a native implementation
someone else has already optimised. Model inference, a statistical summary over
a dataset, an image transform, a similarity search over embeddings.

**Read this example's benchmark number as the floor of what dispatch costs, not
as a verdict on the language.** `/status` is deliberately the shape Python is
worst at: trivial per-request work at a high request rate, with nothing native
to dispatch into. That is the opposite of why anyone would choose it.

No framework and no numpy, per §9: the example demonstrates the contract, and a
numerical dependency would make it about numpy and put a wheel build in the
gate. Standard library only.
"""

import os
import signal
import socket
import socketserver
import sys
import threading

LANGUAGE = "Python"
BACKLOG = 64
# The proxy refuses a response whose header section exceeds 8192 bytes
# (spec §3.2). The same bound is applied to what we accept, so a peer cannot
# make us grow a buffer without limit (spec §6.2).
MAX_HEADERS = 8192
MAX_BODY = 1 * 1024 * 1024


def html_page(title: str, detail: str) -> bytes:
    return (
        '<!doctype html>\n<html lang="en"><meta charset="utf-8">\n'
        f"<title>{title}</title>\n<h1>{title}</h1>\n<p>{detail}</p>\n</html>\n"
    ).encode()


class Handler(socketserver.BaseRequestHandler):
    """One request per connection (spec §1.3).

    Written against the socket rather than with http.server: BaseHTTPRequestHandler
    would add its own Server and Date headers and its own error pages, which would
    put a framework's conventions between the reader and the contract.
    """

    def handle(self) -> None:
        conn: socket.socket = self.request
        status_body: bytes = self.server.status_body  # type: ignore[attr-defined]

        # Read until the end of the header section.
        buf = b""
        while b"\r\n\r\n" not in buf and len(buf) < MAX_HEADERS:
            try:
                chunk = conn.recv(MAX_HEADERS - len(buf))
            except OSError:
                return
            if not chunk:
                break
            buf += chunk

        if b"\r\n\r\n" not in buf:
            # 400 rather than silence: the proxy reports a dropped connection as
            # 502, which would blame the wrong side.
            self.send(conn, 400, "Bad Request", "text/html; charset=utf-8",
                      html_page("400 Bad Request", "Malformed or oversized request."), False)
            return

        head, _, rest = buf.partition(b"\r\n\r\n")
        lines = head.split(b"\r\n")
        parts = lines[0].split()
        if len(parts) < 2:
            self.send(conn, 400, "Bad Request", "text/html; charset=utf-8",
                      html_page("400 Bad Request", "Unparseable request line."), False)
            return

        method = parts[0].decode("latin-1")
        # The target is NOT percent-decoded by the proxy (spec §2.1). Nothing
        # here uses it to touch the filesystem, so it is matched as received.
        target = parts[1].decode("latin-1")
        head_only = method == "HEAD"

        # Drain exactly Content-Length bytes. Reading beyond blocks until the
        # proxy's timeout (spec §2.6); leaving it unread leaves bytes queued on a
        # connection about to close. Nothing here uses the body.
        length = 0
        for line in lines[1:]:
            name, _, value = line.partition(b":")
            if name.strip().lower() == b"content-length":
                try:
                    length = int(value.strip())
                except ValueError:
                    length = 0
                break
        if 0 < length <= MAX_BODY:
            have = len(rest)
            while have < length:
                try:
                    chunk = conn.recv(min(4096, length - have))
                except OSError:
                    break
                if not chunk:
                    break
                have += len(chunk)

        path = target.split("?", 1)[0]

        if path == "/":
            self.send(conn, 200, "OK", "text/html; charset=utf-8",
                      html_page(f"m6 backend example: {LANGUAGE}",
                                f"A minimal m6 backend written in {LANGUAGE}."), head_only)
        elif path == "/status":
            # Byte-identical across every example: the one file, served verbatim.
            self.send(conn, 200, "OK", "application/json; charset=utf-8", status_body, head_only)
        elif path == "/health":
            self.send(conn, 200, "OK", "text/plain; charset=utf-8", b"ok", head_only)
        elif path == "/boom":
            # Fails on purpose. The proxy counts 5xx as a backend error and may
            # replace it with a styled error page (spec §4).
            self.send(conn, 500, "Internal Server Error", "text/html; charset=utf-8",
                      html_page("500 Internal Server Error",
                                "This endpoint fails on purpose."), head_only)
        else:
            # A real 404, not a 200 with an error page, so the status stays
            # honest to caches and crawlers (spec §4).
            self.send(conn, 404, "Not Found", "text/html; charset=utf-8",
                      html_page("404 Not Found",
                                f"This {LANGUAGE} backend does not serve that path."), head_only)

    @staticmethod
    def send(conn: socket.socket, status: int, reason: str, ctype: str,
             body: bytes, head_only: bool) -> None:
        """Status line, Content-Type with charset, accurate Content-Length, body.

        `head_only` covers spec §3.3: no body for HEAD, 1xx, 204 or 304. The
        Content-Length of the equivalent GET is still sent for HEAD, which is
        what lets the proxy frame the response without waiting for bytes that
        will never arrive.
        """
        head = (
            f"HTTP/1.1 {status} {reason}\r\n"
            f"Content-Type: {ctype}\r\n"
            f"Content-Length: {len(body)}\r\n"
            "Connection: close\r\n"
            "\r\n"
        ).encode()
        try:
            conn.sendall(head if head_only or not body else head + body)
        except OSError:
            # The proxy closed early. Survivable, and not worth a traceback.
            pass


class Server(socketserver.ThreadingUnixStreamServer):
    """Spec §7 requires only that a new connection can be accepted while another
    is being handled, and allows threads, processes or an event loop.

    `daemon_threads` so a shutdown is not held up by a thread blocked on a peer
    that has gone away; `allow_reuse_address` is irrelevant for Unix sockets and
    left off deliberately, since the stale socket is removed explicitly instead.
    """

    daemon_threads = True
    request_queue_size = BACKLOG

    def __init__(self, path: str, status_body: bytes):
        self.status_body = status_body
        super().__init__(path, Handler)

    def server_bind(self) -> None:
        # ── Binding sequence, spec §1.2, in order ────────────────────────────
        # 0. The parent directory SHOULD be created if absent.
        parent = os.path.dirname(self.server_address)  # type: ignore[arg-type]
        if parent:
            os.makedirs(parent, exist_ok=True)
        # 1. Remove any existing file. A stale socket from an unclean exit makes
        #    bind fail with EADDRINUSE.
        try:
            os.unlink(self.server_address)  # type: ignore[arg-type]
        except FileNotFoundError:
            pass
        # 2. Bind.
        super().server_bind()
        # 3. chmod 0666. The proxy runs as a different user and cannot connect
        #    otherwise. The spec calls this the single most common cause of a
        #    backend that starts cleanly and is never contacted.
        os.chmod(self.server_address, 0o666)  # type: ignore[arg-type]
        # 4. listen() with this backlog happens in server_activate().


def main() -> int:
    # Exit 2 means "failed before binding", so a supervisor can tell a
    # misconfiguration from a crash (spec §8.3).
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <socket-path> <status-json-path>", file=sys.stderr)
        return 2
    sock_path, status_path = sys.argv[1], sys.argv[2]

    # The payload is read from the one copy in the repository rather than
    # embedded per language, so byte-identity across the six is structural.
    try:
        with open(status_path, "rb") as f:
            status_body = f.read()
    except OSError as e:
        print(f"cannot read status payload {status_path}: {e}", file=sys.stderr)
        return 2

    try:
        server = Server(sock_path, status_body)
    except OSError as e:
        print(f"cannot listen on {sock_path}: {e}", file=sys.stderr)
        return 1

    # Shutdown, spec §8.2: stop accepting, finish what is in flight, remove the
    # socket, exit 0. A second signal exits immediately.
    stopping = threading.Event()

    def on_signal(signum, frame):  # noqa: ARG001
        if stopping.is_set():
            os._exit(0)
        stopping.set()
        # shutdown() must not run on the signal handler's stack: it waits for
        # serve_forever to return, which is this thread.
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)

    try:
        server.serve_forever(poll_interval=0.05)
    finally:
        server.server_close()
        # Removing the socket is what withdraws this member from the pool. Left
        # behind, the proxy keeps selecting a member that refuses every
        # connection (spec §8.2).
        try:
            os.unlink(sock_path)
        except FileNotFoundError:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
