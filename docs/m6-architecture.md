# m6, design and architecture

This document answers why.
Why m6 exists, what problem it solves, how it solves it, and the reason behind each design decision that follows.

Four other documents answer what, and this one does not repeat them.
`README.md` says what m6 is and how to run one, `m6-core-reference.md` names every component and interface, `m6-site-toml.md` gives every configuration key, and `m6-backend-protocol.md` specifies the wire contract.
`PERFORMANCE.md`, `BENCHMARKS.md` and `POSITIONING.md` carry the measurements, including the comparison against other servers.

A decision recorded without its reason is a rule nobody can safely change.
Someone reading only the shape of m6 will reasonably conclude that a branch could be added here, a lock moved there, a file introduced to cover an error case, and each of those undoes something decided for a reason.
So every section asks a question and answers it, and mechanism appears only as far as a reason needs it.

We begin with the problem m6 exists to solve and what solving it that way costs (§1), then the shape that follows (§2).
§3 to §8 take each part of that shape and give the reasoning: the process family, the edge, authentication, the library, the service shape, and configuration.
§9 holds the remaining decisions with a reason each, §10 states what is deliberately absent, and §11 summarises.

Where this document and the code disagree, the code is right and this document is a defect.

## Contents

1. [The problem m6 solves](#1-the-problem-m6-solves)
2. [The shape that follows](#2-the-shape-that-follows)
3. [Why a family of processes](#3-why-a-family-of-processes)
4. [Why one process faces the internet](#4-why-one-process-faces-the-internet)
5. [Why authentication sits where it does](#5-why-authentication-sits-where-it-does)
6. [Why m6-core exists, and where its boundary falls](#6-why-m6-core-exists-and-where-its-boundary-falls)
7. [Why every service has one shape](#7-why-every-service-has-one-shape)
8. [Why configuration is split the way it is](#8-why-configuration-is-split-the-way-it-is)
9. [The remaining decisions, and the reason for each](#9-the-remaining-decisions-and-the-reason-for-each)
10. [What is deliberately absent](#10-what-is-deliberately-absent)
11. [Summary](#11-summary)

---

## 1. The problem m6 solves

**Serving a fast website conventionally means assembling a stack, and the assembly is most of the cost.**
A proxy terminates *TLS* (*Transport Layer Security*), a separate tier caches responses, an application server renders pages, another process authenticates, and a build tool produces content.
Each has its own configuration, its own failure modes and its own operational surface, and a request crosses a process boundary several times before anyone gets an answer.

This section states why that cost is structural, what m6 does instead, and what doing it that way gives up.

### 1.1 Why the conventional cost is structural

**A multi-process or multi-threaded server cannot keep its cache in its own heap, and everything follows from that.**
Each worker has its own address space, so a shared cache has to live somewhere both can reach: shared memory, a separate daemon such as Redis or memcached, or disk.
Reaching it costs serialisation, a lock, and at minimum one inter-process round trip on every cache lookup.

That is not a tuning problem.
It is a consequence of the concurrency model, so no amount of configuration removes it, and it is paid on the most common request a website serves, which is a hit on something already rendered.

### 1.2 What m6 does instead

**One process owns the whole hot path on one thread, so a cache hit never leaves the address space.**
Receiving, TLS decryption, route lookup, cache lookup, TLS encryption and sending all happen in one process on one thread, and the cache is a reference-counted structure in the same heap.

A hit therefore costs a hash lookup and a reference count increment.
The encrypted record is written from the same allocation the response was stored in on the first request, so there is no copy, no lock, and no inter-process round trip.
`BENCHMARKS.md` holds what that measures at and `POSITIONING.md` places it against other servers.

**The rest of the stack collapses the same way.**
Rendering, static files, authentication and content conversion are small single-job processes behind one wire contract, and one `site.toml` configures the set.
A site is therefore one platform with one description, and not an integration of five products each of which has to be separately understood.

### 1.3 What this gives up

Stating the cost is part of the design, because a reader choosing m6 needs to know when not to.
Table 1 gives what the single-threaded model surrenders.

| given up | why it follows from the design |
|---|---|
| multi-core use within one process | one thread cannot occupy several cores. m6 scales by running more instances, or more backend workers, which is why §3.4 makes pool membership automatic |
| HTTP/1.1 throughput against a mature server | a new connection per request pays a TLS handshake each time, and the h1 path makes a blocking backend call on a miss, which stalls the loop |
| zero-copy file serving | content is copied through userspace buffers, so a server using `sendfile()` pulls ahead as responses grow past a few kilobytes |
| kernel TLS | pushing encryption into the kernel is a Linux feature not yet in the TLS stack m6 uses, and it is the whole of the remaining gap to the fastest single-core server measured |

**Table 1: what m6's single-threaded design gives up, and why each follows from it.**
Take from it that every entry is a consequence of one thread owning the hot path, and that none is a defect to be fixed without changing that premise.

### 1.4 When m6 is the wrong choice

**A site dominated by HTTP/1.1 traffic or by large file responses is better served by a mature conventional server.**
So is a deployment that must saturate many cores from one process.
m6 is for a site where the common request is a cache hit over HTTP/2 or HTTP/3, where sub-millisecond response matters, and where one description of the whole stack is worth more than the last increment of raw throughput.

Everything in the rest of this document follows from that choice, starting with the shape it produces.

---

## 2. The shape that follows

**m6 is six serving binaries, each with one job, wired by `site.toml` over Unix sockets, with one of them on the public port.**
This section is the shape in brief, so the reasoning from §3 onwards has something to refer to.

| binary | one job |
|---|---|
| `m6-http` | terminate TLS, rate limit, cache, route, enforce route authentication, proxy to backends |
| `m6-http` in redirect mode | answer `:80` with a redirect to HTTPS, as its own process |
| `m6-html` | render HTML from templates and JSON data |
| `m6-file` | serve files from the filesystem |
| `m6-auth-server` | verify credentials and sign *JWTs* (*JSON Web Tokens*) |
| `m6-monitor` | poll each node's health and performance endpoints and serve one report |

**Table 2: the six serving binaries and the one job each has.**
Take from it that only `m6-http` is reachable from the internet, in either of its two modes, and that every other process answers HTTP/1.1 on a Unix socket behind it.

Two command line tools sit outside that set: `m6-md` converts a directory of Markdown into one JSON file, and `m6-auth-cli` manages users and groups against the auth database.

A site is data and configuration only: `site.toml`, one config file per backend process, templates, assets, and content as JSON.
There are no binaries in a site and no log directory, because binaries are found through the system path and every process logs to stdout.

---

## 3. Why a family of processes

m6 could have been the one process §1.2 describes and nothing else.
It is a family, and this section says what the other processes buy: a backend in any language, a contained crash, an existing process manager, and capacity added without editing configuration.

### 3.1 Why the boundary is a wire contract

**A backend is reached over HTTP/1.1 on a Unix socket, so it can be written in any language.**
That is the whole reason the boundary is a protocol and not a function call, and it is what stops m6 being a Rust framework only Rust can extend.
`m6-backend-protocol.md` is small enough to implement from scratch in under a hundred lines, and six reference backends in C, C++, Go, Python and Rust are built and tested from it on every run.

This constrains everything in §6: `m6-core` is a convenience for Rust and must never become the only readable definition of any part of the contract.
Behaviour a backend depends on that exists only as Rust is a hole in the specification, and the fix is to specify it.

### 3.2 Why one job each

**A process with one job can be restarted without taking anything else down.**
A template that fails to compile stops HTML rendering and leaves static files and TLS serving.
The `:80` redirect listener is its own process for the same reason, so a slow client there cannot stall the process holding TLS connections, and it builds no QUIC stack, cache or route table it has no use for.

**One job also means one config, one log stream and one unit per instance.**
`m6-html` takes a route table in its config and serves every HTML route in it, so that config is a complete picture of what it does.
Splitting it per route type would multiply units and log streams while leaving each config describing a fragment.

### 3.3 Why systemd owns the lifecycle

**systemd is a better process manager than anything m6 could implement, and it is already on the machine.**
Restart policy, resource limits, dependency ordering, log capture and service isolation are all its job, and reimplementing them inside a proxy would add substantial code for worse results.
So `m6-http` spawns nothing, monitors nothing and restarts nothing, and expects its backends to be running.

Two consequences follow.
Every process logs structured JSON to stdout and journald captures it, so a site has no log directory and no rotation of its own.
And a crashed backend restarts in seconds, which is what makes §4.5 affordable.

### 3.4 Why pool membership is discovered rather than declared

**Scaling a backend should not require editing configuration**, and §1.3 makes scaling the answer to load.
A pool is declared as a socket glob, and membership comes from rescanning that glob every 2 seconds, so starting another systemd instance adds a member and stopping one removes it.
An explicit list would mean a config edit and a reload to add capacity, which is ceremony in the path of the one operation an operator performs under pressure.

The 2-second window is the cost of not watching the socket directory, and it is the honest figure: a new instance is absent from the pool for up to 2 seconds, and a stopped one is retried into its backoff for up to 2 seconds.
Requests go to the member holding the fewest connections, which is the only load signal available locally, and a failed member is retried after 1, 2, 4, 8, 16 and then 30 seconds so a restarting backend is not hammered.

---

## 4. Why one process faces the internet

Everything about the edge follows from it being the only process on a public port.
This section says why the protocol burden is concentrated there, why it runs one thread, why the cache is shaped as it is, why it distrusts its own clients, and why there is no fallback file.

### 4.1 Why the protocol burden is concentrated

**One process terminating TLS and three HTTP versions means one place to get them right.**
Conformance is measured, and Table 3 is the current position.

| suite | target | score |
|---|---|---|
| h1spec | `m6-auth-server`, `m6-file`, `m6-html`, `m6-http` in redirect mode | 32/32 each |
| h2spec | `m6-http` | 146/146 |
| h3spec | `m6-http` | 47/49 |

**Table 3: the recorded conformance floors, from `tools/conformance-scores.txt`.**
Take from it that HTTP/1.1 is measured on four binaries because four of them speak it, and that HTTP/3 is the only suite short of full marks.

A backend never terminates TLS, never parses a frame layer and never implements *HPACK* (*HTTP/2 header compression*), so the defect-dense code has one home and one test surface.
That concentration is what makes §4.4 possible: a rule applied once at the edge holds for every backend behind it, in whatever language.

### 4.2 Why one thread and no async runtime

**A cache hit is a hash map lookup and a write, and an async runtime adds overhead to that.**
This is §1.2 as an implementation rule.
One thread runs the loop and owns every connection's state, so the request path has no cross-thread synchronisation to contend for, and the header scan on the hit path allocates nothing.
Network descriptors, backend descriptors and the file watcher sit in one readiness set, so there is nothing to coordinate between.

The trade is stated plainly: a state machine over one loop is harder to write correctly than a thread per request, and far simpler to reason about under load, because there is no interleaving to consider.
It also produces a tighter tail, because there is no lock convoy and no cross-core cache coherence traffic to spike on.

### 4.3 Why the cache keys and bounds as it does

**Two responses that differ in bytes are two entries, and the rest of the key follows.**
The key is the path, the query string and the content coding.
Each coding is a different body, so it is a different entry, and each is stored when a client asks for it rather than fetched eagerly for codings nobody wants.
The query string is part of what identifies the resource, so it is part of the key.

**The cache has a ceiling because memory does.**
It holds 128MB of response footprint by default and evicts to 87.5% of that when passed, so a large site degrades to a lower hit rate instead of exhausting the machine.

**Two rules decide what may be stored, and both exist to stop one client's response reaching another.**
A response varying on anything except `Accept-Encoding` is never stored, because the key cannot express another dimension.
And whether a backend compresses is declared once in `site.toml`, read by the edge to decide whether to advertise an encoding dimension and read by the backend, which refuses to start when the declaration disagrees with what it does.
The dangerous direction is a backend that compresses while the declaration says it does not, because the edge then stops varying on `Accept-Encoding` and a compressed body can be replayed to a client that asked for identity and cannot read it.

### 4.4 Why the edge distrusts its own clients

**A header the proxy generates is a statement only the proxy can make truthfully, so a client's copy is removed on the way in.**
`x-auth-claims` and `x-forwarded-for` are stripped from every inbound request, on every protocol, before routing.

The reason is specific.
Backends resolve a repeated header by first match, and a proxy appends its own value after the client's, so a forged copy would be the one that wins.
That makes `x-auth-claims` an authentication bypass, since any client could assert membership of any group, and `x-forwarded-for` a rate-limit bypass, since rotating the value defeats a per-address counter.
Every per-address decision in m6, including the login throttle, rests on that strip.

**Two more rules hold because the edge is the only place they can be applied once.**
The security response header set is filled in at serialisation, so cache hits, backend responses, generated error pages and refusals all carry it, and a backend setting its own value for one of them wins.
A request whose framing does not parse ends the connection, because after a framing error there is no way to know where the next request starts, and guessing is how a smuggled request gets through.

### 4.5 Why there is one error route and no fallback file

**One route means one template, and adding a new piece of error context costs no configuration change.**
The edge fetches the configured error path with the status and the original path as query parameters, and returns the rendered HTML under the original status code.
A request already on the error path is answered directly, which refuses recursion without tracking depth.

**There is no static fallback file, because systemd makes one unnecessary.**
A crashed backend restarts in seconds (§3.3), so the window in which no pool member can render an error page is short and a clean status code covers it.
Maintaining an HTML file, validating its presence at startup, holding it in memory and serving it correctly for every content type is work that solves a problem a supervised deployment barely has.
What covers the window instead is a mode: a status code with an empty body, the same with minimal generated HTML, or the configured page, with the richest degrading to the simplest.

---

## 5. Why authentication sits where it does

Authentication is enforced at the edge, verified without a network call, and absent from the code path of a public request.
Each of those was a decision and none is obvious.

### 5.1 Why auth is absent rather than skipped

**A static site is the base case and must be as fast as m6 can make it, so a public route executes no authentication code at all.**
Routes compile into distinct types at startup, and a public route is a different code path from a protected one.
A conditional check on every request would cost something even where the branch is never taken, in branch prediction, in unwrapping an option and in cache lines touched, and on a path measured in microseconds that is a cost with no return for most sites.

A site with no route requiring authentication therefore needs no auth section in `site.toml`, no public key on disk, and no auth process running.
Authentication is an absent feature for that site, and absence has no configuration to get wrong.

### 5.2 Why verification is local

**A network call per authenticated request would add latency to every protected route, and signature verification needs no network.**
The edge holds the auth service's public key and checks the signature, expiry and issuer itself, which is local arithmetic with no I/O.
The auth service is contacted only for operations that change state: login, refresh and logout.

Key rotation therefore needs no coordination.
The signing service starts using a new key immediately, the edge reloads the public key when the file changes, and tokens signed with the old key expire within their lifetime.
Nothing has to be sequenced, because nothing holds a cached decision.

### 5.3 Why enforcement happens twice

**Route-level enforcement at the edge is a boundary that does not depend on every backend implementing authentication correctly.**
An unauthenticated request to a protected route never reaches a backend, and the requirement is declared once in `site.toml` rather than reimplemented per backend.

**Resource-level checks stay in the backend because a route cannot express them.**
Whether this user may read this document depends on the document, not on the path that addressed it.
Verified claims are forwarded so the backend can answer that without re-verifying the token, which is why that header is the one that matters most in §4.4.

---

## 6. Why m6-core exists, and where its boundary falls

`m6-core` is the PHP of m6: a generic library of components a service assembles a web application from.
Breadth is the intent and singularity is the rule, and this section says why those are different questions.

### 6.1 Why breadth is the intent

**A service should be able to build what it needs from one dependency**, which is §1.2 applied to the code rather than to the request path.
So core holds templating, compression, minification, cookies, multipart bodies, an SMTP client, an outbound HTTP client, host metrics and firewall reporting beside the HTTP layers, and a component a website might want belongs in it.
The test of whether that is working is that a new service is small: `m6-html` is six lines and renders every HTML page a site serves.

Breadth is paid for by feature gates.
A service names what it wants, so the weight of core is what a binary uses rather than what core contains, which keeps a command line tool such as `m6-md` from acquiring a QUIC stack or a TLS library by depending on it.

### 6.2 Why each thing in core exists exactly once

**The duplicates had already diverged, and four of them were answering incorrectly on the wire.**
Table 4 is the evidence, and it is why the rule below is worth enforcing.

| what existed more than once | what the copies disagreed about |
|---|---|
| four HTTP/1.1 parsers | conformance, scoring between 14/32 and 27/32 against the same suite |
| three path validators | one performed no character validation at all for a parameter of a particular name, accepting spaces, control bytes and NUL |
| two precondition implementations | one compared entity tags strongly where the specification requires weak comparison, answering 200 where 304 was required |
| three content-coding negotiators | one matched coding names as substrings, so a client that refused brotli with `q=0` was sent brotli |
| four cookie formatters | which cookies carried `HttpOnly` and `Secure` |
| four signal handlers | whether a service unlinked its socket, and whether it logged that it had stopped |
| three route matchers | precedence, so one route table could resolve differently in two services |

**Table 4: what was implemented more than once, and what the copies disagreed about.**
Take from it that four of the seven were producing a wrong answer to a real request, which is why one implementation per concept is a rule.

**The rule: code moves into core when it has more than one consumer, and single-consumer code stays with its consumer.**
A library with one consumer is that consumer's code in another directory, and moving it there buys an abstraction boundary nobody crosses.

Path validation is why validation is grouped as a security boundary: **a security boundary with two implementations has two behaviours.**
There is now one, allowing alphanumerics, `-`, `_`, `.`, and `/` only where a route's parameter spans segments, and refusing `..` anywhere as a substring.

### 6.3 Why HTTP/2 and HTTP/3 stay at the edge

**They have exactly one consumer, permanently, because `m6-http` is the only process that terminates a public connection.**
That is the architecture rather than a current limitation, so the consumer count will not change.

The shared-code argument for moving them was measured and did not hold.
The HTTP/3 path imports exactly one symbol from the HTTP/2 module and nothing else: no frame layer, no HPACK, no stream state machine.
HPACK and *QPACK* (*HTTP/3 header compression*) are different algorithms, and HTTP/2 frames and QUIC streams are different transports, so the only thing the two versions genuinely share is version-independent semantics, which is in core, so they share it with neither wire format moving.

**What this costs is a real cost.**
The edge keeps the largest and most defect-dense body of code in the project, and conformance to the HTTP/2 and HTTP/3 specifications stays its property.
That is accepted in exchange for not maintaining a feature matrix and a large migration for a boundary exactly one caller would ever cross.
If a second consumer appears, a backend terminating HTTP/2 itself, this is the decision to revisit.

The same reasoning keeps the caching rules at the edge: nothing outside `m6-http` evaluates `Cache-Control`, and nothing ever will, because the edge is the cache and the backends sit behind it.

### 6.4 Why the QUIC stack is a fork

**The HTTP/3 conformance gap was upstream, and measurement established that.**
Released quiche scores 37/49 on h3spec.
Twelve failures were traced below the layer m6 works at: eight where a first-flight transport error leaves a correctly built connection close unsendable, two where reserved packet bits are accepted without validation, and two in QPACK.
None was reachable through quiche's public interface, so none could be fixed in m6.

Two open upstream pull requests close ten of the twelve, which is 47/49, and the fork is quiche master plus those two, pinned by revision so the dependency cannot move under a build claiming to be reproducible.
That the transport of a public edge carries community changes upstream has not reviewed is the price, recorded as such, and the exit condition is upstream releasing the fixes.

The remaining two are QPACK and they stay.
quiche reads the peer's QPACK instruction streams and discards them, running a static table only, which is an upstream decision rather than a defect.
Closing the gap would mean new validation on the connection path with per-stream buffering for instructions split across reads, which is where a careless implementation becomes unbounded memory on a stream a peer controls.
That trade was declined for two tests about how politely a hostile peer is refused.

---

## 7. Why every service has one shape

Every m6 service except the edge is an `App`: a Unix socket server with a fixed thread pool, a bounded queue, and routes from configuration.
One shape means one bounded queue, one state model and one shutdown sequence serve all of them.

### 7.1 Why a fixed pool with a bounded queue

**A bounded queue refuses work in a way the edge can act on.**
The pool defaults to the CPU count and the queue to eight times the pool, and a full queue answers 503 immediately.
An unbounded queue would accept work it cannot finish, turning overload into growing latency and eventually memory exhaustion, and a 503 under overload is correct behaviour: it is how backpressure reaches the proxy, which can answer from cache or shed.

Scaling is by starting another instance rather than growing one pool, which is why §3.4 made membership automatic.

**State comes in two tiers so that sharing costs as little as possible.**
A value shared across workers sits behind one reference count, so a request pays one atomic increment and no copy.
A value per worker is owned by its thread and needs no synchronisation, which lets a database connection per worker run in parallel with no lock.
A service declares which it needs once and the builder's type carries it, so a handler receives its state with the real type rather than casting.

### 7.2 Why the request dictionary is layered

**Most of what a template reads is identical for every request, so it is built once and shared.**
Configuration keys, global parameter files and a route's static parameter files are merged once per reload and shared behind a reference count, and only what genuinely varies is allocated per request.
Building the whole map per request meant deep-copying the site's content for every page view, and `PERFORMANCE.md` records what that cost.

**The order within it is a security property.**
Built-in keys such as the request path and the current date go in after every parameter file, so a parameter file cannot override them.
A content file that could redefine the request path could make a page lie about which URL it is, and moving that step earlier would look like a tidy-up while opening exactly that.

### 7.3 Why startup and shutdown belong to core

**A service that drops work on SIGTERM turns every deploy into a handful of failed requests.**
So the first signal drains and exits 0, the second exits immediately, and the sequence is core's.
What a service supplies is data: the name in its lifecycle lines, the socket to unlink on every exit path, and a descriptor to wake a parked loop.

**One sequence exists because several diverged.**
With three ways to install a handler, one service of five unlinked its socket, two logged that they had stopped, and three logged a name that was not their own, and none of that was required by anything the services do.
Core owning the lifecycle lines is what makes searching a journal for a shutdown mean the same thing for every unit.

**Blocking signals is the first statement of `main`, and this is the one ordering rule in m6 that cannot be relaxed.**
A thread inherits the signal mask as it stands when the thread is created, and a service's logging writer is a thread.
A writer started before the mask is set has SIGTERM unblocked, the kernel delivers a process-directed signal there, and the default disposition kills the process instead of draining it.
A supervisor counts death by the signal it sent as a clean stop, so the only symptoms are a missing log line, requests cut rather than drained, and a socket file left behind for the edge to keep in a pool.
The install asserts the mask is already set and refuses to start otherwise, because nothing else about that failure is visible.

---

## 8. Why configuration is split the way it is

A site is configured by `site.toml`, one file per backend process, and one system configuration holding what differs between environments.
The split exists because an operator and a developer own different things.

### 8.1 Why the system configuration holds one section and wins

**The bind address and the TLS certificate paths are the only values that genuinely differ between environments**, so the system configuration holds that one section and nothing else, which keeps its purpose obvious.
It is a required argument rather than an optional one, because one code path is simpler to specify and test than a branch for whether it was supplied.

**It wins on conflict so that a deploy cannot change where a server listens or which certificate it presents.**
An operator needs that to hold regardless of what is deployed over the top, and everything else comes from `site.toml` unchanged.
The result is that `site.toml` carries no password and no certificate path, which is what lets a site's repository be published.

### 8.2 Why a key may have only one owner

**A key set in both a configuration and its secrets file is refused at startup, because a file that is overridden is indistinguishable from a file that is correct when you are reading one file.**
A deployed configuration once stated a mail relay, a sender and a port, all of which were inert because the secrets file replaced them, and an audit read the deployed file and believed it.

The intent behind that overlap was sound: a missing secrets file should fail loudly rather than quietly succeed against the wrong host.
The mechanism inverted it, because a value present and deliberately wrong is what made the configuration lie.
An absent required value fails loudly by itself and names the key that is missing.

### 8.3 Why TLS is always on, and only at the edge

**HTTP/3 requires TLS, so supporting plain HTTP would mean two code paths for one saving.**
Development uses a locally trusted certificate, which makes it a two-command setup and leaves development and production identical in the one layer most likely to behave differently.

**Traffic between the edge and a backend crosses a Unix socket with no TLS**, because a Unix socket is local, faster than loopback TCP, and has no network exposure.
The trust boundary is the edge, which is the concentration §4.4 relies on.

---

## 9. The remaining decisions, and the reason for each

The decisions above shape the system.
These shape working with it, and each is small enough that the reason matters more than the rule.
Table 5 gives them by area.

| area | decision | why |
|---|---|---|
| routing | most specific route wins, decided when a route is compiled | the answer must not depend on the order of blocks in a file somebody edits later |
| routing | a literal beats a parameter, a parameter beats a wildcard | adding a catch-all would otherwise quietly capture traffic from the exact routes beside it |
| routing | `{*name}` spans segments and is legal only last | a wildcard in the middle has no single correct split, and making the last parameter implicitly greedy would change the meaning of every route already written |
| routing | a route naming a handler no code registered is fatal at startup and refused on reload | a route that 404s while the config says it should serve is an outage that looks like a missing page |
| routing | the same parameter syntax in `site.toml` and in a backend's config | one syntax to learn, and one matcher to be correct |
| request data | path parameters are validated before use, and a traversal answers 404 while a malformed value answers 400 | answering 400 to a traversal confirms it was recognised as one, which tells the sender their payload reached the router and is worth varying |
| request data | only `application/x-www-form-urlencoded` bodies are decoded, and any other body on a POST is logged loudly | a client that switched to multipart once produced empty fields everywhere, which looked downstream like a failed check with nothing in any log to say a body had been skipped |
| request data | query parameters appear at the top level and as a nested map | a template wants one and a handler iterating wants the other |
| caching | a route backed by code defaults to `no-store`, and one backed by a template to `public` | a handler computes an answer per request, and inheriting `public` by omission is the defect found by someone else seeing another user's page |
| caching | invalidation is derived from the site's own declarations at startup | the site already says which files a route reads, and a backend that has to announce a change is a backend that can forget to |
| caching | a rendered page carries a validator derived from its inputs | without one every conditional request returned the whole page, and a rendered page has no file of its own to date |
| processes | a socket path is derived from the config filename | one convention, so a unit file and a pool glob cannot disagree about where a service listens |
| processes | two positional arguments, and no environment variables | what a service is doing can be read from the files it was given |
| processes | a connection serves at most 100 requests before closing | persistent connections are the default, and an unbounded one lets one peer hold a slot indefinitely by pipelining |
| processes | an accepted connection has a read deadline, 30 seconds by default | a peer that connects and says nothing otherwise holds a pool worker until it disconnects, and a handful of those is the whole pool while the service looks healthy |
| processes | exit 0 clean, 1 runtime error, 2 configuration or usage error before binding | a supervisor can tell a bad config from a crash, which is what lets a deploy validate a new binary and refuse to install |
| content | minification runs before compression, and each minifier returns its input unchanged when it cannot parse | ratios are better in that order, and a minifier that does not understand a file should pass it through rather than corrupt it |
| content | inline script minification is off by default | the engine parses scripts as modules, which has different scoping, and it can silently rewrite a valid classic script into a broken one |
| content | text types declare a charset and JSON does not | without it clients fall back to a legacy encoding and render UTF-8 as mojibake, and the parameter is undefined for JSON |
| auth | access tokens are short-lived with a longer refresh on its own path | a stolen access token expires on its own, and the refresh path is the only thing that needs to be revocable |
| auth | login is throttled per address | it is the one endpoint where guessing is the attack |
| auth | a requirement is spelled as a group or a role, and an unknown form denies | a typo in a requirement must fail closed |

**Table 5: the remaining decisions by area, each with the reason behind it.**
Take from it that most exist because the alternative had already produced a defect, and that the pattern across them is failing closed and failing loudly.

---

## 10. What is deliberately absent

Naming what m6 does not do is how a reader tells a gap from an omission.
Table 6 lists what is absent and the reason, and the last row is the one to read.

| absent | why |
|---|---|
| a build step | content authoring is not web serving. Coupling them would force m6 to hold opinions about content formats, build tools and file pipelines, and a separate tool that understands the site directory needs no m6 internals |
| Windows | the deployment model is systemd units and Unix sockets |
| a built-in OAuth2 or OIDC provider, MFA, WebAuthn | out of scope at 1.0 |
| horizontal scaling of the edge itself | it is one process per node by design (§1.3) and scales through caching and more nodes |
| HTTP/2 server push | the specification deprecated it, and receipt of a push promise is correctly an error |
| a computed admission control bound | m6 sheds at queue-full, which is not admitting work against a bound. A latency bound comes from the second, and maximum handler time is unbounded today, so there is no epoch to rate-limit against. An event loop is the model in which admission control is expressible, because a loop can decline work while a full queue can only report that it is full |
| **RFC 9218 extensible priorities** | **no decision.** Not implemented and not mentioned anywhere in the tree. Every other gap here was weighed and declined, and this one was not |

**Table 6: what m6 does not do, and why.**
Take from it that all but the last were decided, and that the last is a gap rather than a choice.

---

## 11. Summary

m6 exists because serving a fast website conventionally means assembling a stack, and the assembly is most of the cost.
A multi-process server cannot keep its cache in its own heap, so every cache hit pays serialisation, a lock and an inter-process round trip, and that is a consequence of the concurrency model rather than a tuning problem.
m6 puts the whole hot path on one thread in one process with the cache in the same heap, so a hit costs a hash lookup and a reference count, and it collapses the rest of the stack into small single-job processes behind one wire contract with one `site.toml` describing the set.

What that gives up is stated rather than hidden: one core per process, HTTP/1.1 throughput against a mature server, zero-copy file serving, and kernel TLS.
A site dominated by HTTP/1.1 or by large files is better served elsewhere.

Everything else follows from those two facts.
The boundary between processes is a protocol so a backend can be written in any language, and one process faces the internet so TLS, three HTTP versions, the cache and the trust boundary have one home.
Authentication is absent from a public request rather than skipped, because a branch costs even when it is not taken.
`m6-core` is wide on purpose and singular by rule, because the duplicates had diverged and four of them were answering incorrectly on the wire.
HTTP/2, HTTP/3 and the caching rules stay at the edge because they have one consumer permanently.

Changing any of this means answering the reason rather than editing the rule.
