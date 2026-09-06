# m6 release notes

Newest first. One entry per change that reaches a running node.

Each entry records what changed, why it mattered, and how it was verified.
"Verified" means measured against a running server, not inferred from the
source: several defects in this list were invisible in the code and only showed
up against the deployed artefact.

Deploy order is fixed: **test locally, commit, then deploy.** Never the reverse.

---

## m6-http: strict framing for backend HTTP/1.1 responses (F028-F031, F035)

Message framing is the security boundary between this proxy and its backend: if
the two disagree about where a response ends, the leftover bytes become the
head of the next response on a reused connection. Each of these was a way to
disagree, so each is now a hard error rather than a guess.

- **F028** conflicting `Content-Length`. The parser kept the last value seen,
  so a backend emitting two different lengths framed the response by whichever
  came last. Now every value present -- across repeated fields and within a
  comma-separated list -- must agree, or the response is refused. Repeated but
  identical values still work; that is pinned by its own test so the fix cannot
  reject legitimate traffic.
- **F029** `Transfer-Encoding` together with `Content-Length` is refused. RFC
  9112 6.1 forbids sending both, and a recipient that picks one is the classic
  smuggling primitive because the next hop may pick the other.
- **F030** `Transfer-Encoding` is a list and only the final coding frames the
  message. Only a value that was literally `chunked` was recognised, so a valid
  `gzip, chunked` was treated as unframed and the chunk envelope was returned
  as though it were the body. A `Transfer-Encoding` not ending in `chunked`
  now errors instead of falling back to read-to-EOF.
- **F031** an unparseable `Content-Length` is refused. It became `None` via
  `.ok()` and fell through to read-to-EOF, silently switching framing mode on
  malformed input. Digits are now validated explicitly (`1*DIGIT`, RFC 9110
  8.6) rather than left to `str::parse`, **which accepts a leading `+`** -- so
  `Content-Length: +5` had been read as 5. That was caught by the test, not by
  reading the code.
- **F035** responses to HEAD, and 1xx/204/304, are bodyless whatever their
  headers claim (RFC 9112 6.3). The reader did not know the request method and
  blocked waiting for `Content-Length` bytes a correct backend never sends, so
  every HEAD to a backend stalled until the read timeout. `read_response_for`
  takes the method; `read_response` remains for callers that cannot know it.

Ten tests. The bodyless rule is paired with a test that a GET with identical
headers still reads its body, so it cannot have been implemented by ignoring
bodies generally.

632 workspace tests pass. Zero warnings on Linux and macOS.


## m6-http: refuse to forward requests that would smuggle HTTP/1.1 framing (F036/F094)

Decoded HTTP/2 and HTTP/3 header fields were written into HTTP/1.1 request
syntax verbatim. H2 and H3 header fields are length-delimited, so a value may
contain any byte; HTTP/1.1 is CRLF-delimited, so a CR or LF in that value stops
being data and becomes framing. A client could therefore append arbitrary
headers, or a whole second request, to what the backend received.

This is the egress half of the bare-LF class fixed earlier on ingress, and the
worse half: on ingress a malformed request is the client's problem, while here
the proxy generates the malformed bytes itself and does so with the backend's
trust behind it.

`check_forwardable` now gates both forwarding paths (unix socket and URL
upstream) before a connection is opened, and covers everything written into the
request line and header block, not just the header loop:

- field values: no CR, LF or NUL
- field names: RFC 9110 5.6.2 tokens, so a colon or space cannot split a header
- method: no space (a space forges the request line without any CR)
- path and query: no CR, LF, NUL or space
- `X-Forwarded-For` / `X-Forwarded-Host`, both derived from client-controlled
  input and written by the proxy itself

Refused, not sanitised: RFC 9113 8.2.1 makes such a message malformed, and
silently rewriting a request before handing it to a backend hides an attack
rather than stopping it. Hop-by-hop headers are exempt because they are dropped
before serialisation and cannot reach the backend; that exemption is pinned by
a test so the skip list and the check cannot drift into a hole.

Six tests, verified failing with the check disabled -- five of the six fail,
and the sixth (legitimate traffic still passes) correctly does not, which is
what makes it worth keeping. obs-text is explicitly allowed, so the fix cannot
regress into "reject anything non-ASCII".

622 workspace tests pass. Zero warnings on Linux and macOS.


## m6-core: a config reload no longer silences logging

`LogHandle::reload` rebuilt the whole layer -- new writer, new `fmt` layer, new
`.with_filter(..)` -- and handed it to `reload::Handle::modify`. That is
unsupported: `.with_filter(..)` produces a `Filtered` layer, per-layer filter
ids are assigned when the subscriber is *constructed*, and a `Filtered` layer
swapped in afterwards has no id and panics on the first event through it.

In production the symptom was not a crash. m6-http kept serving traffic and
reporting itself healthy while every log target except `analytics` went silent
-- stats, pool events, warnings and errors all gone. `analytics` survived only
because it is a separate layer with its own writer that reload never touches.
Measured on one node across the same PID: stats 1788 -> 0, m6_http 124 -> 0,
pool 74 -> 0, rustls 60 -> 0, analytics 954 -> 42. A deploy touches site.toml
and that triggers a reload, so every deploy blinded the server to its own
errors.

Now only the FILTER is reloaded. The `Filtered` wrapper is built once at
registration and keeps its id for the process lifetime; only the filter value
inside it swaps. The stdout writer is likewise created once rather than per
reload. Format cannot change this way (json and text are distinct layer types),
so a reload requesting a different format keeps the current one and warns
instead of pretending; the level, which is the knob that gets used, still
applies. A failed reload now logs at ERROR rather than being swallowed.

Test: `m6-core/tests/log_reload.rs`. The subscriber is process-global and
cannot be torn down, so it re-executes the test binary as a child and inspects
real stdout -- asserting a line before the reload (so a pass cannot be
vacuous), then after one reload, then after a second. Asserting that `reload()`
returned would prove nothing: it returned cleanly the whole time it was
writing into a dead layer. Verified failing against the old code with the exact
production panic, then passing.


## m6-file: lengthen only the shared-cache lifetime for unversioned assets

Unversioned assets (the webfont, `manifest.json`, any asset requested without
its `?v=` hash) carried `max-age=60, stale-while-revalidate=60`. On a
low-traffic origin the edge entry expired faster than requests arrived, so the
cache re-fetched them from the backend roughly once a minute: measured at a 45%
asset hit rate, with the webfont at 21 misses to 14 hits.

Added `s-maxage=86400`. Deliberately **not** a longer `max-age` or
`stale-while-revalidate`: both are honoured by browsers, and a browser cache
cannot be invalidated, so raising either strands visitors on the previous file
for a day. That was tried before and rolled back. `s-maxage` is defined for
shared caches only (RFC 9110/9111 5.2.2.10) and is ignored by browsers, so it
lengthens exactly the copy an operator can evict.

Versioned `?v=<hash>` assets keep `max-age=31536000, immutable` and are
unchanged.

The rule moved into `cache_control_for()`. It had been an inline expression, and
the tests asserted a *copy* of the condition pasted into the test module, so the
emitted directives were never covered -- the browser-facing window could have
been lengthened with every test still green. Tests now call the real function
and assert the exact strings, plus the invariant that the long window appears
only on `s-maxage`. Both were verified to fail against the previous directive
and against the rolled-back mistake. 608 workspace tests pass; zero warnings on
Linux and macOS.


## 2026-09-06 — `Date` on every response, and complete 304 metadata

### `Date` (F001)

RFC 9110 6.6.1 MUST. m6 has a clock and generated no `Date` on anything. A
recipient cannot compute a response's age without it, which is why the `Age`
header added earlier was not sufficient on its own — the two are only useful
together.

**Stamped before the cache insert, not after.** This is the part worth
recording. `Vary` is deliberately applied *after* the insert so `should_cache`
sees the backend's own value; copying that pattern for `Date` produced a cache
hit whose headers had no `Date` at all. Adding a fresh one on the hit path
would have been worse still: it would claim the response was generated just now
while the `Age` beside it said sixty seconds. `Date` means generation time, so
it has to be captured at generation and stored.

Verified end to end: MISS carries `Date` and no `Age`; a hit three seconds later
carries the **same** `Date` and `Age: 3`.

### 304 responses were missing their metadata (F004, F011, F062)

RFC 9110 15.4.5: a 304 carries the metadata a 200 would have, so a client can
update its stored response from it. m6 sent only `ETag`, `Last-Modified` and
`Cache-Control`.

`Vary` was the damaging omission: a client or shared cache updating its stored
entry from such a 304 loses the knowledge that the response varies by
`Accept-Encoding`, and can then reuse a brotli body for a gzip-only request.
`Date` was missing too, leaving the recipient nothing to compute age from.

Now carries `Vary`, `Date`, `Age`, `Expires` and `Content-Location` alongside
the validators. `Content-Length` is deliberately **not** carried: RFC 9110 8.6
permits it on a 304 only when it equals the 200's length, and getting that
wrong is worse than omitting it.

A second bug surfaced while fixing this and is worth naming: the 304 path reads
from the *stored* headers, so it inherited the same gap — `Vary` and `Date`
were absent there for the same reason. Adding them at the three 304
construction sites was required as well as fixing the filter. A filter that
copies fields correctly is no use when the source never had them.

604 workspace tests pass across two consecutive runs, zero warnings.

---

## 2026-09-06 — `Expires`-based freshness (F047)

`Expires` was not consulted at all. A response using the older header — still
perfectly valid, and what a great deal of software emits — fell straight
through to "no freshness given" and was treated as fresh indefinitely. Exactly
backwards: it carried an explicit expiry and the cache ignored it.

Now implemented in the precedence RFC 9111 4.2.1 requires: `s-maxage`, then
`max-age`, then `Expires - Date`, then the bounded heuristic.

Measured against the **response's own `Date`**, not our clock. Using local time
would silently lengthen or shorten the lifetime by however much the two
servers' clocks disagree, which is a real effect on a cache sitting between two
machines. A missing `Date` falls back to now; an `Expires` at or before `Date`
means already stale, and yields zero rather than a duration that would wrap.

Five tests, including clock skew (Date and Expires both shifted an hour, and
the interval is still honoured), `max-age` outranking `Expires`, a past
`Expires`, and unparseable garbage falling back to the heuristic rather than
being mistaken for an expiry.

601 workspace tests pass across two consecutive runs, zero warnings.

---

## 2026-09-06 — Bounded heuristic freshness, and unsafe-method invalidation

### `Cache-Control: public` no longer means fresh forever (F048)

A response with no explicit lifetime stored with `expires_at = None` and stayed
fresh indefinitely. That was the deliberate CDN-style model the deploy pipeline
relies on — content lives until `invalidate-cache.sh` clears it — but
"forever" is not a defensible reading of RFC 9111 4.2.2, which permits a
*heuristic* freshness lifetime, not an unbounded one. An entry whose
invalidation was missed for any reason would be served indefinitely.

Bounded at 24 hours. The model is intact: deploys invalidate far more often
than that, so in normal operation nothing expires that the pipeline was not
going to clear anyway. What changes is that a missed invalidation is now a
bounded fault rather than a permanent one. Nothing this site serves depends on
it — every route and asset carries an explicit `max-age`.

### A successful POST/PUT/DELETE now invalidates the cached URI (F058)

RFC 9111 4.4 MUST. Nothing invalidated anything: after a successful
state-changing request the cache kept serving the previous representation
until it expired on its own. That is a live concern the moment the CMS returns
— edit a page, and the edge keeps serving the old one.

Two deliberate restrictions:

- **Only on a non-error status.** A 4xx/5xx means the state change did not
  happen, so the cached copy is still correct. Invalidating on failure would
  also hand anyone a trivial way to flush the cache by spamming failing POSTs.
- **`Location`/`Content-Location` only when same-origin.** An off-site redirect
  target is not ours to evict, and following one blindly would let a backend
  clear arbitrary entries.

Safe methods (GET, HEAD, OPTIONS, TRACE, per RFC 9110 9.2.1) change nothing and
are skipped; everything else counts as state-changing, including methods this
server does not itself implement.

### A third flaky test, same shape as the others

`m6-html`'s `spawn_server` waited for the socket *file* to appear, which is not
the same as the server accepting on it. Under a full run the connect raced the
listen backlog, and `http_request` returns an empty string on I/O error — its
comment says "so callers can retry rather than panic", but no caller retries —
so an assertion on `"200 OK"` failed against `""`. Now polls with a real
request until the server answers.

That is the third instance of the same mistake in this codebase's tests:
waiting for a proxy for readiness instead of readiness itself.

596 workspace tests pass across three consecutive runs, zero warnings.

---

## 2026-09-06 — `Age` on cache-served responses (F045, F046)

RFC 9111 5.1 requires a shared cache to send `Age`. Nothing emitted it at all,
so a downstream cache had no way to know how old what we handed it already was
and treated a minute-old response as newly generated.

Two halves, and the second is the one that matters in a chain:

- **`Age` is now emitted** on every cache-served response, at all three
  cache-hit sites. Verified live: absent on MISS, 3 after three seconds, 8
  after eight, exactly one header.
- **Upstream age carries forward** (F046). RFC 9111 4.2.3 defines age as time
  since the response was *generated*, not since this cache happened to store
  it. An entry that arrived already one hop old had its clock reset to zero,
  so each hop in a chain made the content look fresher than it was. The `Age`
  the origin declared is now folded into the stored entry and added to the
  time held locally.

The age travels on the `Lookup` result rather than being written into the
stored headers: the value changes every second, the stored headers are shared
behind an `Arc`, and the callers already build an owned header vector to add
`Vary`/`alt-svc` — so this costs nothing on the cache-hit path.

Four tests, including a malformed upstream `Age` being ignored rather than
poisoning the calculation.

Still open in this area: `Expires`/`Date`-based freshness (F047), bare `public`
being fresh forever (F048), and unsafe-method invalidation (F058).

594 workspace tests pass, zero warnings.

---

## 2026-09-06 — 501 vs 405, case-sensitive methods, and two more allocation bounds

### An unrecognised method is 501, not 405 (F016)

RFC 9110 15.5.6 vs 15.6.2: 405 means the method is *known* and the resource
will not do it, and MUST carry `Allow`; 501 means the server does not implement
the method at all. Everything got 405, which claims knowledge of an invented
verb and points the client at the resource when the method is the problem.

This one was mine — introduced with the method gate earlier the same day.

### The method gate could be bypassed by changing case

Found while testing the above, and the more serious of the two.

`method_may_read_cache` compared with `eq_ignore_ascii_case`, while the
configured allow-list compares exactly. The cache lookup runs **before** the
method gate, so a request with method `get` matched the cache predicate, was
served from cache, and never reached the check that would have refused it.

RFC 9110 9.1 makes the method token case-sensitive, so `get` is simply not GET.
Both cache predicates are now exact matches. The general lesson is the one
worth keeping: two comparisons of the same thing, in different places, that
disagree about case is a bypass waiting to be found.

Verified on a raw socket: `get`, `Get` and `head` now return 501 where they
previously returned 200 from cache.

### `HTTP/1.1 501 Unknown`

501 was missing from the reason-phrase table and fell through to the default,
so the server emitted a real status with a phrase describing nothing. Added,
along with 504.

### Two more unbounded allocations (H003, H004)

- **H003, the sharper one.** `vec![0u8; len]` allocated the backend's declared
  `Content-Length` *before reading a byte*, so a backend declaring 4 GB
  allocated 4 GB immediately. Now refused above 128 MiB — before allocating,
  since the allocation is the damage. The bodyless read-to-EOF path was
  unbounded the same way and is now capped too.
- **H004.** H3 request bodies accumulated with no ceiling, the same defect
  fixed for h2 earlier. Same 20 MiB limit so the two protocols cannot disagree
  about what is acceptable; over-limit streams get a 413 and are reset.

590 workspace tests pass across two consecutive runs, zero warnings.

---

## 2026-09-06 — HTTP/2 frame validation: a remote panic, and flow-control accounting

From the RFC audit, revision 1. The first item is the reason this jumped the queue.

### A three-byte frame could kill the process (F076)

```rust
if flags & FLAG_PRIORITY != 0 { pos += 5; }
let header_block = &payload[pos..];        // panics when payload.len() < pos
```

A HEADERS frame declaring PRIORITY but carrying fewer than the five bytes the
priority fields require sliced out of range and aborted. Reachable immediately
after the connection preface, with no other setup and no valid request.

Verified rather than assumed: restoring the original line makes the new test
fail with `range start index 5 out of range for slice of length 0`. (A first
attempt at that verification was itself wrong — a partial revert left the new
padding bounds-check in place, which also guards the slice, so the test passed
and looked vacuous. Reverting to the exact original line showed the panic.)

Now a FRAME_SIZE_ERROR, per RFC 9113 6.2.

### Padding was fed to the HPACK decoder (same six lines)

`&payload[pos..]` ran to the end of the payload, so trailing pad bytes were
passed to HPACK as though they were field data. The header block is now
`payload[pos .. len - pad]`.

### Flow-control accounting (F078-F081)

- **RFC 9113 6.9.1: the whole payload counts against flow control**, padding
  and pad-length byte included. Only `data.len()` was charged, so a peer padding
  heavily reclaimed credit it never spent and the two ends' views of the window
  drifted apart.
- **No per-stream receive window existed at all** (F080) — only the connection
  window, so a single stream could consume the entire connection's credit.
  `H2Stream` now carries one.
- **An empty DATA frame emitted `WINDOW_UPDATE` with increment 0**, which is
  itself a PROTOCOL_ERROR (F081). Never emitted now.
- **`PADDED` with an empty payload** was silently treated as unpadded (F078).

### Request bodies were unbounded (H001)

Not an RFC clause; a robustness finding the audit listed separately, and one of
the two it rated Critical. Bodies accumulated with no ceiling while flow-control
credit was returned, so a peer could stream indefinitely and grow the process
until it was killed. Capped at 20 MiB — above the renderers' own 16 MiB
multipart limit, so it never trips before their check does.

### Tests

Seven, including a sweep of every frame type against a spread of flag
combinations, payload lengths and stream IDs, asserting only that nothing
panics. That property is the one that matters here: a remote peer controls
every byte, so any panic is a remote kill. It is also what the audit asked for
under "fuzz targets for every frame parser".

587 workspace tests pass, zero warnings.

---

## 2026-09-06 — Reject a bare LF in the header block, and a raw-socket robustness suite

### The defect

`httparse` accepts a lone LF as a line terminator, so a header **value**
containing a raw `\n` was silently split into two headers:

```
X-Test: a\nX-Injected: yes   ->   X-Test: a   +   X-Injected: yes
```

and the second was forwarded to the backend. No HTTP client will send this,
which is why it survived: it is only reachable from a raw socket.

**Honest scope.** This is not a demonstrated exploit against the current
topology, and the CHANGELOG should not imply otherwise. Ingress stripping runs
*after* the split, so proxy-owned headers (`X-Forwarded-For`, `X-Auth-Claims`)
are still removed; and m6-http re-serialises with CRLF, so a cache node
forwarding to the origin cannot desync with itself. The hazard is the classic
one from RFC 9112 11.2 — two hops disagreeing where a header ends — and it goes
live the moment anything is placed in front of m6. Current hardened servers
reject it, so m6 now does too: one scan of the already-parsed header block,
rejecting any LF not preceded by CR. The body is excluded, so legitimate LF
bytes in a payload are unaffected.

### The suite that found it

`m6-http/tests/robustness.rs` — 17 tests over raw TLS sockets against a real
`m6-http`, covering the gaps left by `security_regressions.rs` (function level)
and `redirect.rs` (the `:80` listener):

- **Framing/smuggling**: conflicting `Content-Length`, `CL` + `Transfer-Encoding`,
  malformed lengths (negative, non-numeric, overflowing), space before the
  header colon.
- **Injection**: CR/LF/NUL in the request target, in header values, and in
  `Host`; illegal header names; response-splitting assertions on every case.
- **Bounds**: a 128 KB header value, 5,000 headers, a 64 KB request target.
- **Incomplete input**: connect-and-send-nothing, a dribbled and abandoned
  request, a declared body that never arrives, and eight concurrent half-open
  connections which must not delay a normal request (the slowloris class the
  old Python `:80` shim was vulnerable to).
- **Malformed request lines**: ten shapes including absolute-form, CONNECT and
  raw control bytes.
- **Application injection**: SQLi/XSS/template/JNDI-shaped queries, overlong
  UTF-8, double-encoding, and six path-traversal encodings.

The tests assert properties rather than status codes — no smuggling, no
injected header in the response head, no hang and no crash, with a
known-good request after every case to prove the server is still healthy.
A deliberate diagnostic test prints what the server actually returns, so a
suite of vacuous passes (every reply empty) cannot masquerade as coverage;
it shows 400 for every framing abuse, 404 for traversal, and 405 for CONNECT.

### Also observed, not yet changed

An empty `Host:` header is answered 200. RFC 9112 3.2 requires 400 for a
missing or invalid `Host` in HTTP/1.1, and an empty value is arguably invalid.
Left alone for now because the redirect path validates the host separately and
nothing reflects it; recorded rather than silently passed over.

544 workspace tests pass, zero warnings; the robustness suite is 17/17 serially.

---

## 2026-09-06 — `Last-Modified` on rendered HTML

The last of the three caching defects from the owner's audit. m6-html emitted an
`ETag`, so `If-None-Match` validated correctly, but there was no `Last-Modified`
at all — so a client or tool validating by date got the entire page back every
time. `/` is 52 KB, which is exactly the figure the audit reported.

**The hard part was deciding what value is truthful, not where to put it.** A
rendered page has no file of its own to stat. It does have inputs, and their
mtimes are an honest answer:

- **Templates, pooled.** They include each other — `_head.html`, `_banner.html`
  and `_footer.html` are on every page — so the newest template dates every
  route. Resolving the transitive include set per route would be a lot of
  machinery to make one date slightly tighter, and pooling errs toward
  revalidating, which costs a request rather than serving something stale.
- **Params, per route.** This is where precision pays: editing a publication
  should not make `/capabilities` look modified. Visible in the live output —
  `/` reads its own `timeline.json` mtime while the other pages read the
  template floor.

Computed once when the framework state is built and recomputed on reload, so it
costs nothing per request. Emitted only on a success: a validator on a 404 or a
500 invites a client to revalidate an error as though it were content.

**Omitted rather than guessed** in the two cases where no honest value exists: a
code route, whose handler decides at request time, and a route whose params path
carries a `{placeholder}`, which resolves per request. RFC 9110 permits omitting
the header, and a wrong date is far worse than an absent one.

No change was needed in m6-http: `cache::is_not_modified` already checked
`If-Modified-Since` against a cached entry's `Last-Modified`. It simply never
had one to compare against for rendered HTML.

The directory walk is depth-bounded (8) so a symlink loop under the site
directory cannot hang startup, and that bound is tested.

544 workspace tests pass, zero warnings. Verified locally: header present on
every page, `If-Modified-Since` with the exact value returns 304, with an epoch
date returns 200 and the full 16,678 bytes.

**Deploy note:** m6-render is a path dependency of **four** binaries — m6-html,
render-contact, render-analytics and render-cms. `m6-html` was covered by
neither deploy script (not `deploy-platform.sh`, and not `deploy.sh --binary`,
whose loop is only the three site-repo renderers). That is the same multi-binary
trap that took `/contact` down previously; `deploy-platform.sh` now builds,
installs and restarts m6-html alongside m6-http and m6-file.

---

## 2026-09-06 — Method validation and HEAD framing

Two coupled defects from the owner's audit, fixed together because fixing
either alone is worse than fixing neither.

### Every method was served the cached page

Verified live before the fix, warm and cold, on both protocols:

```
GET/HEAD/POST/PUT/DELETE/PATCH/OPTIONS/TRACE/FOO  ->  200, 54,361 bytes
```

Including TRACE, and including an entirely invented `FOO`. `cacheable` was
derived from the route's auth requirement alone; `make_lookup_key` had no method
component; and nothing downstream inspected the method either, so m6-html
rendered whatever it was handed. `m6-file` returned 405 for non-GET/HEAD of its
own accord, which is why only HTML routes were affected and why spot-checking
an asset always looked fine.

Now: `[server].allowed_methods` (default GET, HEAD, POST) is checked before
routing, cache lookup or backend dispatch, and anything else gets 405 with an
`Allow` header. POST is admitted because the contact form needs it, and is
never cached. Configurable because the disabled CMS routes need PUT and DELETE
when they return.

**Deviation from the audit, stated deliberately.** It recommended including the
method in the cache key. That would touch 30+ call sites and would stop HEAD
sharing GET's entry — the sharing that makes HEAD cheap, and the shape the audit
itself preferred. The same property is obtained by construction instead:
`method_may_read_cache` admits GET and HEAD, `method_may_write_cache` admits
GET alone, so the key namespace can only ever hold GET representations and no
unsafe method can read or write one. Pinned by tests rather than left implicit
in a string encoding.

### HEAD responses were malformed, not merely bodied

```
HEAD /capabilities -> 200, Content-Length: 16738, and 16,738 body bytes
```

Not just oversized: the response advertised the GET representation's length and
then mis-terminated, so curl reported *"transfer closed with N bytes
remaining"* (exit 18) and HTTP/2 aborted the stream with `INTERNAL_ERROR`.
Health checks, link validators, crawlers and uptime monitors all use HEAD, so
every one of them was either transferring the whole page or erroring.

`curl -I` hid all of it — it parses the response and discards the body, so every
hand check looked clean. Only a raw socket read shows it. That is why the new
tests assert on raw bytes.

Fixed at all four serialisation points (h1 `build_response`, h2 sync and async
dispatch, h3 `send_h3_response`): `Content-Length` still describes what a GET
would have returned, and no body is sent.

### Why the order mattered

While HEAD wrongly returned a full body, the entry it stored was byte-identical
to a GET entry, so the method-less key was harmless. Fixing the HEAD body alone
would have stored a *bodyless* response under the key a later GET reads —
turning a protocol violation into silent content loss. The write gate lands in
the same commit.

Verified against a local stack, on a raw socket, in the sequence that would
expose it: cold GET full body; warm HEAD zero body with the correct
Content-Length; warm GET still the full body; and separately a **cold HEAD
followed by a GET**, which returns the full page rather than an empty one.

539 workspace tests pass, zero warnings. `curl -I` now exits 0 on both
protocols; POST still reaches the contact backend; all pages 200.

---

## 2026-09-06 — `Link: rel="describedby"` on HTML responses

New `[site].describedby` setting. When set (the site uses `/llms.txt`), every
**HTML** response carries `Link: </llms.txt>; rel="describedby"`.

The site already emits the equivalent `<link rel="describedby">` in `<head>`.
The header is the better of the two and there is no reason to have only one:
it arrives before the page is parsed, so anything inspecting response headers
on its first request discovers the site's machine-facing layer without reading
any markup. The markup link covers readers that only see the document.

HTML only. `llms.txt` describes the *site*; a stylesheet or a PNG claiming to
be described by it says nothing useful and would add a header to the majority
of requests, which are assets.

Applied at the same five places as `Vary`: the two miss-path wrappers and the
three cache-hit replay sites. Empty by default, so a deployment with no such
file does not advertise one.

Two hazards, both covered by tests. A cache node's backend is the origin, which
already added this header before the response was forwarded and cached, so the
node would emit two without a dedupe. And `Link` is also carrying the preload
hints — the dedupe matches on `rel="describedby"` specifically, so the preloads
survive.

529 workspace tests pass, zero warnings on both platforms. Verified locally on
HTML for cache MISS and HIT, and verified absent on CSS and on llms.txt itself.

---

## 2026-09-06 — HTTP caching correctness

Three defects in the revalidation and caching headers, all raised from a live
audit of mgrosvenor.com, then reproduced and root-caused here. None is
cosmetic: each costs bandwidth or risks a downstream cache serving the wrong
bytes.

### `Vary: Accept-Encoding` was emitted only when replaying a cache hit

Same URL, back to back:

```
req1 (MISS) -> vary: <absent>
req2 (HIT)  -> vary: Accept-Encoding
```

This is the worse of the two orderings. The first client to request any URL is
the one that gets the wrong headers, and that is every fresh visitor, plus
every downstream shared cache at the moment it populates itself.

The header was never being stripped on the forward path, as first assumed. It
was simply only ever added on the hit path. Both backends were affected, at
both tiers: m6-file emits no `Vary` of its own, and neither does m6-render.

Fixed by applying it once in a thin `handle_request` wrapper covering all four
server paths and all eleven of the inner function's `Ready` returns, plus a
matching wrapper on `finalize_url_response` for the async URL-backend
completion path, which bypasses the first one entirely.

**`set_vary_accept_encoding` now merges rather than overwrites, and that is
load-bearing.** It used to drop every existing `Vary` and write
`Accept-Encoding` in its place. That was harmless only while it ran solely on
the cache-hit path, where `should_cache` had already refused anything varying
on more than encoding. Called on the miss path, an overwriting version would
rewrite a backend's `Vary: Cookie` to `Vary: Accept-Encoding`, `should_cache`
would then see a cacheable response, and one client's private variant would be
stored and replayed to everyone. Preserving the other field names is what keeps
such a response uncacheable. `Vary: *` is passed through untouched.

Seven tests, including the merge case asserted end to end against
`should_cache`.

### All content codings shared one strong ETag

```
br        etag="6a9bdf1d-140ca"   7,246 bytes
gzip      etag="6a9bdf1d-140ca"   7,576 bytes
identity  etag="6a9bdf1d-140ca"  45,355 bytes
```

Three representations, three different byte strings, one validator asserting
they are the same thing. RFC 9110 requires a strong validator to identify the
representation actually sent. Combined with the missing `Vary` above, a
downstream shared cache holding the brotli entry could legitimately match that
tag against a gzip-only client and hand back a body it cannot decode.

The coding is now folded into the tag (`-br` / `-gz`; identity keeps the
historical unsuffixed form so already-cached URLs do not spuriously miss).
Content negotiation moved above the conditional check to make this possible —
it reads the path and request headers only, never the file, so a 304 still
skips the read, minify and compress entirely.

Four tests, including the pairing that matters: a brotli ETag validates a
brotli request (304) and does **not** validate a gzip one (200).

### A failed compression was served mislabelled

Found while fixing the above, in the same lines.
`compress_brotli(&data, lvl).unwrap_or(data)` discarded the error and fell back
to the uncompressed bytes **while still setting `Content-Encoding: br`** — a
body no client could decode, announced as one it could. Silent, with no log
line.

Another instance of the pattern worth naming across this codebase: m6 fails
open in several places (minification, compression negotiation, cache refresh).
Each is the right behaviour and each is invisible when it triggers. The
fallback now drops the label, strips the coding suffix from the ETag, and warns.

### Verified

524 workspace tests pass, zero warnings. Both fixes measured end to end against
a local dev stack before deploying: `Vary` present on MISS and HIT for both
backends, ETags distinct per coding, negotiation sizes unchanged
(br 7,372 / gzip 7,694 / identity 45,792), conditional GET returning 304 for
the matching coding and 200 for a mismatched one, and all pages still 200.

### Still open from the same audit

- Rendered HTML carries no `Last-Modified`, so `If-Modified-Since` never
  validates and returns the full page. Strictly RFC-legal with no modification
  date available, but it costs a real transfer. Needs a truthful value derived
  from the render inputs, not a fabricated one.
- Method confusion and HEAD framing. Must be fixed together: today a HEAD
  wrongly returns a full body, so the entry it stores is byte-identical to a
  GET entry and the method-less cache key is harmless. Fixing the HEAD body
  alone would store a bodyless response under the shared key and serve the next
  GET an empty page.
