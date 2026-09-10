#!/usr/bin/env python3
"""Cleartext TCP -> TLS bridge, so a plaintext HTTP tester can reach the :443 engine.

Plumbing only. It terminates nothing of HTTP: bytes in one side go out the
other unchanged, wrapped in TLS on the upstream leg. It must not normalise,
buffer by line, or reframe anything, or it would hide the parser behaviour
under test.

ALPN is pinned to http/1.1. Without that the server negotiates h2 and every
byte sequence the tester sends means something entirely different.

**A client half-close is NOT forwarded to the TLS leg.** `shutdown(SHUT_WR)`
on the socket underneath an `SSLSocket` destroys the session, so a tester that
half-closes after its request -- which h1spec does, and which is ordinary
client behaviour -- got nothing back. Measured through this bridge before the
fix: 924 bytes with the write side open, 0 bytes with it half-closed, and a
conformance score of 6/32 that was entirely the harness's fault.

A client half-close means "no more request bytes", which for HTTP means the
request is complete. The bridge simply stops forwarding and waits for the
response; the server answers and closes, which ends the other pump.

    python3 tls_bridge.py 18095 127.0.0.1 10443
"""
import socket
import ssl
import sys
import threading

port = int(sys.argv[1])
up_host = sys.argv[2]
up_port = int(sys.argv[3])

ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE
ctx.set_alpn_protocols(["http/1.1"])


def pump(src, dst, half_close_dst):
    """Copy src -> dst. Half-close dst's write side only if it is a raw socket."""
    try:
        while True:
            b = src.recv(65536)
            if not b:
                break
            dst.sendall(b)
    except OSError:
        pass
    if half_close_dst:
        try:
            dst.shutdown(socket.SHUT_WR)
        except (OSError, ValueError):
            pass


def serve(client):
    try:
        raw = socket.create_connection((up_host, up_port), timeout=10)
        up = ctx.wrap_socket(raw, server_hostname="localhost")
    except OSError:
        client.close()
        return
    # client -> upstream: never half-close the TLS socket (see the note above).
    t = threading.Thread(target=pump, args=(client, up, False), daemon=True)
    t.start()
    # upstream -> client: a raw socket, where half-close is exactly right and
    # tells the tester the response is complete.
    pump(up, client, True)
    t.join(timeout=5)
    for s in (client, up):
        try:
            s.close()
        except OSError:
            pass


srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(64)
print(f"tls bridge :{port} -> {up_host}:{up_port} (alpn http/1.1)", flush=True)
while True:
    c, _ = srv.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
