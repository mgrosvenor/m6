"""Time N requests against a unix socket and print the median in nanoseconds.

A fresh connection per request. Its cost is the same for every build, so it
cancels out of the comparison, and it avoids the request-per-connection cap
that a keep-alive client has to work around.
"""
import socket, sys, time, statistics

sock_path, path, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
REQ = ("GET %s HTTP/1.1\r\nHost: x\r\nAccept-Encoding: identity\r\n"
       "Connection: close\r\n\r\n" % path).encode()

def once():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(10)
    t = time.perf_counter_ns()
    s.connect(sock_path)
    s.sendall(REQ)
    while s.recv(65536):
        pass
    el = time.perf_counter_ns() - t
    s.close()
    return el

try:
    for _ in range(30):
        once()
    samples = [once() for _ in range(n)]
except OSError:
    sys.exit(1)
print(int(statistics.median(samples)))
