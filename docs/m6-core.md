# m6-core — Design

**Status: design. Describes the target, not the current crate.** `m6-core`
today is an unplanned collection of whatever was convenient to share; see
§9 for the gap. This document defines what it is for, so the gap can be closed
deliberately.

---

## 1. What it is

A **component library for building m6 applications quickly and robustly.**

Every m6 process does the same handful of things: receive an HTTP request,
process headers, run a handler, produce a correctly framed response, compress
it, label it, log it, take configuration, notice when configuration changes,
and shut down cleanly. `m6-core` provides all of that as components. An
application supplies only what is specific to it.

It is also the **home of every RFC obligation in the platform.** HTTP/1.1,
HTTP/2 and HTTP/3 live here, next to each other, sharing the version
independent semantics they have in common. Conformance is a property of this
crate.

The test of success is that a new m6 application is small. `m6-html` is six
lines today and serves every HTML page on mgrosvenor.com. That is the shape
everything else should approach.

## 2. What it is not

`m6-core` is **not** the way applications must be written. `m6-overview.md`
promises that renderers can be written in any language, and the mechanism that
delivers that promise is the **wire contract**: HTTP/1.1 over a Unix socket.
See `m6-backend-examples.md`.

That ordering is deliberate and load bearing:

- The **wire contract is primary**. It is what a backend must satisfy. It is
  small enough to implement from scratch in any language in well under a
  hundred lines.
- `m6-core` is a **convenience for Rust**. It makes the common case fast to
  build and hard to get wrong. It must never become the only viable path, or
  the multi-language promise dies quietly while still being written down.

Anything that would make a non-Rust backend a second class citizen belongs in
`m6-http`, not here.

## 3. Position

```
                        ┌──────────────────────────────┐
   public traffic ────► │  m6-http                     │
   TLS, h1/h2/h3        │  listener, cache, routing,   │
                        │  auth, rate limit, policy    │
                        └──────────────┬───────────────┘
                                       │  HTTP/1.1 over Unix socket
                                       │  (the wire contract)
                        ┌──────────────▼───────────────┐
                        │  application                 │
                        │  m6-html, m6-file, renderers │
                        │  any language                │
                        └──────────────────────────────┘

   m6-core is linked by m6-http and by Rust applications.
   It is not a process and does not appear in the request path by itself.
```

`m6-core` sits beside both, not between them. `m6-http` links it for the
protocol implementations and the semantics. A Rust application links it for
the service scaffolding. The two use different features (§5).

`m6-render` layers on top of `m6-core` for template driven applications and is
documented separately.

## 4. What belongs in m6-core

The rule, in one line: **if an external specification defines the correct
answer, or every m6 service needs it, it belongs here.**

### 4.1 Protocol

All three HTTP versions, together, because they share more than they differ.

- **Semantics (RFC 9110)**, version independent: method and status meaning,
  header field validation, conditional request preconditions and comparison,
  content negotiation, `Via`, hop by hop field handling, trusted header rules.
- **Caching semantics (RFC 9111)**: storability, freshness, age calculation,
  request and response directives, revalidation rules. The *rules*, not the
  storage.
- **HTTP/1.1 (RFC 9112)**: request and response parse and serialise, chunked
  transfer coding, framing validation.
- **HTTP/2 (RFC 9113)**: frame layer, stream state machine, HPACK, flow
  control, connection and stream error taxonomy.
- **HTTP/3 (RFC 9114)**: request and response mapping, QPACK integration.

**Why together.** `validate_request_header_bytes` is already shared by the h2
and h3 paths, because RFC 9114 §4.3 is RFC 9113 §8.3 restated. Today it lives
inside `http2.rs` for want of a home. Before it was shared, HTTP/3 had no
request validation at all: an uppercase field name or a duplicate `:method`
was served a normal 200 over h3 while the identical request was correctly
rejected over h2. Keeping the implementations adjacent is what prevents that
class of divergence, and it has already been paid for once.

### 4.2 Service scaffolding

What every m6 process needs to be a service.

- Unix socket server: stale socket removal, bind, permissions, accept loop,
  socket removal on shutdown.
- Concurrency: fixed thread pool, bounded queue, 503 on queue full as
  backpressure to `m6-http`, per `m6-decisions.md`.
- Signals: SIGTERM and SIGINT identical, first is a clean drain, second exits
  immediately. One implementation.
- Exit codes: 0 clean, 1 runtime error, 2 config or usage error before
  binding.

### 4.3 Configuration

- Startup configuration parsing and validation.
- Per application config files.
- `site.toml` observation and reload, with the reload semantics stated once so
  every service behaves the same way when it changes.

### 4.4 Content

- MIME type resolution including charset.
- Compression: gzip and brotli, with negotiation driven by §4.1.
- Minification.

### 4.5 Safety

- Path parameter validation and traversal refusal.
- Filesystem path resolution confined to a root.
- Request and response size limits.

These are grouped separately because they are security boundaries, and a
security boundary with more than one implementation is a security boundary
with more than one behaviour. Path validation currently has three.

### 4.6 Observability

- Structured logging, including the analytics event shape.
- The counter and percentile primitives services report through.

### 4.7 Test kit

Standing a service up, claiming a port or socket without a
time of check to time of use race, driving it, tearing it down, and raw socket
clients for framing level tests.

This is not a convenience. Core owns conformance, so core owns the harness
that proves conformance. It is also the fix for a live problem: the scaffolding
to start a service is currently reimplemented across many test files, and that
duplication is the prime suspect for an intermittent full suite failure.

## 5. Dependency weight, and features

**Constraint: `m6-core` must remain linkable by a command line tool.** `m6-md`
is a build time Markdown converter. It must not acquire a QUIC stack, a
BoringSSL toolchain requirement, or a TLS library because it depends on core.

Feature gates, not separate crates:

| Feature | Adds | Consumers |
|---|---|---|
| *default* | semantics, HTTP/1.1, service scaffolding, config, content, safety, logging | applications, `m6-md`, `m6-render` |
| `http2` | HTTP/2 frame layer, HPACK | `m6-http` |
| `http3` | HTTP/3 mapping, QPACK, QUIC integration | `m6-http` |
| `testkit` | harness, raw socket clients | dev dependency everywhere |

This works because the heavy dependencies are already confined by protocol
rather than smeared across the codebase, and because `m6-render` already uses
feature gates for its optional extras. Applications take the default and get
nothing heavier than they need.

Separate crates were considered and rejected: the protocol versions share the
semantics layer, and splitting them across crates would either duplicate that
layer or require a fourth crate to hold it.

## 6. What does not belong

Things that look like core and are not. Each of these is a **deployment
decision**, not a specification.

| Stays in `m6-http` | Why |
|---|---|
| TLS listener, certificate handling | A deployment decision, and applications never terminate TLS |
| The epoll event loop | `m6-http`'s concurrency model. Applications use the thread pool (§4.2) |
| Cache storage and eviction | The RFC defines freshness, not how many megabytes to keep |
| Route table and matching policy | Which paths exist is site configuration |
| Rate limiting | A policy choice about who to refuse |
| Proxy and backend pool logic | Specific to being the front door |

The pattern: `m6-core` answers "what does the specification require", `m6-http`
answers "what does this deployment do".

## 7. Guidance, not law

Real exceptions exist and using this document to justify a bad refactor is
worse than ignoring it.

- **A good external crate is the same win.** The goal is one implementation,
  not one location. `httpdate` is shared already and does not need reimplementing
  in core.
- **The hot path may justify specialisation.** A generic core API that costs an
  allocation on a 3 microsecond cache hit is a bad trade. Measure, then keep the
  specialised version and record why.
- **Policy that merely looks generic is not specification.** Which MIME types
  get compressed, what `Cache-Control` values a site sends, what the route
  table contains.
- **A wrong abstraction in core costs more than a duplicate**, because
  applications then build on it. When the second caller is hypothetical and the
  interface is not obvious, waiting is reasonable.

## 8. Consequences

- **Conformance becomes a property of `m6-core`.** h2spec, h3spec and the
  HTTP/1.1 corpus run against this crate. `m6-http` inherits the result rather
  than owning it.
- **The public API becomes a compatibility surface at 1.0**, in the same way
  `site.toml` keys do. Regrettable names should be fixed before that, not
  after.
- **`m6-http` gets smaller.** It becomes a listener, a cache, a router and a
  set of policies over a correct protocol library.
- **Rust applications get shorter.** The scaffolding every service reimplements
  moves behind one dependency.

## 9. Current state, and how far away it is

`m6-core` was never designed. It appears once in the entire documentation set,
in `m6-render-lib.md`, as a conditional aside: "which may be extracted into a
lower level `m6-core` crate if warranted". It was warranted, it happened, and
nothing said what it was for. `m6-decisions.md` records twenty seven decisions
down to exit codes and JSON merging; the crate boundary is not among them,
because `m6-core` is not one of the five processes the architecture names.

Measured against this document:

- **`m6-http` uses one module from `m6-core`: logging.** Everything in §4.1
  currently lives in `m6-http`.
- **Two modules have no consumers at all**, roughly a sixth of the crate.
- **Path validation exists three times** and the copy in core is shadowed by a
  function of the same name in `m6-render`.
- **Signal handling exists four times**, though the behaviour is specified once
  in `m6-decisions.md` for all tools.
- **Route matching exists three times**, twice hand rolled, with different
  specificity rules, so the same route table can resolve differently in two
  services.
- The test kit does not exist.

`CRATE-MATRIX.md` and `CORE-DUPLICATION-AUDIT.md` in the site repository carry
the detail.

## 10. Sequence

Ordered so each step ships on its own and makes the next cheaper. This is a
re-verification exercise against a live system, not a refactor: the conformance
suites and the fleet are the gate at every step.

1. **The small consolidations.** Path validation, signals, header lookup,
   token generation. Mechanical, and path validation is a security boundary
   with three behaviours today.
2. **The test kit.** Everything after this needs it, and moving protocol code
   without a shared harness means writing the tests twice.
3. **Semantics (RFC 9110 and 9111 rules).** Version independent, so it moves
   without touching any wire format. Conditional requests first: the rules
   exist twice today and one copy is wrong.
4. **HTTP/1.1.** The smallest wire format, and the one applications use.
5. **HTTP/2, then HTTP/3.** Largest and last. One protocol per release, with
   h2spec and h3spec green at every step before proceeding.

Step 1 can begin immediately. Nothing before step 3 changes what goes on the
wire.
