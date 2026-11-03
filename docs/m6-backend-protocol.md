# m6 Backend Protocol: Specification

**The contract between `m6-http` and a backend.**
It is language agnostic and normative: anything that satisfies it is a valid m6 backend, whatever it is written in.

The contract is the primary interface of the platform, and `m6-core` and `m6-render` are conveniences for writing Rust backends against it.
Neither is required, and neither is allowed to justify a change that would make a non-Rust backend harder to write.

**Conformance language.**
MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are used in the *RFC* (Request for Comments) 2119 sense.
"The proxy" means `m6-http`.
"The backend" means the process implementing this specification.

**Version:** 1, and additions are backward compatible.
Anything else is a new version number.

**Contents**

1. [Transport](#1-transport)
2. [The request](#2-the-request)
3. [The response](#3-the-response)
4. [Status codes](#4-status-codes)
5. [Failure handling](#5-failure-handling)
6. [Security requirements](#6-security-requirements)
7. [Concurrency](#7-concurrency)
8. [Lifecycle](#8-lifecycle)
9. [Minimum viable backend](#9-minimum-viable-backend)

## 1. Transport

A backend receives every request over one Unix domain socket, and this section fixes that socket: its path, the order in which it is bound, and how long a connection lives.
The socket is the whole transport, because the proxy is the only process that connects to it.

The backend MUST listen on a Unix domain socket of type `SOCK_STREAM`.
A Unix socket is not reachable from the network, so a backend on this contract needs no *TLS* (Transport Layer Security) and no TCP listener.

### 1.1. Socket path

The path is declared in `site.toml` as a glob:

```toml
[[backend]]
name    = "my-renderer"
sockets = "/run/m6/my-renderer*.sock"
```

The backend MUST bind a path matching that glob.
The glob exists so a pool can have more than one member, with names such as `my-renderer-1.sock` and `my-renderer-2.sock`, one per service instance.

The proxy discovers members by scanning the glob and watching the directory, so a socket that appears joins the pool and a socket that disappears leaves it, with no restart on either side.
`m6-site-toml.md` owns the `sockets` key and the pool behaviour.

### 1.2. Binding sequence

The backend MUST take these steps in this order.

- **Remove any existing file at the path.**
  A stale socket from an unclean exit makes `bind` fail with `EADDRINUSE`.
- **Bind the socket.**
- **Make the socket reachable by the proxy's user.**
  The proxy's user MUST be able to connect.
  A group used to admit the proxy MUST contain only users trusted as the proxy is trusted, because every user who can connect can send the proxy-owned fields in §2.5.
  With mode `0666` and a `0755` directory, any local user on the host can connect and bypass the proxy.
  The socket is not reachable from the network.
  The m6 `App` default is `0660`, which lets the proxy connect when its user is in the socket's group.
  A backend that runs as its own user SHOULD set `0666` only when the parent directory limits traversal to the same trusted users.
  A backend that any user outside that set can reach MUST NOT trust the fields in §2.2 and §2.3.
  A socket the proxy cannot connect to serves no traffic at all.
- **Listen with a backlog of at least 64.**

The service manager can create the parent directory, for example systemd's `RuntimeDirectory=` with `RuntimeDirectoryMode=0750`.
The m6 `App` creates a missing parent directory before it binds.
A backend that creates the directory itself SHOULD use mode `0750` and run in the proxy's group, because the proxy lists the directory to find sockets.

### 1.3. Connection lifetime

The proxy opens a connection, writes exactly one request, reads exactly one response and closes the connection.
Every request it forwards carries `Connection: close`.

A backend MAY support persistent connections for clients that ask for them.
The backend MUST close the connection after responding to a request that carries `Connection: close` (RFC 9112 §9.6), and MUST handle the proxy closing the connection after each response.
Pipelining does not occur and MUST NOT be relied on.
A backend MAY handle connections concurrently, as §7 sets out.

The socket, its mode and its one-request lifetime are the whole transport contract.
Section 2 gives the request that travels inside it.

## 2. The request

Every forwarded request is HTTP/1.1 in origin-form, whatever protocol the client used.
This section gives the wire format, the fields the proxy owns, and the fields it removes.

### 2.1. Wire format

The syntax is the one in RFC 9112 §3, where *CRLF* (carriage return then line feed) ends each line:

```text
method SP request-target SP HTTP/1.1 CRLF
field-name ":" OWS field-value OWS CRLF
...
CRLF
[ message-body ]
```

The version token is always `HTTP/1.1`, regardless of the version the client used.
A request that arrived over HTTP/2 or HTTP/3 is translated, as §2.4 sets out.

The request target is in origin-form: an absolute path, optionally followed by `?` and a query string.
The proxy forwards the target in the encoded form the client sent, so the backend MUST decode it before use and MUST treat the decoded result as untrusted input (§6.2).

### 2.2. Headers the proxy always sends

Table 1 gives the fields every forwarded request carries.
Take from it that the proxy sets each value itself, after removing any client copy in §2.5.

| Field | Value |
|---|---|
| `Host` | The public host the client used: its own `Host` field, its `:authority`, or the configured site domain when it sent neither |
| `Via` | The protocol version the request arrived on, then the pseudonym `m6`, for example `1.1 m6`. An existing `Via` is kept and the proxy's hop is appended (RFC 9110 §7.6.3) |
| `X-Forwarded-For` | The client's *IP* (Internet Protocol) address, as the proxy attributes it |
| `X-Forwarded-Proto` | `https` on every request |
| `X-Forwarded-Host` | The public host the client used |
| `Content-Length` | Present when the body is non-empty, absent when it is empty |
| `Connection` | `close` |

**Table 1: the fields every forwarded request carries.**
A backend can rely on every row being present on every request.

### 2.3. Headers the proxy can add

Table 2 gives the one field the proxy adds after routing.
Take from it that a protected route is the only place the claims field appears.

| Field | When | Meaning |
|---|---|---|
| `X-Auth-Claims` | The route declares `require` | Verified *JWT* (JSON Web Token) claims, base64-encoded JSON |

**Table 2: the field the proxy adds after routing.**
The value is encoded, so a backend decodes it before reading any claim.

Every other field is forwarded from the client unchanged, in the order the client sent it, except the hop-by-hop and proxy-owned fields in §2.5.
A field named by the client's `Connection` header is also dropped (RFC 9110 §7.6.1).

### 2.4. Version translation

The proxy terminates HTTP/1.1, HTTP/2 and HTTP/3 and forwards all of them as HTTP/1.1, so a backend never sees a frame, a stream identifier, *HPACK* (HTTP/2 header compression), *QPACK* (HTTP/3 header compression) or *QUIC* (the transport under HTTP/3).

The original version is recoverable from `Via`.
It is informational, and a backend SHOULD NOT vary its behaviour on it.

### 2.5. Headers the proxy removes

Hop-by-hop fields (RFC 9110 §7.6.1) are consumed by the proxy and never forwarded:

```text
connection   upgrade   keep-alive   transfer-encoding
te           trailer   proxy-authorization   proxy-connection
```

Proxy-owned fields are stripped from client input before routing, on every protocol, and the proxy re-adds the ones it can vouch for:

```text
x-auth-claims   x-forwarded-for   x-forwarded-proto   x-forwarded-host   x-real-ip
```

The proxy re-emits `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Forwarded-Host` on every request, and `X-Auth-Claims` on a `require` route.
It drops `X-Real-IP` and never re-adds it.

The stripping is a security boundary.
A backend that resolves a repeated field by first match would take a client's forged copy, because the proxy appends its own copy after the client's.
Without stripping, any client could assert `{"groups":["admins"]}`.
A forged `X-Forwarded-For` would defeat per-IP rate limiting by rotating the value.

A value in one of those fields can only have come from the proxy, and MAY be trusted.
That trust holds only while the socket is reachable by the proxy alone (§6.1), so a backend that another path can reach MUST NOT trust them.

### 2.6. Request body

A body is present when `Content-Length` is present and non-zero.
The proxy writes entire bodies and never chunked transfer coding, so `Transfer-Encoding` never appears on a forwarded request.

The backend MUST read exactly `Content-Length` bytes.
A read beyond that waits for bytes that will never arrive, until the proxy's timeout expires.

Those are the fields a backend parses, and the only ones it can trust.
Section 3 gives the response it writes.

## 3. The response

The proxy validates every response before a client sees it, and framing is the part it validates hardest.
This section gives the wire format, the three framing choices, the responses that carry no body, and the headers a backend sends.

### 3.1. Wire format

The syntax is the one in RFC 9112 §4:

```text
HTTP/1.1 SP status-code SP [reason-phrase] CRLF
field-name ":" OWS field-value OWS CRLF
...
CRLF
[ message-body ]
```

The status line MUST begin `HTTP/1.1`.
The reason phrase MAY be empty, and the proxy does not interpret it.

### 3.2. Framing

The proxy refuses a malformed response when it cannot tell where the response ends, because a recipient that guesses is the request-smuggling primitive: the next hop can guess differently.

A backend MUST do exactly one of these:

- send `Content-Length` with the exact byte count of the body.
- send `Transfer-Encoding: chunked` and a correctly framed chunked body.
- send neither and signal the end of the body by closing the connection.

A backend SHOULD choose the first: it is the simplest to produce correctly, and the reference examples use it.

The proxy refuses a response that:

- carries both `Transfer-Encoding` and `Content-Length`, which RFC 9112 §6.1 forbids.
- carries two `Content-Length` fields with different values.
- carries a `Content-Length` that is not a valid non-negative integer.
- carries a `Transfer-Encoding` whose final coding is not `chunked`, which leaves the response unframed.
- has a header section larger than 8192 bytes.
- declares or delivers a body larger than 128 MiB.

A refused response reaches the client as a backend error (§5).

`Transfer-Encoding` is parsed as a comma-separated list, so `gzip, chunked` is chunked.
A backend MAY send it and SHOULD prefer `Content-Length`.
A chunked body MUST terminate every chunk with CRLF and end with a terminated trailer section.

### 3.3. Responses that must not have a body

Per RFC 9110, the backend MUST NOT send a body when the request method was `HEAD`, or when the status is `1xx`, `204` or `304`.

For `HEAD`, the backend SHOULD send the `Content-Length` the equivalent `GET` would have produced.
The proxy knows the request method and does not wait for body bytes.

The proxy stops reading at the header section for these responses and closes the connection, so a body sent anyway is discarded.

### 3.4. Content type

The backend SHOULD send `Content-Type` on every response with a body, and MUST include a `charset` parameter on textual types.

The charset is part of the representation.
A browser falls back to Latin-1 for a textual type without one, so correct UTF-8 bytes render as mojibake and the missing label is the only fault.

### 3.5. Caching

The backend MAY send `Cache-Control`, and the proxy applies RFC 9111 semantics to it.

The proxy stores a response when the backend marks it `Cache-Control: public` and the status is storable.
A backend that sends nothing is treated as uncacheable, so a backend serving public content SHOULD send `Cache-Control: public`.

### 3.6. Compression

Compression is the backend's job.
The proxy is a cache: `m6-http` negotiates between and caches the representations a backend produced, and nothing under `m6-http/src` compresses.

A backend MAY compress.
If it does, it MUST send an accurate `Content-Encoding` and MUST honour `Accept-Encoding`, including `q=0` meaning not acceptable.

A backend that does not compress serves one representation, which is a legitimate choice for a service that would rather spend its cycles elsewhere.
Either way it MUST declare which, in config, so the proxy and the backend agree.
Section 3.6.1 gives the key.

### 3.6.1. Declaring it: `[[backend]] compresses`

`site.toml` is read by both sides:

```toml
[[backend]]
name       = "my-renderer"
sockets    = "/run/m6/my-renderer*.sock"
compresses = false          # default: true
```

The proxy uses `compresses` to decide whether to advertise an encoding dimension.
A backend that does not compress has one representation, so `Vary: Accept-Encoding` would promise variants that can never exist, and every shared cache downstream would fragment its storage on a field that cannot change the body.
The proxy leaves a `Vary` the backend sent itself alone.

The backend reads the same key and MUST refuse to start when it disagrees with what it can do.
An `m6-core` service gets this from `m6_core::compress::check_declared_support`, called before binding, and exits `2` per §8.3.
A backend in another language SHOULD do the equivalent.
Table 3 gives the two directions.
Take from it that a false declaration is worse in one direction than the other.

| declared | reality | what goes wrong |
|---|---|---|
| `true` | compresses nothing | The proxy advertises variants that never exist, and downstream caches fragment for nothing |
| `false` | compresses | The proxy stops varying on `Accept-Encoding`, so a compressed body can be cached and replayed to a client that asked for identity and cannot read it |

**Table 3: the two directions a compression declaration can be wrong.**
A `false` declaration is the dangerous one because the client that cannot read the body has no way to ask again.

The default is `true`, because an `m6-core` backend compresses unless told not to.
A `site.toml` written before the key existed keeps working and keeps agreeing.

Those are the rules a response satisfies.
Section 4 gives the status codes the proxy treats specially.

## 4. Status codes

A backend chooses its own status codes.
The proxy passes a status below `400` through, and it handles a status of `400` or above according to `[errors] mode`.
This section gives that split and the rule for a path a backend does not serve.

Table 4 gives the treatment by range.
Take from it that the status is always the backend's and the body of an error is the proxy's.

| Range | Proxy behaviour |
|---|---|
| `5xx` | Counted as a backend error, and the body is replaced according to `[errors] mode` |
| `4xx` | The status reaches the client and the body is replaced according to `[errors] mode` |
| below `400` | Passed through |

**Table 4: status ranges and how the proxy handles each.**
`m6-site-toml.md` gives the `[errors]` values, and no value delivers the backend's own error body to the client.

A backend SHOULD return `404` for a path it does not serve, so the status is honest to caches and crawlers.
A `200` with an error page hides a missing resource from both.

Those are the statuses the proxy handles for the backend.
Section 5 gives the conditions under which no response arrives.

## 5. Failure handling

The proxy turns every backend failure into a status the client can act on.
This section gives the proxy's response to each failure, because a backend author needs to know which failures are survivable.

Table 5 gives the conditions.
Take from it that every transport-level failure reaches the client as a `502`, whatever its cause.

| Condition | Proxy behaviour |
|---|---|
| Socket does not exist | The member is absent from the pool. Another member serves the request, or the client receives `502` |
| `connect` refused | `502`, and the member is backed off for 1 to 30 seconds |
| No response within `backend_timeout_secs` (default **30**) | The connection is dropped and the client receives `502` |
| Malformed status line or headers | `502` |
| Framing violation (§3.2) | `502` |
| Response exceeds 128 MiB | `502` |
| Connection closed before a complete response | `502` |
| Backend returns `4xx` or `5xx` | The status reaches the client and the body is replaced according to `[errors] mode` (§4) |

**Table 5: backend conditions and the response the proxy sends.**
A slow backend and a refused connection produce the same `502`, so a client cannot tell them apart.

The proxy does not retry a request against another member.
A backend SHOULD fail fast, because a prompt `500` reaches the client sooner than a connection held for 30 seconds.

Those are the failures a backend can survive and the ones that end the request.
Section 6 gives the security rules that keep the socket trustworthy.

## 6. Security requirements

Two boundaries keep the protocol safe: the socket is reachable only by the proxy, and every request field outside §2.2 and §2.3 is attacker-controlled.
This section gives both boundaries and the header-injection rule that follows.

### 6.1. Trust boundary

The backend socket MUST be reachable only by the proxy.
The permissions on the socket's directory are the mechanism, and everything in §2.5 depends on them.

### 6.2. Untrusted input

Everything in a request except the fields in §2.2 and §2.3 is attacker-controlled: the request target, the query string, and every client-supplied field.

A backend MUST:

- reject or sanitise path traversal (`..`, encoded variants, absolute paths) before using any request-derived value in a filesystem path.
- treat the request target as bytes until it has validated them, because the proxy passes the target through in the encoded form the client sent.
- bound anything derived from the request that it allocates.

### 6.3. Header injection

A backend MUST NOT copy a request-derived value into a response field without validating it.
A CR or LF in a field value terminates the header section early and injects an attacker-controlled response.

The proxy performs the equivalent check in the other direction and refuses a request whose fields would inject HTTP/1.1 framing.

Those rules hold whatever language the backend is written in.
Section 7 gives the concurrency the socket supports.

## 7. Concurrency

A backend is required to accept work while it is busy, and the proxy's pool decides which member gets the next connection.
This section gives the pool's rule, the reference concurrency model and the way a pool grows.

The proxy picks the member with the fewest in-flight connections.

A backend MUST be able to accept a new connection while it is handling another.
It MAY use threads, processes or an event loop.

The reference model, given in `m6-architecture.md` §7.1, is a fixed thread pool with a bounded queue: the pool size defaults to the *CPU* (central processing unit) count, the queue depth to eight times the pool size, and a full queue answers `503` at once.
Returning `503` under overload is correct behaviour, because it is how backpressure reaches the proxy.

A pool scales by adding members, with more sockets and more service instances.
One process keeps its fixed pool.

That is the concurrency a member provides and the way a pool grows.
Section 8 gives the lifecycle that puts a member in the pool and takes it out.

## 8. Lifecycle

A member joins the pool when its socket appears and leaves when the socket goes.
This section gives startup, shutdown, exit codes and the reload that happens without leaving the pool at all.

### 8.1. Startup

Bind before announcing readiness.
The socket appearing in the directory is what adds the backend to the pool, so a backend that creates the socket before it can serve receives requests it cannot answer.

### 8.2. Shutdown

On `SIGTERM` or `SIGINT`, the backend MUST take these steps in order:

- **Stop accepting new connections.**
- **Finish requests already in flight.**
- **Remove the socket file.**
- **Exit with status `0`.**

A second `SIGTERM` or `SIGINT` MUST exit immediately.
Removing the socket withdraws the member from the pool, and a backend that exits leaving the file behind causes the proxy to keep selecting a member that refuses every connection.

### 8.3. Exit codes

Table 6 gives the three codes.
Take from it that a supervisor learns from the code whether the process ever bound.

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error |
| `2` | Configuration or usage error, detected before binding |

**Table 6: the three exit codes.**
Code `2` means the process failed before binding, so a supervisor can tell a misconfiguration from a crash.

### 8.4. Configuration reload

A backend MAY watch `site.toml` and its own configuration file and reload without restarting.
This step is OPTIONAL.
A restart is a valid implementation, because the pool handles the socket disappearing and reappearing.

Those are the four points at which a member enters or leaves service.
Section 9 collects every MUST into one checklist.

## 9. Minimum viable backend

A backend that meets this checklist satisfies the contract.
This section collects every MUST in the document and names the work the proxy keeps for itself.

Everything a backend MUST do:

- [ ] Remove the stale socket and bind a Unix `SOCK_STREAM` socket matching the glob.
- [ ] Make the socket reachable by the proxy's user, with `0660` and a trusted group or `0666` behind a trusted directory.
- [ ] Accept connections and serve requests.
- [ ] Parse an HTTP/1.1 request line and header section.
- [ ] Read exactly `Content-Length` body bytes, if any.
- [ ] Emit an `HTTP/1.1` status line.
- [ ] Emit an accurate `Content-Length`.
- [ ] Emit `Content-Type` with a charset on text.
- [ ] Send no body for `HEAD`, `1xx`, `204` or `304`.
- [ ] Close the connection after responding to a request with `Connection: close`.
- [ ] On `SIGTERM`, drain, unlink the socket and exit `0`.

There is no requirement to implement TLS, HTTP/2, HTTP/3, chunked encoding, caching, authentication or connection reuse.
The proxy terminates TLS and the newer protocols, caches responses and verifies tokens.
Compression is the exception: §3.6 makes it the backend's job, and `m6-http` has no compressor.

The checklist is small enough to implement from scratch in any language.
`m6-backend-examples.md` gives six reference implementations, all written against this document.

A backend that binds one socket, answers each HTTP/1.1 request with correct framing and drains on `SIGTERM` satisfies this version of the contract.
A later version adds to it without breaking what this version promises.
