# m6, design and architecture

This document holds the reasons.
`README.md` says what m6 is and how to run one, `m6-core-reference.md` names every component, `m6-site-toml.md` gives every configuration key, `m6-backend-protocol.md` specifies the wire contract, and `PERFORMANCE.md` and `BENCHMARKS.md` carry the numbers.
None of them says why m6 is built this way, and that is what is written down here.

A decision recorded without its reason is a rule nobody can safely change.
Someone reading only the shape of the system will reasonably conclude that a branch could be added here, a lock moved there, a file introduced to cover an error case, and each of those undoes something that was decided for a reason.
So every section below asks a question and answers it, and the mechanism appears only as far as the reason needs it.

We begin with why m6 is several processes at all (§1) and why only one of them faces the internet (§2).
We then cover why authentication sits where it does (§3), why `m6-core` exists and where its boundary falls (§4), why every service has one shape (§5), and why configuration is split the way it is (§6).
§7 states what is deliberately absent and why, and §8 summarises.

Where this document and the code disagree, the code is right and this document is a defect.

## Contents

1. [Why a family of processes](#1-why-a-family-of-processes)
2. [Why one process faces the internet](#2-why-one-process-faces-the-internet)
3. [Why authentication sits where it does](#3-why-authentication-sits-where-it-does)
4. [Why m6-core exists, and where its boundary falls](#4-why-m6-core-exists-and-where-its-boundary-falls)
5. [Why every service has one shape](#5-why-every-service-has-one-shape)
6. [Why configuration is split the way it is](#6-why-configuration-is-split-the-way-it-is)
7. [What is deliberately absent](#7-what-is-deliberately-absent)
8. [Summary](#8-summary)

---

## 1. Why a family of processes

m6 could have been one binary that listens, renders and serves files.
It is six, each with one job, wired by `site.toml` over Unix sockets, and this section says what that buys.
Four reasons: a backend can be written in any language, a crash is contained, the process manager already exists, and capacity is added without editing configuration.

### 1.1 Why the boundary is a wire contract

**A backend is reached over HTTP/1.1 on a Unix socket, so it can be written in any language.**
That is the whole reason the boundary is a protocol rather than a function call.
`m6-backend-protocol.md` is small enough to implement from scratch in under a hundred lines, and six reference backends in C, C++, Go, Python and Rust are built and tested from it on every run.

This ordering has a consequence that constrains everything in §4: `m6-core` is a convenience for Rust and must never become the only readable definition of any part of the contract.
Behaviour a backend depends on that exists only as Rust is a hole in the specification, and the fix is to specify it.

### 1.2 Why one job each

**A process with one job can be restarted without taking anything else down.**
A template that fails to compile stops HTML rendering and leaves static files and *TLS* (*Transport Layer Security*) serving.
The `:80` listener that answers a redirect runs as its own process for the same reason, so a slow client there cannot stall the process holding TLS connections, and it never builds a QUIC stack, a cache or a route table it has no use for.

**One job also means one config, one log stream and one unit per instance.**
`m6-html` takes a route table in its config and serves every HTML route in it.
Splitting it per route type would multiply units and log streams while leaving each config describing a fragment, and a complete picture of what serves what is worth more than that separation.

### 1.3 Why systemd owns the lifecycle

**systemd is a better process manager than anything `m6-http` could implement, and it is already on the machine.**
Restart policy, resource limits, dependency ordering, log capture and service isolation are all its job, and reimplementing them inside a proxy would add substantial code for worse results.
So `m6-http` spawns nothing, monitors nothing and restarts nothing, and expects its backends to be running.

Two consequences follow.
Every process logs structured JSON to stdout and journald captures it, so a site has no log directory and no log rotation of its own.
And because a crashed backend is restarted in seconds, the window in which every member of a pool is down is short, which is what makes §2.5 affordable.

### 1.4 Why pool membership is discovered rather than declared

**Scaling a backend should not require editing configuration.**
A pool is declared as a socket glob, and membership comes from rescanning that glob every 2 seconds, so starting another systemd instance adds a member and stopping one removes it.
An explicit list of sockets would mean a config edit and a reload to add capacity, which is ceremony in the path of the one operation an operator performs under load.

The 2-second window is the cost of not watching the socket directory, and it is the honest figure: a new instance is not in the pool for up to 2 seconds, and a stopped one is retried into its backoff for up to 2 seconds.
Requests go to the member holding the fewest connections, which is the only signal of load available locally, and a member that fails is retried after 1, 2, 4, 8, 16 and then 30 seconds so a restarting backend is not hammered.

Those four reasons, a protocol boundary, one job each, an existing process manager and discovered membership, describe a system of peers.
The next section is about the one process that is not a peer.

---

## 2. Why one process faces the internet

`m6-http` is the only process on a public port, and everything about its design follows from being the only one.
This section says why the protocol burden is concentrated there, why it runs one thread, why the cache is shaped as it is, and why it distrusts its own clients.

### 2.1 Why the protocol burden is concentrated

**One process terminating TLS, HTTP/1.1, HTTP/2 and HTTP/3 means one place to get them right.**
Conformance is measured, and Table 1 is the current position.

| suite | target | score |
|---|---|---|
| h1spec | `m6-auth-server`, `m6-file`, `m6-html`, `m6-http` in redirect mode | 32/32 each |
| h2spec | `m6-http` | 146/146 |
| h3spec | `m6-http` | 47/49 |

**Table 1: the recorded conformance floors, from `tools/conformance-scores.txt`.**
Take from it that HTTP/1.1 is measured on four binaries because four of them speak it, and that HTTP/3 is the only suite short of full marks.

A backend never terminates TLS, never parses a frame layer and never implements *HPACK* (*HTTP/2 header compression*), so the defect-dense code has one home and one test surface.
That concentration is also what makes §2.4 possible: a rule applied once at the edge holds for every backend behind it, whatever language it is written in.

### 2.2 Why one thread and no async runtime

**A cache hit is a hash map lookup and a write, and an async runtime adds overhead to that.**
One thread runs the loop and owns every connection's state, so the request path has no cross-thread synchronisation to contend for, and the header scan on the cache-hit path allocates nothing.
Network descriptors, backend descriptors and the file watcher all sit in one readiness set, so there is nothing to coordinate between.

The trade is stated plainly: a state machine over one loop is harder to write correctly than a thread per request, and far simpler to reason about under load, because there is no interleaving to consider.
Latency is the measure m6 is judged on, and `BENCHMARKS.md` holds the method for proving a change has not cost any.

### 2.3 Why the cache keys and bounds as it does

**Two responses that differ in bytes are two entries, and everything else about the key follows from that.**
The key is the path, the query string and the content coding.
Each coding is a different body, so it is a different entry, and each is stored only when a client asks for it rather than fetched eagerly for codings nobody wants.
The query string is part of what identifies the resource, so it is part of the key.

**The cache has a ceiling because memory does.**
It holds 128MB of response footprint by default and evicts to 87.5% of that when passed, so a large site degrades to a lower hit rate instead of exhausting the machine.

**Two rules decide what may be stored at all, and both exist to stop one client's response reaching another.**
A response varying on anything except `Accept-Encoding` is never stored, because the key cannot express another dimension.
And whether a backend compresses is declared once in `site.toml`, read by the edge to decide whether to advertise an encoding dimension and read by the backend itself, which refuses to start when the declaration disagrees with what it does.
The dangerous direction is a backend that compresses while the declaration says it does not, because the edge then stops varying on `Accept-Encoding` and a compressed body can be replayed to a client that asked for identity and cannot read it.

### 2.4 Why the edge distrusts its own clients

**A header the proxy generates is a statement only the proxy can make truthfully, so a client's copy is removed on the way in.**
`x-auth-claims` and `x-forwarded-for` are both stripped from every inbound request, on every protocol, before routing.

The reason is specific.
Backends resolve a repeated header by first match, and a proxy appends its own value after the client's, so a forged copy would be the one that wins.
That makes `x-auth-claims` an authentication bypass, since any client could assert membership of any group, and `x-forwarded-for` a rate-limit bypass, since rotating the value defeats a per-address counter.
Every per-address decision in the system, including `m6-auth-server`'s login throttle, rests on that strip.

**Two more rules hold for the same reason: the edge is the only place they can be applied once.**
The security response header set is filled in at serialisation, so cache hits, backend responses, generated error pages and refusals all carry it, and a backend that sets its own value for one of them wins.
A request whose framing does not parse ends the connection, because after a framing error there is no way to know where the next request starts, and guessing is how a smuggled request gets through.

### 2.5 Why there is one error route and no fallback file

**One route means one template, and adding a new piece of error context costs no configuration change.**
The edge fetches the configured error path with `status` and `from` as query parameters and returns the rendered HTML under the original status code.
A request already on the error path is answered directly, which is how recursion is refused without tracking depth.

**There is no static fallback file, because systemd makes one unnecessary.**
A crashed backend is restarted in seconds (§1.3), so the window in which no pool member can render an error page is short, and a clean status code covers it.
Maintaining an HTML file, validating its presence at startup, holding it in memory and serving it correctly for every content type is work that solves a problem a supervised deployment barely has.
What covers the window instead is a mode: a status code with an empty body, the same with minimal generated HTML, or the configured error page, with the richest degrading to the simplest.

The edge is therefore where protocol, caching, trust and error policy all live.
The next section is about the work it deliberately does not do on a public request.

---

## 3. Why authentication sits where it does

Authentication is enforced at the edge, verified without a network call, and absent from the code path of a public request.
This section says why each of those three is true, because each was a decision and none is obvious.

### 3.1 Why auth is absent rather than skipped

**A static site is the base case and must be as fast as m6 can make it, so a public route executes no authentication code at all.**
Routes compile into distinct types at startup, and a public route is a different code path from a protected one.
A conditional check on every request would cost something even where the branch is never taken, in branch prediction, in unwrapping an option and in cache lines touched, and on a path measured in microseconds that is a cost with no return for the majority of sites.

A site with no route requiring authentication needs no auth section in `site.toml`, no public key on disk, and no `m6-auth-server` process running.
Authentication is an absent feature for that site, and absence has no configuration to get wrong.

### 3.2 Why verification is local

**A network call per authenticated request would add latency to every protected route, and signature verification needs no network.**
The edge holds `m6-auth-server`'s public key and checks the signature, expiry and issuer itself, which is local arithmetic with no I/O.
The auth service is contacted only for operations that change state: login, refresh and logout.

Key rotation therefore needs no coordination.
The signing service starts using a new key immediately, the edge reloads the public key when the file changes, and tokens signed with the old key expire on their own within their lifetime.
Nothing has to be sequenced, because nothing holds a cached decision.

### 3.3 Why enforcement happens twice

**Route-level enforcement at the edge is a boundary that does not depend on every backend implementing authentication correctly.**
An unauthenticated request to a protected route never reaches a backend, and the requirement is declared once in `site.toml` rather than reimplemented per backend, in whatever language each is written in.

**Resource-level checks stay in the backend because a route cannot express them.**
Whether this user may read this document depends on the document, not on the path that addressed it.
Verified claims are forwarded to the backend so it can answer that question without re-verifying the token, which is why the forwarding header is the one that matters most in §2.4.

---

## 4. Why m6-core exists, and where its boundary falls

`m6-core` is the PHP of m6: a generic library of components a service assembles a web application from.
Breadth is the intent, and singularity is the rule.
This section says why those two are different questions, why some things stay outside, and why the one exception to all of it is a forked dependency.

### 4.1 Why breadth is the intent

**A service should be able to build what it needs from one dependency.**
So core holds templating, compression, minification, cookies, multipart bodies, an SMTP client, an outbound HTTP client, host metrics and firewall reporting beside the HTTP layers, and a component a website might want belongs in it.
The test of whether that is working is that a new service is small: `m6-html` is six lines and renders every HTML page a site serves.

Breadth is paid for by feature gates.
A service names what it wants, so the weight of core is what a given binary uses rather than what core contains, which is what keeps a command line tool such as `m6-md` from acquiring a QUIC stack or a TLS library by depending on it.

### 4.2 Why each thing in core exists exactly once

**The duplicates had already diverged, and four of them were answering incorrectly on the wire.**
Table 2 is the evidence, and it is the reason the rule below is worth enforcing.

| what existed more than once | what the copies disagreed about |
|---|---|
| four HTTP/1.1 parsers | conformance, scoring between 14/32 and 27/32 against the same suite |
| three path validators | one performed no character validation at all for a parameter of a particular name, accepting spaces, control bytes and NUL |
| two precondition implementations | one compared ETags strongly where the specification requires weak comparison, answering 200 where 304 was required |
| three content-coding negotiators | one matched coding names as substrings, so a client that refused brotli with `q=0` was sent brotli |
| four cookie formatters | which cookies carried `HttpOnly` and `Secure` |
| four signal handlers | whether a service unlinked its socket, and whether it logged that it had stopped |
| three route matchers | precedence, so one route table could resolve differently in two services |

**Table 2: what was implemented more than once, and what the copies disagreed about.**
Take from it that four of the seven were producing a wrong answer to a real request, which is why one implementation per concept is a rule.

**The rule: code moves into core when it has more than one consumer, and single-consumer code stays with its consumer.**
A library with one consumer is that consumer's code in another directory, and moving it there buys an abstraction boundary nobody crosses.

Path validation is the clearest case of why this matters, and it is why validation is grouped as a security boundary: **a security boundary with two implementations has two behaviours.**
There is now one, allowing alphanumerics, `-`, `_`, `.`, and `/` only where a route's parameter spans segments, and refusing `..` anywhere as a substring.
A rejected traversal answers 404 and a merely malformed value answers 400, because answering 400 to a traversal confirms to the sender that it was recognised as one, which tells them their payload reached the router and is worth varying.

### 4.3 Why HTTP/2 and HTTP/3 stay at the edge

**They have exactly one consumer, permanently, because `m6-http` is the only process that terminates a public connection.**
That is the architecture rather than a current limitation, so the consumer count will not change.

The shared-code argument for moving them was measured and did not hold.
The HTTP/3 path imports exactly one symbol from the HTTP/2 module, and nothing else: no frame layer, no HPACK, no stream state machine.
HPACK and *QPACK* (*HTTP/3 header compression*) are different algorithms and HTTP/2 frames and QUIC streams are different transports, so the only thing the two versions genuinely share is version-independent semantics, which is in core, so they still share it with neither wire format moving.

**What this costs is worth stating, because it is a real cost.**
The edge keeps the largest and most defect-dense body of code in the project, and conformance to the HTTP/2 and HTTP/3 specifications remains its property rather than core's.
That is accepted deliberately in exchange for not maintaining a feature matrix and a large migration for a boundary exactly one caller would ever cross.
If a second consumer ever appears, a backend that terminates HTTP/2 itself, this is the decision to revisit rather than work around.

The same reasoning keeps the RFC 9111 caching rules at the edge.
Nothing outside `m6-http` parses or evaluates `Cache-Control`, and by the architecture nothing ever will, because the edge is the cache and the backends sit behind it.

### 4.4 Why the QUIC stack is a fork

**The HTTP/3 conformance gap was upstream rather than ours, and measurement is what established that.**
Released quiche scores 37/49 on h3spec.
Twelve failures were traced to the layer below m6: eight where a first-flight transport error leaves a correctly built connection close unsendable, two where reserved packet bits are accepted without validation, and two in QPACK.
None was reachable through quiche's public interface, so none could be fixed in m6.

Two open upstream pull requests close ten of the twelve, which is 47/49, and the fork is quiche master plus those two, pinned by revision so the dependency cannot move under a build that claims to be reproducible.
That the transport of a public edge carries community changes upstream has not reviewed is the price, it is recorded as such, and the exit condition is upstream releasing the fixes.

The remaining two failures are QPACK, and they stay.
quiche reads the peer's QPACK instruction streams and discards them, running a static table only, which is a decision upstream made rather than a defect.
Closing the gap would mean new validation on the connection path with per-stream buffering for instructions split across reads, which is where a careless implementation becomes unbounded memory on a stream a peer controls.
That trade was declined for two tests about how politely a hostile peer is refused.

---

## 5. Why every service has one shape

Every m6 service except the edge is an `App`: a Unix socket server with a fixed thread pool, a bounded queue, and routes from configuration.
This section says why one shape at all, why that concurrency model, why the request dictionary is layered, and why startup and shutdown are not each service's business.

### 5.1 Why a fixed pool with a bounded queue

**A bounded queue refuses work in a way the edge can act on.**
The pool defaults to the CPU count and the queue to eight times the pool, and a full queue answers 503 immediately.
An unbounded queue would accept work it cannot finish, turning an overload into growing latency and eventually memory exhaustion, and a 503 under overload is correct behaviour rather than a failure: it is how backpressure reaches the proxy, which can then answer from cache or shed.

Scaling is by starting another instance rather than by growing one pool, which is why §1.4 made membership discovered.

**State comes in two tiers so that sharing costs as little as possible.**
A value shared across workers sits behind one reference count, so a request pays one atomic increment and no copy.
A value per worker is owned by its thread and needs no synchronisation at all, which is what lets a database connection per worker run in parallel with no lock.
A service declares which it needs once, and the builder's type carries it, so a handler receives its state with the real type rather than casting.

### 5.2 Why the request dictionary is layered

**Most of what a template reads is identical for every request, so it is built once and shared.**
Configuration keys, global parameter files and a route's static parameter files are merged once per reload and shared behind a reference count, and only what genuinely varies per request is allocated per request.
Building the whole map per request meant deep-copying the site's content for every page view, and `PERFORMANCE.md` records what that cost.

**The order within it is a security property, not a convenience.**
Built-in keys such as the request path and the current date go in after every parameter file, so a parameter file cannot override them.
A content file that could redefine the request path could make a page lie about which URL it is, and moving that step earlier would look like a tidy-up while opening exactly that.

### 5.3 Why startup and shutdown belong to core

**A service that drops work on SIGTERM turns every deploy into a handful of failed requests.**
So the first signal drains and exits 0, the second exits immediately, and the sequence is core's rather than each service's.
What a service supplies is data: the name in its lifecycle lines, the socket to unlink on every exit path, and a descriptor to wake a loop that is parked.

**One sequence exists because several diverged.**
With three ways to install a handler, one service of five unlinked its socket, two logged that they had stopped, and three logged a name that was not their own, and none of that difference was required by anything the services do.
Core owning the lifecycle lines is what makes searching a journal for a shutdown mean the same thing for every unit.

**Blocking signals is the first statement of `main`, and this is the one ordering rule in m6 that cannot be relaxed.**
A thread inherits the signal mask as it stands when the thread is created, and a service's logging writer is a thread.
A writer started before the mask is set has SIGTERM unblocked, the kernel delivers a process-directed signal there, and the default disposition kills the process instead of draining it.
A supervisor counts death by the signal it sent as a clean stop, so the only symptoms are a missing log line, requests cut rather than drained, and a socket file left behind for the edge to keep in a pool.
The install asserts the mask is already set and refuses to start otherwise, because nothing else about that failure is visible.

**Exit codes separate a bad configuration from a crash**: 0 for a clean stop, 1 for a runtime error, and 2 for a configuration or usage error, always before anything binds.
That distinction is what lets a deploy validate a new binary against an existing configuration and refuse to install rather than discover the disagreement in production.

---

## 6. Why configuration is split the way it is

A site is configured by `site.toml`, one file per backend process, and one system configuration holding what differs between environments.
This section says why that third file exists, why it wins, and why a secret may have only one home.

### 6.1 Why the system configuration holds one section and wins

**The operator and the developer own different things, and the split is what keeps them apart.**
The bind address and the TLS certificate paths are the only values that genuinely differ between environments, so the system configuration holds that one section and nothing else, which keeps its purpose obvious.
It is a required argument rather than an optional one, because one code path is simpler to specify and test than a branch for whether it was supplied.

**It wins on conflict so that a deploy cannot change where a server listens or which certificate it presents.**
An operator needs that to hold regardless of what is deployed over the top of it, and everything else, routes, backends, authentication and logging, comes from `site.toml` unchanged.

The result is that `site.toml` carries no password and no certificate path, which is what lets a site's repository be published.

### 6.2 Why a key may have only one owner

**A key set in both a configuration and its secrets file is refused at startup, because a file that is overridden is indistinguishable from a file that is correct when you are reading one file.**
A deployed configuration once stated a mail relay, a sender and a port, all four of which were inert because the secrets file replaced them, and an audit read the deployed file and believed it.

The intent behind that overlap was sound: a missing secrets file should fail loudly rather than quietly succeed against the wrong host.
The mechanism inverted it, because a value that is present and deliberately wrong is what made the configuration lie.
An absent required value fails loudly by itself and names the key that is missing, which is what the refusal now produces.

### 6.3 Why TLS is always on, and only at the edge

**HTTP/3 requires TLS, so supporting plain HTTP would mean two code paths for one saving.**
Development uses a locally trusted certificate, which makes it a two-command setup and leaves development and production identical in the one layer most likely to behave differently.

**Traffic between the edge and a backend crosses a Unix socket with no TLS**, because a Unix socket is local, faster than loopback TCP, and has no network exposure.
The trust boundary is therefore the edge, which is the same concentration that §2.4 relies on.

---

## 7. What is deliberately absent

Naming what m6 does not do is how a reader tells a gap from an omission.
Table 3 lists what is absent and the reason for each, and the last row is the one to read.

| absent | why |
|---|---|
| a build step | content authoring is not web serving. Coupling them would force m6 to hold opinions about content formats, build tools and file pipelines, and a separate tool that understands the site directory needs no m6 internals |
| Windows | the deployment model is systemd units and Unix sockets |
| a built-in OAuth2 or OIDC provider, MFA, WebAuthn | out of scope at 1.0 |
| horizontal scaling of the edge itself | it runs as a single instance per node and scales through caching |
| HTTP/2 server push | the specification deprecated it, and receipt of a push promise is correctly an error |
| a computed admission control bound | m6 sheds at queue-full, which is not the same as admitting work against a bound. A latency bound comes from the second, and maximum handler time is unbounded today, so there is no epoch to rate-limit against. An event loop is the model in which admission control is expressible, because a loop can decline work while a full queue can only report that it is full |
| **RFC 9218 extensible priorities** | **no decision.** Not implemented and not mentioned anywhere in the tree. Every other gap here was weighed and declined, and this one was not, which is why it is named rather than left out |

**Table 3: what m6 does not do, and why.**
Take from it that all but the last were decided, and that the last is a gap rather than a choice.

---

## 8. Summary

m6 is several processes rather than one because the boundary between them is a wire contract, which is what lets a backend be written in any language, and because one job per process means a restart is contained.
Only one process faces the internet, which concentrates TLS and three HTTP versions in one place to get right, lets one strip of client-supplied headers protect every backend behind it, and puts the cache where the request already arrives.
That process runs a single thread because a cache hit is a hash lookup, and its cache is bounded because memory is.

Authentication is absent from a public request rather than skipped, because a branch costs even when it is not taken, and it is verified locally because a network hop per request would cost every protected route.
`m6-core` is wide on purpose and singular by rule: a service assembles a web application from one dependency, and each thing in it exists exactly once, because the duplicates had diverged and four of those divergences were correctness or security defects.
HTTP/2, HTTP/3 and the caching rules stay at the edge because they have one consumer permanently, and a library with one consumer is that consumer's code in another directory.

Every service has one shape so that one bounded queue, one state model and one shutdown sequence serve all of them, and the two ordering rules inside that shape each carry a reason recorded beside them: built-ins go in after parameter files so a content file cannot rewrite the request, and signals are blocked before anything spawns a thread so the process drains instead of dying.

Changing any of this means answering the reason rather than editing the rule.
