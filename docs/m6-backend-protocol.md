# m6 Backend Protocol — Specification

**The contract between `m6-http` and a backend.** Language agnostic and
normative. Anything that satisfies this document is a valid m6 backend,
whatever it is written in.

This is the primary interface of the platform. `m6-core` and `m6-render` are
conveniences for writing Rust backends against it; neither is required, and
neither may be used to justify a change that would make a non-Rust backend
harder to write.

**Conformance language.** MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are used
in the RFC 2119 sense. "The proxy" means `m6-http`. "The backend" means the
process implementing this specification.

**Version:** 1. Additions will be backward compatible; anything else is a new
version number.

---

## 1. Transport

The backend MUST listen on a **Unix domain socket** of type `SOCK_STREAM`.

There is no TCP listener, no TLS, and no network exposure. The proxy is the
only process that connects to the socket.

### 1.1 Socket path

The path is declared in `site.toml` as a glob:

```toml
[[backend]]
name    = "my-renderer"
sockets = "/run/m6/my-renderer*.sock"
```

The backend MUST bind a path matching that glob. The glob exists so a pool can
have several members: `my-renderer-1.sock`, `my-renderer-2.sock` and so on,
typically one per systemd instance.

The proxy discovers sockets by scanning the glob and watching the directory. A
socket appearing is added to the pool; a socket disappearing is removed. No
restart or configuration reload is needed on either side.

### 1.2 Binding sequence

The backend MUST, in this order:

1. **Remove any existing file at the path.** A stale socket from an unclean
   exit will otherwise cause `bind` to fail with `EADDRINUSE`.
2. **Bind** the socket.
3. **Set permissions to `0666`.** The proxy runs as a different user (`m6` in
   the reference deployment) and cannot connect otherwise. This is the single
   most common cause of a backend that starts cleanly and is never contacted.
4. **Listen**, with a backlog SHOULD be at least 64.

The backend SHOULD create the parent directory if it does not exist.

### 1.3 Connection lifetime

**One request per connection.** The proxy opens a connection, writes exactly
one request, reads exactly one response, and closes.

The backend MUST NOT expect connection reuse and MUST NOT keep the connection
open after responding. Pipelining does not occur and MUST NOT be relied on.

A backend MAY handle connections concurrently. See §7.

---

## 2. The request

### 2.1 Wire format

Standard HTTP/1.1 request syntax, RFC 9112 §3:

```
method SP request-target SP HTTP/1.1 CRLF
field-name ":" OWS field-value OWS CRLF
...
CRLF
[ message-body ]
```

The version token is always `HTTP/1.1`, regardless of the version the client
used. A request that arrived over HTTP/2 or HTTP/3 is translated; see §2.4.

The request-target is in **origin-form**: an absolute path, optionally followed
by `?` and a query string. It is **not** percent-decoded by the proxy. The
backend MUST decode it itself if it needs to, and MUST treat the decoded result
as untrusted input (§6.2).

### 2.2 Headers the proxy always sends

| Field | Value |
|---|---|
| `Host` | The public host as the client sent it |
| `Via` | The protocol version of the hop the proxy received, per RFC 9110 §7.6.3 |
| `X-Forwarded-For` | The client's IP address |
| `X-Forwarded-Proto` | The scheme the client used, normally `https` |
| `X-Forwarded-Host` | The original public host |
| `Content-Length` | Present if and only if there is a body |
| `Connection` | `close` |

### 2.3 Headers the proxy may send

| Field | When | Meaning |
|---|---|---|
| `x-auth-claims` | The route declares `require` | Verified JWT claims as JSON |

Every other header is forwarded from the client unchanged, in the order the
client sent it, except those in §2.5.

### 2.4 Version translation

The proxy terminates HTTP/1.1, HTTP/2 and HTTP/3 and forwards all of them as
HTTP/1.1. A backend never sees a frame, a stream identifier, HPACK, QPACK or
QUIC.

The original version is recoverable from `Via`. It is informational: a backend
SHOULD NOT vary its behaviour on it.

### 2.5 Headers the proxy removes

**Hop-by-hop fields** (RFC 9110 §7.6.1) are consumed by the proxy and never
forwarded:

```
connection   upgrade   keep-alive   transfer-encoding
te           trailer   proxy-authorization   proxy-connection
```

**Proxy-owned fields** are stripped from client input *before routing*, on
every protocol, and then re-added by the proxy with values it can vouch for:

```
x-auth-claims   x-forwarded-for   x-forwarded-proto
x-forwarded-host   x-real-ip
```

This is a security boundary, not tidiness. Backends resolve a repeated header
by first match, and the proxy appends its own copy after the client's, so
without stripping, a forged `x-auth-claims` would win and any client could
assert `{"groups":["admins"]}`. A forged `x-forwarded-for` would likewise
defeat per-IP rate limiting by rotating the value.

**Consequence for the backend:** a value in one of those fields can only have
come from the proxy, and MAY be trusted. That trust is valid **only** because
the socket is unreachable except by the proxy. A backend that is also reachable
by any other path MUST NOT trust them.

### 2.6 Request body

Present if and only if `Content-Length` is present and non-zero. The proxy does
not use chunked transfer coding towards backends; `Transfer-Encoding` is never
sent.

The backend MUST read exactly `Content-Length` bytes. Reading beyond that will
block until the proxy's timeout expires.

---

## 3. The response

### 3.1 Wire format

Standard HTTP/1.1 response syntax, RFC 9112 §4:

```
HTTP/1.1 SP status-code SP [reason-phrase] CRLF
field-name ":" OWS field-value OWS CRLF
...
CRLF
[ message-body ]
```

The status line MUST begin `HTTP/1.1`. The reason phrase MAY be empty; it is
not interpreted.

### 3.2 Framing

**This is the part most worth getting right.** The proxy validates framing
strictly and refuses malformed responses rather than guessing, because a
recipient that guesses is the classic request-smuggling primitive: the next hop
may guess differently.

A backend MUST do exactly one of:

- send `Content-Length` with the exact byte count of the body, or
- send `Transfer-Encoding: chunked` and a correctly framed chunked body, or
- send neither, and signal the end of the body by closing the connection.

The first is strongly RECOMMENDED. It is the simplest to produce correctly, and
it is what the reference examples do.

The proxy **refuses** a response that:

- carries both `Transfer-Encoding` and `Content-Length` (RFC 9112 §6.1 forbids
  sending both);
- carries two `Content-Length` fields with different values;
- carries a `Content-Length` that is not a valid non-negative integer;
- has a header section larger than **8192 bytes**;
- declares or delivers a body larger than **128 MiB**.

A refused response is reported to the client as a backend error (§5).

`Transfer-Encoding` is parsed as a comma-separated list, so `gzip, chunked` is
recognised as chunked. A backend MAY send it, but SHOULD prefer
`Content-Length`.

### 3.3 Responses that must not have a body

Per RFC 9110, the backend MUST NOT send a body when:

- the request method was `HEAD`, or
- the status is `1xx`, `204`, or `304`.

For `HEAD`, the backend SHOULD still send the `Content-Length` the equivalent
`GET` would have produced. The proxy knows the request method and will not wait
for body bytes.

Sending a body in these cases will cause the proxy to mis-frame the response or
stall until timeout.

### 3.4 Content type

The backend SHOULD send `Content-Type` on every response with a body, and MUST
include a `charset` parameter on textual types.

Omitting the charset is not cosmetic. Clients fall back to Latin-1 and render
UTF-8 as mojibake; this has happened in production, to every Markdown file and
to `llms.txt`, with correct bytes and a missing label.

### 3.5 Caching

The backend MAY send `Cache-Control`. The proxy honours it and applies RFC 9111
semantics.

A backend that sends nothing is treated as uncacheable, which is safe and
usually wrong for static content. A backend serving public, cacheable content
SHOULD say so.

### 3.6 Compression

The backend SHOULD NOT compress its response, and SHOULD ignore
`Accept-Encoding`.

The proxy performs content negotiation and compression itself, caches each
representation, and reuses it across clients. A backend that compresses
duplicates that work per request and prevents the proxy from serving a
different encoding from cache.

A backend that does compress MUST send an accurate `Content-Encoding` and MUST
honour `Accept-Encoding` correctly, including `q=0` meaning "not acceptable".

---

## 4. Status codes

The backend chooses its own status codes. The proxy interprets two ranges
specially:

| Range | Proxy behaviour |
|---|---|
| `5xx` | Counted as a backend error. May be replaced with a styled error page depending on `[errors] mode` |
| everything else | Passed through |

A backend SHOULD return `404` for a path it does not serve, rather than `200`
with an error page, so the status is honest to caches and crawlers.

---

## 5. Failure handling

What the proxy does when a backend misbehaves. A backend implementer needs this
to know which failures are survivable.

| Condition | Proxy behaviour |
|---|---|
| Socket does not exist | Member not in the pool; another member is used, or `502` |
| `connect` refused | `502`, member marked unhealthy |
| No response within `backend_timeout_secs` (default **30**) | Connection dropped, `504` |
| Malformed status line or headers | `502` |
| Framing violation (§3.2) | `502` |
| Response exceeds 128 MiB | `502` |
| Connection closed before a complete response | `502` |
| Backend returns `5xx` | Passed through or replaced by a styled error page |

The proxy does not retry a request against another pool member. A backend
SHOULD therefore fail fast rather than hang: a prompt `500` is a better outcome
than occupying a connection for 30 seconds.

---

## 6. Security requirements

### 6.1 Trust boundary

The backend socket MUST NOT be reachable other than by the proxy. Filesystem
permissions on `/run/m6` are the mechanism. Everything in §2.5 depends on it.

### 6.2 Untrusted input

Everything in the request except the fields in §2.2 and §2.3 is attacker
controlled: the target, the query string, and every client-supplied header.

A backend MUST:

- reject or sanitise path traversal (`..`, encoded variants, absolute paths)
  before using any request-derived value in a filesystem path;
- treat the request target as bytes until it has validated them, since it is
  not percent-decoded by the proxy;
- bound anything derived from the request that it allocates.

### 6.3 Header injection

A backend MUST NOT copy request-derived values into response header fields
without validating them. A CR or LF in a field value would terminate the header
section early and inject an attacker-controlled response.

The proxy performs the equivalent check in the other direction and refuses to
forward a request whose header values would inject HTTP/1.1 framing.

---

## 7. Concurrency

The proxy performs least-connections load balancing across the members of a
pool and may have several requests in flight to one member at once.

A backend MUST be able to accept a new connection while handling another. It
MAY do so with threads, processes, or an event loop.

The reference model, per `m6-decisions.md`, is a **fixed thread pool with a
bounded queue**: pool size defaults to the CPU count, queue depth to pool size
times eight, and a full queue returns `503` immediately rather than queueing
without limit. Returning `503` under overload is correct behaviour and is how
backpressure reaches the proxy.

Scaling is by adding pool members (more sockets, more systemd instances) rather
than by growing one process.

---

## 8. Lifecycle

### 8.1 Startup

Bind before announcing readiness. The socket appearing in the directory is what
adds the backend to the pool, so a backend that creates the socket before it can
serve will receive requests it cannot answer.

### 8.2 Shutdown

On `SIGTERM` or `SIGINT`, the backend MUST:

1. Stop accepting new connections.
2. Finish requests already in flight.
3. Remove its socket file.
4. Exit with status `0`.

A second `SIGTERM` or `SIGINT` MUST exit immediately.

Removing the socket is what withdraws the member from the pool. A backend that
exits leaving the file behind causes the proxy to keep selecting a member that
refuses every connection.

### 8.3 Exit codes

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error |
| `2` | Configuration or usage error, detected before binding |

`2` specifically means "failed before binding", so a supervisor can distinguish
a misconfiguration from a crash.

### 8.4 Configuration reload

A backend MAY watch `site.toml` and its own configuration file and reload
without restarting. This is OPTIONAL. Restarting is a valid implementation, and
the socket disappearing and reappearing is handled by the pool.

---

## 9. Minimum viable backend

Everything a backend MUST do, as a checklist:

- [ ] Remove stale socket, bind a Unix `SOCK_STREAM` socket matching the glob
- [ ] `chmod 0666`
- [ ] Accept connections, one request each
- [ ] Parse an HTTP/1.1 request line and header section
- [ ] Read exactly `Content-Length` body bytes, if any
- [ ] Emit an `HTTP/1.1` status line
- [ ] Emit an accurate `Content-Length`
- [ ] Emit `Content-Type` with a charset on text
- [ ] Send no body for `HEAD`, `1xx`, `204`, `304`
- [ ] Close the connection
- [ ] On `SIGTERM`: drain, unlink the socket, exit `0`

There is no requirement to implement TLS, HTTP/2, HTTP/3, chunked encoding,
caching, compression, authentication, or connection reuse. The proxy owns all
of it.

This is deliberately small enough to implement from scratch in any language.
`m6-backend-examples.md` describes reference implementations, all of which are
written against this document.
