# m6 release notes

Newest first. One entry per change that reaches a running node.

Each entry records what changed, why it mattered, and how it was verified.
"Verified" means measured against a running server, not inferred from the
source: several defects in this list were invisible in the code and only showed
up against the deployed artefact.

Deploy order is fixed: **test locally, commit, then deploy.** Never the reverse.

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
