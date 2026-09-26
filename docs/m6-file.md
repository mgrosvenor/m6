# m6-file — the static file service

m6-file serves files from disk. It is an `App` service with exactly one handler,
registered under the name `files`, and **every route it answers comes from its own
config**. That last point is the one this document exists for: it is the thing most
easily got wrong, and until now it was readable only in the source.

```toml
# m6-file.conf
[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

## Two route tables, and they do different jobs

A request for a static file passes through two configs, and confusing them produces a
route that exists over a backend that 404s, with nothing in either log saying which
half is wrong.

| config | whose | what it decides |
|---|---|---|
| `site.toml` | m6-http's | **which backend** gets the request |
| `m6-file.conf` | m6-file's | **which file** is returned |

m6-http matches the request against its route table and forwards it to the named
backend over a unix socket. m6-file then matches the request against *its* route table
and resolves a path. Neither knows the other's table.

So a static file needs an entry in **both**:

```toml
# site.toml — m6-http: send this path to the file service
[[route]]
path    = "/assets/{relpath}"
backend = "m6-file"

# m6-file.conf — m6-file: and here is the file it means
[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

Add only the first and every request 404s. Add only the second and m6-http never
forwards anything.

## How a file path is resolved

`resolve_fs_path` joins three things:

1. **m6-file's own root**, the first positional argument the process was started with.
2. **the route's `root`**, from `m6-file.conf`.
3. **the matched `relpath` or `filename`** parameter.

```
<m6-file's root> / <route's root> / <relpath>
```

Nothing in that comes from the URL directly. A route can map any URL to any directory,
which is what makes `root` worth setting narrowly: it is the only thing bounding what
that route can reach.

`root` may also contain `{param}` placeholders, which are substituted from the request
dict, and it may name a single file rather than a directory:

```toml
[[route]]
path    = "/robots.txt"
handler = "files"
root    = "static/robots.txt"
```

### The wildcard is explicit

`{*relpath}` spans several path segments. A bare trailing `{relpath}` does **not** —
core does not make the last parameter implicitly greedy, because that would silently
change the meaning of every route already written. A route that needs to span segments
says so.

This is a real failure and not a hypothetical one: with a non-greedy matcher,
`/assets/style.css` keeps serving while `/assets/css/style.css` returns 404, so a
check that only fetches a top-level file passes while the site is broken. Test a path
at least two segments deep.

## What the handler owns

**m6-file builds its own representation.** It negotiates the content coding,
compresses, and constructs an ETag naming the result, so every response it returns is
final and core's pipeline leaves it alone. Letting core compress afterwards would put
brotli bytes on the wire under a tag asserting identity.

Consequences worth knowing:

- the ETag covers the **encoding**, not just mtime and size. Without that, brotli,
  gzip and identity of one file share a tag, and a cache can hand a client bytes in a
  coding it did not ask for.
- negotiation happens before the ETag is built, in that order and deliberately.

### Cache-Control is computed from the query string

| request | header |
|---|---|
| `?v=<hash>` present | `public, max-age=31536000, immutable` |
| anything else | `public, max-age=60, s-maxage=86400, stale-while-revalidate=60` |

A `?v=` URL addresses one exact version — changed bytes mean a changed hash and so a
different URL — which is what makes a year and `immutable` safe, and also stops a
browser revalidating on reload.

For everything else the two audiences are split on purpose. `max-age` and
`stale-while-revalidate` are honoured by **browsers**, and no invalidation can reach a
browser cache, so they stay short and a deploy is visible promptly. `s-maxage` is
honoured only by **shared caches** (RFC 9110 5.2.2.10), so it lengthens just the edge's
copy, which an invalidation can evict.

**There is currently no way for a route to override this.** `cache` on the route is not
read for it, `cache` on a `[[route_group]]` in `site.toml` is ignored with a warning,
and `headers` is appended rather than substituted, so it yields two `Cache-Control`
headers. That matters for anything short-lived served from disk — a one-time download,
an ACME HTTP-01 challenge token. See issue #126.

## It refuses to serve outside its root

A path that is, or traverses, a symlink resolving outside m6-file's root returns 404.
The check only pays for `canonicalize` when a symlink is actually present, so ordinary
files are not slowed by it, and a path that cannot be resolved at all is treated as
escaping because it would 404 either way.

That is a backstop, not the boundary. **The boundary is the root the process is started
with**, which is why it is worth making that directory hold only what this service
serves. See [`m6-site-layout.md`](m6-site-layout.md).

## Methods

`GET` and `HEAD`. Anything else returns 405.

## Configuration

```toml
[thread_pool]
# Defaults to the CPU count, which a page firing dozens of concurrent image
# requests exhausts easily; each queued request surfaces as a "pool empty"
# backend error and a slow or broken image.
size = 32

[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

A route naming a handler the binary does not have is **fatal**: the service exits 2 at
startup, and a reload is refused with the previous routes left serving.

Routes are rebuilt on every config reload, so an asset tree can be added without
restarting the service.

## See also

- [`m6-site-layout.md`](m6-site-layout.md) — where m6-file's root belongs relative to
  the other apps, and why
- [`m6-site-toml.md`](m6-site-toml.md) — m6-http's route table, including
  `[[route_group]]`
- [`m6-app-anatomy.md`](m6-app-anatomy.md) — writing an `App` service of your own
