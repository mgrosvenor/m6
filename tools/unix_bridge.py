#!/usr/bin/env python3
"""TCP -> unix-socket bridge, so a cleartext HTTP tester can reach an m6 backend.

Plumbing only: it copies bytes in both directions and interprets nothing. It
must not normalise, buffer by line, or reframe anything, or it would hide the
parser behaviour under test.

**Half-close, not full close.** The first version called
`shutdown(SHUT_RDWR)` on both sockets as soon as either direction ended. A
tester that finishes sending and then waits for the response had its response
path torn down underneath it, and every test reported "no response". That
looked exactly like the server failing, and produced a conformance score that
was entirely the harness's fault. When one direction ends, shut down only the
write side of the peer and let the other direction drain.

    python3 bridge.py 18090 /run/m6/m6-file.sock
"""
import socket
import sys
import threading

port = int(sys.argv[1])
target = sys.argv[2]


def pump(src, dst):
    """Copy src -> dst until src is done, then half-close dst's write side."""
    try:
        while True:
            b = src.recv(65536)
            if not b:
                break
            dst.sendall(b)
    except OSError:
        pass
    try:
        dst.shutdown(socket.SHUT_WR)
    except OSError:
        pass


def serve(client):
    try:
        up = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        up.connect(target)
    except OSError:
        client.close()
        return
    t = threading.Thread(target=pump, args=(client, up), daemon=True)
    t.start()
    pump(up, client)          # returns when the server is done responding
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
print(f"bridge :{port} -> {target}", flush=True)
while True:
    c, _ = srv.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
