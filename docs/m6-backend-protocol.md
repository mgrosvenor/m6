# m6 Backend Protocol: Specification

**The contract between `m6-http` and a backend.**
It is language agnostic and normative: anything that satisfies it is a valid m6 backend, whatever it is written in.

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

The backend MUST listen on a Unix domain socket of type `SOCK_STREAM`.
A backend needs no *TLS* (Transport Layer Security) and no TCP listener.

### 1.1. Socket path

The path is declared in `site.toml` as a glob:

```toml
[[backend]]
name    = "my-renderer"
sockets = "/run/m6/my-renderer*.sock"
```

The backend MUST bind a path matching that glob.
The glob exists so a pool can have more than one member, with names such as `my-renderer-1.sock` and `my-renderer-2.sock`, one per service instance.

`m6-site-toml.md` owns the `sockets` key and the pool behaviour.
The proxy expands the glob once at start (`BackendPool::expand_glob` in `m6-http/src/pool.rs`), and a start where the glob matches no socket stops with an error.

By default, an `m6-auth-server` listens on two sockets.
The proxy treats one of them as the authority, so the split is part of this contract.
The primary is the service's socket, `/run/m6/<config file stem>.sock` by default (`socket_path_from_config` in `m6-core/src/server.rs`).
The check socket sits beside it as `<parent>/check-<primary file name>` (`check_socket_path` in `m6-auth-server/src/main.rs`), so `/run/m6/m6-auth.sock` pairs with `/run/m6/check-m6-auth.sock`.
The four login routes answer on the primary socket and `POST /check` answers on the check socket.
A route that belongs to the other socket answers the house `404` (`run_route` in `m6-auth-server/src/main.rs`), the same answer an unknown path gets.
A `[auth] backend` whose glob matches the primary socket gets `404` from that member, and the start probe stops the proxy (§2.7).

### 1.2. Binding sequence

The backend MUST take these steps in this order.

- **Remove any existing file at the path.**
  A stale socket from an unclean exit makes `bind` fail with `EADDRINUSE`.
- **Bind the socket.**
- **Make the socket reachable by the proxy's user.**
  The proxy's user MUST be able to connect.
  A group used to admit the proxy MUST contain only users trusted as the proxy is trusted, because every user who can connect can send the proxy-owned fields in §2.5.
  With mode `0666` and a `0755` directory, any local user on the host can connect and bypass the proxy.
  The m6 service default is `0660`, which lets the proxy connect when its user is in the socket's group.
  A backend that runs as its own user SHOULD set `0666` only when the parent directory limits traversal to the same trusted users.
- **Listen with a backlog of at least 64.**

The service manager can create the parent directory, for example systemd's `RuntimeDirectory=` with `RuntimeDirectoryMode=0750`.
An m6 service creates a missing parent directory before it binds.
A backend that creates the directory itself SHOULD use mode `0750` and run in the proxy's group, because the proxy expands the glob in that directory at start.

### 1.3. Connection lifetime

The proxy opens a connection, writes exactly one request, reads exactly one response and closes the connection.
Every request it forwards carries `Connection: close`.

A backend MAY support persistent connections for clients that ask for them.
The backend MUST close the connection after responding to a request that carries `Connection: close` (RFC 9112 §9.6), and MUST handle the proxy closing the connection after each response.
Pipelining does not occur and MUST NOT be relied on.
A backend MAY handle connections concurrently, as §7 sets out.

Section 2 gives the request that travels over the socket.

## 2. The request

Every forwarded request is HTTP/1.1 in origin-form, whatever protocol the client used.
This section gives the wire format, the fields the proxy owns, the fields it removes, the credential check and the reserved path `/genid`.

### 2.1. Wire format

The syntax is the one in RFC 9112 §3, where *CRLF* (carriage return then line feed) ends each line:

```text
method SP request-target SP HTTP/1.1 CRLF
field-name ":" OWS field-value OWS CRLF
...
CRLF
[ message-body ]
```

The request target is in origin-form: an absolute path, optionally followed by `?` and a query string.
The proxy forwards the target in the encoded form the client sent, so the backend MUST decode it before use and MUST treat the decoded result as untrusted input (§6.2).

### 2.2. Headers the proxy always sends

Table 1 gives the fields every forwarded request carries.
Take from it that `Host` is the client's value, forwarded unchanged when the client sent one, and that every other row is the proxy's statement about the request.

| Field | Value |
|---|---|
| `Host` | The public host the client used: its own `Host` field, its `:authority`, or the configured site domain when it sent neither |
| `Via` | The protocol version the request arrived on, then this node's `[node] name`, for example `1.1 edge-a`. An entry a trusted cache node sent on the backbone is kept in front of it, and the list stays in path order (RFC 9110 §7.6.3) |
| `X-Forwarded-For` | The client's *IP* (Internet Protocol) address, as the proxy attributes it |
| `X-Forwarded-Proto` | `https` on every request |
| `X-Forwarded-Host` | The public host the client used |
| `Content-Length` | Present when the body is non-empty, absent when it is empty |
| `Connection` | `close` |

**Table 1: the fields every forwarded request carries.**
`Host` is client-supplied and attacker-controlled (§2.5).

### 2.3. Headers the proxy can add

Table 2 gives the one field the proxy adds after routing.
Take from it that a protected route is the only place the claims field appears.

| Field | When | Meaning |
|---|---|---|
| `X-Auth-Claims` | The route declares `require` | The identity the authority answered for this request: `sub`, `username`, `roles` and `groups`, base64-encoded JSON |

**Table 2: the field the proxy adds after routing.**
The value holds the four named fields and nothing else: the proxy forwards the identity the authority answered, so the token's `exp`, `iss` and any extra claim do not appear.
The encoder writes exactly those keys (`encode_claims_header` in `m6-core/src/auth.rs`), and the backend-side parser reads them into the `auth_*` request-dictionary keys (`parse_auth_claims` in `m6-core/src/request.rs`).
Every `require` request that reaches a backend has passed the check, so it carries the field (§2.7).

Every field not in Table 1 or Table 2 is forwarded from the client unchanged, in the order the client sent it, except the hop-by-hop and proxy-owned fields in §2.5.
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

Proxy-owned fields are stripped from client input before routing, on every protocol:

```text
content-length   x-auth-claims   x-forwarded-for   x-forwarded-proto   x-forwarded-host   x-real-ip
```

The proxy re-emits `Content-Length` from the body it sends, `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Forwarded-Host` on every request, and `X-Auth-Claims` on a `require` route.
It drops `X-Real-IP` and never re-adds it.

`Via` is the one proxy-owned field whose strip depends on the listener.
A public listener drops the client's copy, because a client must not be able to name a node, and then writes this node's own entry.
An h2c listener named in `[server] internal_binds` keeps the entry a peer in `[server] internal_peers` sends and appends this node's entry after it.
The trust is the one that decides whether a forwarded client address is believed, so a listener that refuses a peer's `X-Forwarded-For` refuses its `Via` too.

The proxy forwards the client's `Host` unchanged (§2.2), so a backend MUST treat `Host` as attacker-controlled.

The stripping is a security boundary.
The proxy appends its own copy after any client copy, so without stripping a backend that resolves a repeated field by first match would take the client's forged value.
A forged `X-Auth-Claims` would assert any group, and a forged `X-Forwarded-For` would defeat per-IP rate limiting.

A value in one of the proxy-owned fields can only have come from the proxy, and MAY be trusted.
That trust holds only while the socket is reachable by the proxy alone (§6.1), so a backend that another path can reach MUST NOT trust them.

### 2.6. Request body

A body is present when `Content-Length` is present and non-zero.
The proxy writes the whole body under `Content-Length`, so `Transfer-Encoding` never appears on a forwarded request.

The backend MUST read exactly `Content-Length` bytes.
A read beyond that waits for bytes that will never arrive, until the proxy's timeout expires.

### 2.7. The check the proxy makes

Every credential gate in the proxy asks the authority, and this subsection gives that exchange.
The gates are a route's `require`, and a source node's `GET` and `POST /genid` through `read_require` and `write_require`.
The authority decodes every credential.
The authority is the backend named by `[auth] backend`, which must name a `[[backend]]` with `sockets`.
A gate with no `[auth]`, an `[auth]` with no `backend`, or an `[auth] backend` that names a `url` backend stops the start with exit `2` (config validation in `m6-http/src/config.rs`).

The proxy takes the credential from `Authorization: Bearer` or the `session` cookie (`extract_token` in `m6-http/src/auth.rs`).
A request that carries neither is refused by the proxy, so the authority never sees it.
A credential is written into one request with no body (`encode_check_request` in `m6-http/src/auth.rs`):

```text
POST /check HTTP/1.1
Host: localhost
Authorization: Bearer <credential>
Content-Length: 0
Connection: close
```

A credential carrying CR, LF or NUL, or longer than one header line, is refused before this request is written (`credential_is_safe`).
The check has a 50 ms deadline, `CHECK_DEADLINE` in `m6-http/src/auth.rs`, so a slow authority delays a gated request by at most 50 ms.

The answer has three outcomes (`parse_check` in `m6-core/src/auth.rs`):

- A `200` whose JSON body carries all four identity fields is an identity.
- A `401` is the authority's refusal.
- Every other status, a `200` that does not parse, a timeout and a connect failure are no decision.

Each gate applies its own `require` string to the identity (`Answer::decide` in `m6-core/src/auth.rs`).
An identity without the named role or group is `403`.
A refusal is the `401` or `403` from `token_refusal` in `m6-core/src/http.rs`.
The `401` carries `WWW-Authenticate: Bearer`.
On a route, an allowed request is forwarded with `X-Auth-Claims` set (§2.3).
A browser with no credential, or one the authority refused, is redirected to `/login`, or to `/auth/refresh` when it still holds a refresh cookie.
No decision is `503` with `Retry-After: 1`, because a `401` would tell a user with a valid credential to log in again.

The `m6-auth-server` reads the identity from its database on every check, so a role change or a revocation counts from the next request (`handle_check` in `m6-auth-server/src/handlers.rs`).
Its `200` carries `Cache-Control: no-store`.
A missing, invalid or expired token is `401` `{"error":"unauthorised"}`.
A locked database is `503` `{"error":"unavailable"}` at once, because the proxy's 50 ms deadline would pass before the service's one-second deferral.

The proxy probes every `[auth] backend` member once at start with one credential-less `POST /check` under a 2 s deadline (`probe_auth_backend` in `m6-http/src/main.rs`).
A member that answers `401` or `503` serves checks.
A `404` from any member stops the start with exit `2` and names the socket.
A member that answers another status is ignored, and the start stops only when no member answered `401` or `503` and at least one answered another status.
An unreachable member warns and the node starts.

Those are the fields a backend parses and the check that runs before `X-Auth-Claims` appears.
A backend MAY trust the proxy-owned values in §2.2 and §2.3 only while §2.5 holds, with `Host` excepted.

### 2.8. Reserved path: `/genid`

`POST /genid` is reserved on every backend that serves a site.
The backend answers it before routing: it reads the config, `site.toml`, templates and params again, builds a new site beside the one it is serving, and swaps it in with one assignment.
A load that succeeds answers `204` with `Cache-Control: no-store`.
A load that fails assigns nothing, the old site keeps serving, and the answer is `500` with `Cache-Control: no-store` and a body naming the cause.

A method other than POST on `/genid` is `405` with `Allow: POST`, and a route whose pattern is exactly `/genid` is refused at startup (`m6-core/src/site.rs`).
The socket's directory permissions are the only authentication for this request (§6.1).

Section 3 gives the response a backend writes.

## 3. The response

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

A backend SHOULD send `Content-Length`.

The proxy refuses a response that:

- carries both `Transfer-Encoding` and `Content-Length`, which RFC 9112 §6.1 forbids.
- carries two `Content-Length` fields with different values.
- carries a `Content-Length` that is not a valid non-negative integer.
- carries a `Transfer-Encoding` whose final coding is not `chunked`, which leaves the response unframed.
- has a header section larger than 8192 bytes.
- declares or delivers a body larger than 128 MiB.

A refused response reaches the client as a backend error (§5).

`Transfer-Encoding` is parsed as a comma-separated list, so `gzip, chunked` is chunked.
A chunked body MUST terminate every chunk with CRLF and end with a terminated trailer section.

### 3.3. Responses that must not have a body

Per RFC 9110, the backend MUST NOT send a body when the request method was `HEAD`, or when the status is `1xx`, `204` or `304`.

For `HEAD`, the backend SHOULD send the `Content-Length` the equivalent `GET` would have produced.
The proxy knows the request method and does not wait for body bytes.

The proxy stops reading at the header section for these responses and closes the connection, so a body sent anyway is discarded.

### 3.4. Content type

The backend SHOULD send `Content-Type` on every response with a body, and MUST include a `charset` parameter on textual types.

A browser reads a textual type without a charset as Latin-1, so correct UTF-8 bytes render as mojibake.

### 3.5. Caching

The backend MAY send `Cache-Control`, and the proxy applies RFC 9111 semantics to it.

The proxy stores a response when the backend marks it `Cache-Control: public` and the status is storable.
A backend that sends nothing is treated as uncacheable, so a backend serving public content SHOULD send `Cache-Control: public`.

### 3.6. Compression

Compression is the backend's job.
The proxy is a cache: `m6-http` negotiates between and caches the representations a backend produced, and `m6-http` has no response compressor.

A backend MAY compress.
If it does, it MUST send an accurate `Content-Encoding` and MUST honour `Accept-Encoding`, including `q=0` meaning not acceptable.

A backend that does not compress serves one representation.
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

The proxy uses `compresses` to decide whether to advertise an encoding dimension on a cache miss.
When the backend sent no `Vary` and does not compress, a cache miss adds none.
When the backend sent a `Vary`, a cache miss keeps its field names and adds `Accept-Encoding` to them, even when the backend does not compress.
A cache hit, including a `304` revalidation, adds `Accept-Encoding` to the served `Vary` whatever `compresses` says, because the cache key carries the content coding.
A `Vary: *` is left unchanged on both paths.

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

Those are the rules a response satisfies.
Section 4 gives the status codes the proxy treats specially.

## 4. Status codes

A backend chooses its own status codes.
The proxy passes a status below `400` through, and it handles a status of `400` or above according to `[errors] mode`.
This section gives that split and the rule for a path a backend does not serve.

Table 4 gives the treatment by range.
Take from it that the status is always the backend's, and that the proxy replaces the body of an error it did not store in the cache.

| Range | Proxy behaviour |
|---|---|
| `5xx` | Counted as a backend error, and the body is replaced according to `[errors] mode` |
| `4xx` | The status reaches the client and the body is replaced according to `[errors] mode` |
| below `400` | Passed through |

**Table 4: status ranges and how the proxy handles each.**
`m6-site-toml.md` gives the `[errors]` values.
A cacheable error response, such as a `404` marked `Cache-Control: public`, is stored before the error mode runs, so a later cache hit can serve the backend's own error body.

A backend SHOULD return `404` for a path it does not serve, so the status is honest to caches and crawlers.
A `200` with an error page hides a missing resource from both.

Those are the statuses the proxy handles for the backend.
Section 5 gives the conditions under which no response arrives.

## 5. Failure handling

The proxy turns every backend failure into a status the client can act on.
This section gives the proxy's response to each failure.

Table 5 gives the conditions.
Take from it that a failure the proxy cannot route to another member reaches the client as a `502`, except a pool whose every member has a full listen queue, which answers `503`.

| Condition | Proxy behaviour |
|---|---|
| Socket does not exist | The connect fails, and the next member serves the request. If every member fails, the client receives `502` |
| `connect` refused | The next member serves the request. If every member fails, the client receives `502` |
| No response within `backend_timeout_secs` (default **30**) | The connection is dropped and the client receives `502` |
| Malformed status line or headers | `502` |
| Framing violation (§3.2) | `502` |
| Response exceeds 128 MiB | `502` |
| Connection closed before a complete response | `502` |
| Every member's listen queue is full | `503` with `Retry-After: 1` |

**Table 5: backend conditions and the response the proxy sends.**
A slow backend and a refused connection produce the same `502`.

The proxy retries the connect against the next member.
A request a member accepted is not retried, so a reset in the middle of a response answers `502`.
A backend SHOULD fail fast, because a prompt `500` reaches the client sooner than a connection held for 30 seconds.

Those are the failures a backend can survive and the ones that end the request.
Section 6 gives the security rules that keep the socket trustworthy.

## 6. Security requirements

Two boundaries keep the protocol safe: the socket is reachable only by the proxy, and every request value is attacker-controlled unless the proxy owns it (§2.5).
This section gives both boundaries and the header-injection rule that follows.

### 6.1. Trust boundary

The backend socket MUST be reachable only by the proxy.
The permissions on the socket's directory enforce this, and §2.5 depends on them.

### 6.2. Untrusted input

Every field in a request is attacker-controlled except the proxy-owned fields in §2.2 and §2.3.
`Host` is client-supplied and untrusted.

A backend MUST:

- reject or sanitise path traversal (`..`, encoded variants, absolute paths) before using any request-derived value in a filesystem path.
- treat the request target as bytes until it has validated them, because the proxy passes the target through in the encoded form the client sent.
- bound anything derived from the request that it allocates.

### 6.3. Header injection

A backend MUST NOT copy a request-derived value into a response field without validating it.
A CR or LF in a field value terminates the header section early and injects an attacker-controlled response.

The proxy performs the equivalent check in the other direction and refuses a request whose fields would inject HTTP/1.1 framing.

Section 7 gives the concurrency the socket supports.

## 7. Concurrency

This section gives the concurrency model a member needs.

A backend MUST be able to accept a new connection while it is handling another.
It MAY use threads, processes or an event loop.

The reference model, given in `m6-architecture.md` §7.1, is one process with one event loop, scaling by running more instances.
A backend at its connection ceiling stops accepting, and clients wait in the kernel's listen backlog.
The proxy's own deadline answers a request that waits there too long.

That is the concurrency a member provides.
Section 8 gives the lifecycle: startup, shutdown and exit.

## 8. Lifecycle

A member's lifecycle is three events: startup, shutdown and exit.
This section gives all three.

### 8.1. Startup

Bind before announcing readiness.
The proxy expands the glob once at start and sends requests to every socket it finds, so a backend that creates the socket before it can serve receives requests it cannot answer.
A backend MUST bind before the proxy starts, because a glob that matches no socket stops the proxy.

### 8.2. Shutdown

On `SIGTERM` or `SIGINT`, the backend MUST take these steps in order:

- **Stop accepting new connections.**
- **Finish requests already in flight.**
- **Remove the socket file.**
- **Exit with status `0`.**

A second `SIGTERM` or `SIGINT` MUST exit immediately.
The pool is fixed at start, so a removed socket file leaves its member in the pool until the proxy restarts.
Each request that lands on a member whose connect fails goes to the next member.

### 8.3. Exit codes

Table 6 gives the three codes.
Take from it that code `2` tells a supervisor the process never bound.

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error |
| `2` | Configuration or usage error, detected before binding |

**Table 6: the three exit codes.**
A supervisor tells a misconfiguration from a crash by that code.

Those are the three events in a member's lifecycle.
Section 9 collects every MUST into one checklist.

## 9. Minimum viable backend

A backend that meets every item here satisfies every MUST in this version of the contract.
This section collects every MUST a backend implements, with the section that states it.

Everything a backend MUST do:

- [ ] Remove the stale socket and bind a Unix `SOCK_STREAM` socket matching the glob (§1.2).
- [ ] Make the socket reachable by the proxy's user, with `0660` and a trusted group or `0666` behind a trusted directory (§1.2).
- [ ] Listen with a backlog of at least 64 (§1.2).
- [ ] Keep the socket reachable only by the proxy (§6.1).
- [ ] Accept a new connection while it is handling another, and serve requests (§1.3, §7).
- [ ] Parse an HTTP/1.1 request line and header section (§2.1).
- [ ] Treat the request target as bytes until validated, then decode it and treat the result as untrusted (§2.1, §6.2).
- [ ] Read exactly `Content-Length` body bytes, if any (§2.6).
- [ ] Emit an `HTTP/1.1` status line (§3.1).
- [ ] Frame the response with `Content-Length`, with a correctly chunked body and a terminated trailer section, or by closing the connection (§3.2).
- [ ] Emit `Content-Type` with a charset on text (§3.4).
- [ ] Send no body for `HEAD`, `1xx`, `204` or `304` (§3.3).
- [ ] Close the connection after responding to a request with `Connection: close`, handle the proxy closing it after each response, and do not rely on pipelining (§1.3).
- [ ] Declare in `site.toml` whether the service compresses, and refuse to start with exit `2` when the declaration disagrees with what it does (§3.6.1).
- [ ] If it compresses, send an accurate `Content-Encoding` and honour `Accept-Encoding` (§3.6).
- [ ] Trust the proxy-owned fields in §2.2 and §2.3 only while the socket is reachable by the proxy alone (§2.5).
- [ ] If `[auth] backend` names it, answer `POST /check` on the check socket with a `200` identity, a `401` refusal or a `503` when it cannot decide, and answer the house `404` for every other path (§2.7).
- [ ] Treat `Host` as attacker-controlled (§2.5, §6.2).
- [ ] Reject or sanitise path traversal before using a request-derived value in a filesystem path (§6.2).
- [ ] Bound anything derived from the request that it allocates (§6.2).
- [ ] Refuse to copy a request-derived value into a response field without validating it (§6.3).
- [ ] On `SIGTERM` or `SIGINT`, stop accepting, drain, unlink the socket and exit `0` (§8.2).
- [ ] Exit immediately on a second `SIGTERM` or `SIGINT` (§8.2).

The proxy terminates TLS, HTTP/2 and HTTP/3, caches responses and asks the authority for every credential gate (§2.7), so a backend implements none of them.
Compression is the backend's job (§3.6).

`m6-backend-examples.md` gives six reference implementations, all written against this document.

---

## Summary

A backend is a process that binds a Unix socket the proxy can reach, parses an HTTP/1.1 request and answers with a framed HTTP/1.1 response.
The proxy owns TLS, HTTP/2, HTTP/3, caching and every credential gate, so a backend trusts the proxy-owned request fields only while the socket is reachable by the proxy alone and treats everything else in a request as attacker-controlled.
A backend that meets the checklist in section 9 satisfies every MUST in this version of the contract.
