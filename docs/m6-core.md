# m6-core — Design

**Status: design. Describes the target, and the target was reached.** This
document defines what the crate is for and where its boundary is. It was
written before the migration, when m6-core was an unplanned collection of
whatever was convenient to share.

Phases 0 to 6 are done, m6-render is deleted, and m6-core is now the only crate
a service links. **§9 below is therefore history, not current state**: it
describes the gap as it stood before the migration and is kept as the record of
what was closed. For the crate as it is, and for every component and its
interface, see **`m6-core-reference.md`**.

---

## 1. What it is

A **component library for building m6 applications quickly and robustly.**

Every m6 process does the same handful of things: receive an HTTP request,
process headers, run a handler, produce a correctly framed response, compress
it, label it, log it, take configuration, notice when configuration changes,
and shut down cleanly. `m6-core` provides all of that as components. An
application supplies only what is specific to it.

It is also the home of **HTTP semantics and HTTP/1.1**: the rules that do not
depend on protocol version, plus the one wire format that both the proxy and
every backend speak. HTTP/2 and HTTP/3 stay in `m6-http`, which is the only
process that terminates them; §4.1 gives the reasoning and the measurement
behind it.

The test of success is that a new m6 application is small. `m6-html` is six
lines today and serves every HTML page on mgrosvenor.com. That is the shape
everything else should approach.

## 2. What it is not

`m6-core` is **not** the way applications must be written. `m6-overview.md`
promises that renderers can be written in any language, and the mechanism that
delivers that promise is the **wire contract**: HTTP/1.1 over a Unix socket.
See `m6-backend-examples.md`.

That ordering is deliberate and load bearing:

- The **wire contract is primary**. `m6-backend-protocol.md` is the platform's
  interface specification, and it is language agnostic by construction. It is
  small enough to implement from scratch in any language in well under a
  hundred lines.
- **`m6-core` MUST NOT be the only readable definition of any part of it.**
  If behaviour a backend depends on exists only as Rust, the specification has
  a hole. Fix the specification.
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

   m6-core is linked by m6-http, by the default apps, and OPTIONALLY by
   consumer apps. It is not a process and does not appear in the request
   path by itself. There is no framework layer: m6-render is dissolved.
```

`m6-core` sits beside both, not between them. `m6-http` links it for the
protocol implementations and the semantics. A Rust application links it for
the service scaffolding. The two use different features (§5).

**`m6-render` is dissolved** (decided 2026-09-10). Its service scaffolding
moves into `m6-core` and its templating into `m6-html`; the crate is deleted.
`m6-html` becomes the default templating app, in the same way `m6-file`
provides static files and `m6-auth-server` provides auth. A consumer app links
`m6-core`, or nothing at all, and never a default app.

The measurement behind it: `m6-render` was **14% templating and 86% service
scaffolding**, and three of its four consumers had **zero template files**
while linking Tera, comrak, pest, chrono-tz and fourteen other crates in order
to obtain `App`, `Request` and `Response`. The server loop was trapped inside a
crate named after the one feature most of its users did not want.

Duplicate dependencies are acceptable where coupling is not: an app that wants
Tera declares Tera. It does not acquire a template engine as a side effect of
wanting a server loop, which is the situation today, where three of the four
`m6-render` consumers have zero template files and link it for `App`,
`Request` and `Response`.

## 4. What belongs in m6-core

The rule, in one line: **if an external specification defines the correct
answer, or every m6 service needs it, it belongs here.**

### 4.1 Protocol

**Version-independent semantics, and HTTP/1.1. Not HTTP/2 or HTTP/3.**

In core:

- **Semantics (RFC 9110)**: method and status meaning, header field
  validation, conditional request preconditions and comparison, content
  negotiation, `Via`, hop-by-hop field handling, trusted header rules.
- **Caching semantics (RFC 9111)**: storability, freshness, age calculation,
  request and response directives, revalidation. The *rules*, not the storage.
- **HTTP/1.1 (RFC 9112)**: request and response parse and serialise, chunked
  transfer coding, framing validation.

Stays in `m6-http`:

- **HTTP/2 (RFC 9113)**: frame layer, stream state machine, HPACK, flow
  control, error taxonomy.
- **HTTP/3 (RFC 9114)**: request and response mapping, QPACK, QUIC
  integration.

#### Why the line is drawn there

The test is **how many consumers a thing has**, and the answer differs sharply.

**Semantics have many.** The h1 path, the h2 path, the h3 path and every Rust
backend all need the same answers about what a method means, whether a
representation is fresh, and which header fields are legal. One implementation
or they drift.

**HTTP/1.1 has many.** It is the backend wire contract
(`m6-backend-protocol.md`), so `m6-http` needs it to talk to backends and every
Rust backend needs it to answer. `m6-core::parse` already exists and
`m6-auth-server` already uses it.

**HTTP/2 and HTTP/3 have exactly one, permanently.** `m6-http` is the only
process that terminates a public connection; that is the architecture, not a
current limitation. A library with one consumer is not a library, it is that
consumer's code in another directory.

#### The sharing argument, tested and withdrawn

An earlier version of this document argued that all three versions belonged
together because h2 and h3 share code. Measured, the H3 path imports **exactly
one symbol** from `http2.rs`:

```
use m6_http_lib::http2::validate_request_header_bytes;
```

Nothing else. No frame layer, no HPACK, no stream state machine, no
concurrency cap. HPACK and QPACK are different algorithms; h2 frames and QUIC
streams are different transports. **The only thing h2 and h3 share is RFC 9110
semantics**, which is in core under this design, so they still share it,
without either wire format moving.

That also disposes of the bug the earlier argument leaned on. HTTP/3 once had
no request validation at all and served 200 to requests HTTP/2 correctly
rejected. The fix is that the *rule* has one home. It does not require the
frame layers to be neighbours.

#### What this costs

`m6-http` keeps the largest and most defect-dense body of code in the project,
and conformance to RFC 9113 and 9114 remains its property rather than core's.
That is accepted deliberately: the alternative is a feature matrix and a
4,300-line migration against a live system, in exchange for an abstraction
boundary that exactly one caller would ever cross.

If a second consumer ever appears, a backend that needs to terminate h2 itself,
this decision should be revisited rather than worked around.

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

## 5. Dependency weight

**Constraint: `m6-core` must remain linkable by a command line tool.** `m6-md`
is a build-time Markdown converter. It must not acquire a QUIC stack, a
BoringSSL toolchain requirement, or a TLS library because it depends on core.

With HTTP/2 and HTTP/3 staying in `m6-http` (§4.1), this is satisfied by
construction rather than by configuration. Nothing in core's scope needs
`quiche`, `rustls`, `hpack` or `ring`. HTTP/1.1 parsing and serialisation, the
semantics layer, the socket server, config, logging and the content modules are
all dependency-light.

**One feature gate:**

| Feature | Adds | Consumers |
|---|---|---|
| *default* | semantics, HTTP/1.1, service scaffolding, config, content, safety, logging | applications, `m6-md`, `m6-render`, `m6-http` |
| `testkit` | harness, raw-socket clients | dev-dependency everywhere |

An earlier version of this document proposed `http2` and `http3` features to
keep the weight off `m6-md`. Those gates existed only to contain a problem that
this design does not create, and a build matrix maintained for one consumer's
benefit is a cost with no matching return.

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
| HTTP/2 and HTTP/3 | Exactly one consumer, permanently. See §4.1 |

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

- **Conformance splits along the same line.** The HTTP/1.1 corpus and the
  semantics tests run against `m6-core`. h2spec and h3spec continue to run
  against `m6-http`, which is where those protocols live. Neither suite changes
  owner by accident.
- **The public API becomes a compatibility surface at 1.0**, in the same way
  `site.toml` keys do. Regrettable names should be fixed before that, not
  after.
- **`m6-http` gets smaller, but stays the largest crate.** It sheds the
  semantics layer, HTTP/1.1 and the service scaffolding, and keeps the h2 and
  h3 implementations, the listener, the cache and the routing and rate-limiting
  policy. That is the intended shape, not a failure to finish.
- **Rust applications get shorter.** The scaffolding every service reimplements
  moves behind one dependency.

## 9. Current state, and how far away it is

> **Historical, as of before the migration.** Every item in this section has
> been closed. Kept as the record of what the migration was for. Current state
> is `m6-core-reference.md`.

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

The step-by-step migration, with gates, risks and rollback per phase, is in
**`m6-core-implementation-plan.md`**. Summary below.

Ordered so each step ships on its own and makes the next cheaper. This is a
re-verification exercise against a live system, not a refactor: the conformance
suites and the fleet are the gate at every step.

1. **The small consolidations.** Path validation, signals, header lookup, token
   generation. Mechanical, and path validation is a security boundary with
   three behaviours today.
2. **The test kit.** Everything after this needs it, and moving code without a
   shared harness means writing the tests twice.
3. **Semantics (RFC 9110 and 9111 rules).** Version-independent, so it moves
   without touching any wire format. Conditional requests first: the rules
   exist twice today and one copy is wrong. `validate_request_header_bytes`
   moves here, which is what lets the h2 and h3 paths keep sharing it from
   `m6-http`.
4. **HTTP/1.1.** The last protocol move, and the only one. It is the backend
   wire contract, so it is the piece that makes a Rust backend short.

There is no step 5. HTTP/2 and HTTP/3 stay where they are.

Nothing before step 3 changes what goes on the wire, and nothing in the
sequence touches the h2 or h3 implementations at all, which is the main
practical argument for the boundary in §4.1: the highest-risk code in the
project is not involved.
