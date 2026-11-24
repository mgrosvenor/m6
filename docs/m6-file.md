# m6-file

m6-file serves files from disk.
It is an m6 service built on `m6-core`: one process, one event loop, and one handler registered under the name `files`.
Every route it answers comes from its own config, not from m6-http's.
Until now that was readable only in the source.
This document covers its two route tables, how it resolves a path, what its handler owns, and how to configure it.

## Contents

1. [The shape of a request](#1-the-shape-of-a-request)
2. [Two route tables](#2-two-route-tables)
3. [How a path is resolved](#3-how-a-path-is-resolved)
4. [What the handler owns](#4-what-the-handler-owns)
5. [Refusing to serve outside its root](#5-refusing-to-serve-outside-its-root)
6. [What a served root may carry](#6-what-a-served-root-may-carry)
7. [Configuration](#7-configuration)
8. [Summary](#8-summary)

## 1. The shape of a request

A static file request passes through two processes, each with its own route table.
m6-http decides which backend gets the request.
m6-file then decides which file to return.
Neither reads the other's table, which is why a file needs an entry in both.

Figure 1 shows the two hops a request makes and which config governs each. Take from it that one file needs a route in both.

```
client ──► m6-http ──► m6-file ──► disk
           site.toml   m6-file.conf
           which       which
           backend     file
```

**Figure 1: a static file request crosses two processes.** m6-http matches `site.toml` to pick a backend. m6-file then matches `m6-file.conf` to pick a file.

The rest of this document is about that split and its consequences.

## 2. Two route tables

A static file needs a route in both configs, and each config answers a different question.
Omit the m6-http half and nothing is forwarded.
Omit the m6-file half and every request returns 404, with neither log saying which half is wrong.

Table 1 names the two configs and what each decides. Take from it that omitting either one breaks the request.

| config | whose | decides |
|---|---|---|
| `site.toml` | m6-http | which backend receives the request |
| `m6-file.conf` | m6-file | which file is returned |

**Table 1: the two route tables serving one static file request.**
Each config answers a different question, and neither process reads the other's table.

A minimal pair looks like this.

```toml
# site.toml
[[route]]
path    = "/assets/{relpath}"
backend = "m6-file"
```

```toml
# m6-file.conf
[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

With both in place, the request reaches m6-file and m6-file knows what to open.

## 3. How a path is resolved

m6-file builds a filesystem path from three parts, and none of them is the *URL* (Uniform Resource Locator) directly.

Table 2 lists the three parts of a resolved path and where each comes from. Take from it that the request URL is not one of them.

| part | comes from |
|---|---|
| m6-file's own root | the first argument the process was started with |
| the route's `root` | `m6-file.conf` |
| `relpath` or `filename` | the matched route parameter |

**Table 2: the three inputs to a resolved filesystem path.**
None of the three is the request URL.

The three are joined in that order.

```
<m6-file's root> / <route's root> / <relpath>
```

A route can therefore map any URL to any directory, which is why `root` should name the narrowest directory that works.
It is the only thing bounding what that route can reach.

Two further points:

- `root` may contain `{param}` placeholders, substituted from the request.
- `root` may name a single file instead of a directory, as in `root = "static/robots.txt"`.

Those three parts fix the file, but only if the route parameter spans the whole remaining path.
The next subsection covers when it does.

### 3.1. The wildcard is explicit

`{*relpath}` spans more than one path segment.
A bare trailing `{relpath}` does not, because m6-core does not make the last parameter implicitly greedy.
Making it greedy would silently change the meaning of every route already written.

This matters in practice.
With a non-greedy matcher, `/assets/style.css` keeps serving while `/assets/css/style.css` returns 404.
A check that fetches only a top-level file therefore passes while the site is broken, so test a path at least two segments deep.

Path resolution ends there, with the file chosen.
What m6-file then does with that file is the subject of the next section.

## 4. What the handler owns

m6-file builds its own representation of a response.
It negotiates the content coding, compresses, and constructs an *ETag* (Entity Tag, a cache validator) naming the result.
Every response it returns is final, and m6-core's pipeline leaves it alone.
Letting m6-core compress afterwards would put Brotli bytes on the wire under a tag asserting identity encoding.

Two consequences follow:

- the ETag covers the encoding, not just modification time and size, so Brotli, gzip and identity copies of one file do not share a tag.
- negotiation happens before the ETag is built, in that order and deliberately.

### 4.1. Cache-Control is computed from the query string

m6-file sets `Cache-Control` itself, from the query string alone.

Table 3 gives the header for each case. Take from it that a versioned request is pinned for a year and everything else revalidates on every browser request.

| request | header |
|---|---|
| `?v=<hash>` present | `public, max-age=31536000, immutable` |
| anything else | `public, max-age=0, must-revalidate, s-maxage=86400` |

**Table 3: `Cache-Control` by request, decided from the query string alone.**
A versioned request is pinned for a year.
Everything else is stale in the browser at once and held for 86400 seconds by shared caches.

A `?v=` URL addresses one exact version, because changed bytes mean a changed hash and so a different URL.
That is what makes a year and `immutable` safe, and it also stops a browser revalidating on reload.

For everything else the two audiences are split deliberately:

- `max-age=0` with `must-revalidate` is honoured by browsers and makes the browser validate on every use (RFC 9111 sections 5.2.2.1 and 5.2.2.2). An unchanged file costs one 304, and a deploy is visible on the next request.
- `s-maxage=86400` is honoured only by shared caches and overrides `max-age` for them (RFC 9111 section 5.2.2.10), so it lengthens just the edge copy, which an invalidation can evict.

The handler sends an `ETag` on every file, so that browser validation is a 304 with no body.
The edge also answers the conditional request from its own copy (RFC 9111 section 4.3.2), without reading the file or contacting m6-file.

A route overrides this by naming `cache`, as nginx does per location.
The route's value replaces the header the handler set, and the reply carries one `Cache-Control`.

Table 4 lists where a route can say so and what each does.
Take from it that `cache` is the one input, and that `headers` does not replace.

| where | result |
|---|---|
| `cache` on the m6-file route in `m6-file.conf` | replaces the handler's value |
| `cache` on a `[[route]]` or `[[route_group]]` in `site.toml` | replaces it on the edge, before the edge stores the reply |
| `headers` on the route | appended, so two `Cache-Control` headers. Use `cache` |

**Table 4: where a route sets `Cache-Control`.**
Without `cache` the handler's value stands.

An ACME challenge path, a one-time download or a health file takes `cache = "no-store"` on its route.
Set it in both configs when the file is served through the edge: the edge stops storing the reply, and m6-file stops advertising a day to anything else in front of it.

So the handler decides the bytes, the encoding and the validator, and a route can override the caching.
What a route does bound is which files can be reached at all, which is the next section.

### 4.2. Tail mode

A route with `tail = true` serves a file from a byte offset, which is how a browser follows a growing log.
`GET <path>?offset=N` returns the bytes from `N` to the end of the file, and `X-Log-End` names the byte where the reply ended, so the client asks next from there.
`GET <path>?n=N` with no offset returns the last `N` lines, and `X-Log-End` is the end of the file.
The reply is `Cache-Control: no-store`.

One reply reads at most one page, which is `[limits.tail_read] max`, 512 KiB by default (`m6-app-anatomy.md` §4.2, Table 6).
A client further behind than a page reads the next page from `X-Log-End`, and loses nothing.
A last-`N`-lines request scans back from the end of the file in a window that starts at `[limits.tail_read] start`, 64 KiB, and doubles until it holds `N` lines.
When `N` lines do not fit in one page the reply is 413, and its body and the log line name `[limits.tail_read] max` and the file to raise it in.
The limit is a top-level `[limits.tail_read]` table in `m6-file.conf`.

## 5. Refusing to serve outside its root

m6-file returns 404 for a path that is, or traverses, a symbolic link resolving outside its root.
The check pays for canonicalisation only when a link is present, so ordinary files are not slowed.
A path that cannot be resolved at all counts as escaping, because it would return 404 either way.

This is a backstop, not the boundary.
The boundary is the root the process was started with, so that directory should hold only what this service serves.
[`m6-site-layout.md`](m6-site-layout.md) covers how to arrange that.

## 6. What a served root may carry

m6-file walks every route's `root` once at start and reports what is there.
The walk happens before the socket is bound, so a refused start leaves no listener behind, and a request pays nothing for it.
It follows the routes, so a renderer's `target/` beside the app root is not examined when no route reaches it.

Table 5 gives the names and the answer.

| entry | answer | why |
|---|---|---|
| `.git` (file or directory) | exit 2 | History, hooks and config are never a served asset |
| `target/`, `node_modules/` | exit 2 | A build tree or a dependency tree is never a served asset |
| `logs/` | warning, then serve | A log viewer roots a route there on purpose, and a wildcard route over a wider root would publish it by accident |
| `*~`, `*.swp`, `*.swo`, `*.rs.bk`, `.#*`, `.DS_Store` | warning, then serve | An editor backup or OS metadata leaks source and is never a served asset |

**Table 5: what a served root may carry.**
A version-control directory and a build tree stop the start, while a log directory and an editor backup are named and served.

A root that names one file, such as `root = "static/robots.txt"`, makes that file served on purpose, so its name is never a finding.
A root holding a `{param}` is cut at the first placeholder, because the tree up to that point is the most the route can reach without a request.
An absolute root and a root that climbs out with `..` are skipped here, and the handler's `resolve_within` refuses them per request.

### 6.1. The size budget

`site_size_budget_mb` is a top-level key in m6-file's config.
The walk compares the budget with the regular-file bytes every route's root can reach.
Overlapping roots are counted once, symlinks are not followed, and a file a route names outright counts as one file.

The budget counts bytes, not files.
Bytes are what fills the disk and what a deploy transfers, and a file count would refuse a legitimate tree of many small thumbnails while one huge video passed.
The warning also prints the file count so an operator can see a tree that is large in inodes as well as bytes.

The default is 128 MiB.
The largest tree in m6's own examples is 5.1 MB, so the default leaves two orders of magnitude of room for a legitimate asset tree while still catching a `target/` or a log file that has grown for a year.
A deployment that serves more raises the key.

The over-budget line names the total, the file count and the setting:

```text
m6-file: the document root /srv/site holds 131.0 MiB in 1204 files, over site_size_budget_mb = 128: raise site_size_budget_mb in the config, or remove files that should not be deployed
```

The budget is a warning.
Content changes without a config edit, and a legitimate large asset must not stop a serving node at 02:00.
A mistake at start is an error when the mistake is a name that can never be intentional, which is why Table 5 splits the two answers.

## 7. Configuration

m6-file accepts two arguments, a root and a config path, serves from one event loop, and reads its routes from the config.

```toml
site_size_budget_mb = 128

[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

Three facts about that config:

- a route naming a handler the binary does not have is fatal, so the service exits 2 at startup.
- routes come from config, read once at start, so an asset tree is added by editing the config and restarting.
- `site_size_budget_mb` is a top-level key, so it goes before the first `[[route]]` table. TOML puts a key written after that header inside the route.

The process serves every connection from one loop, so more load is more instances, each with its own socket in `site.toml`'s `[[backend]]` glob.
A `[thread_pool]` table is refused at start for the same reason.

m6-file answers `GET` and `HEAD`, and returns 405 for anything else.

Those two arguments and this one config are the whole interface.
The summary below collects what they imply.

## 8. Summary

m6-file is a small service with one handler and its own route table.
The table is the part to remember, because a static file needs a route in `site.toml` to reach m6-file and a route in `m6-file.conf` to reach the disk.
Its handler owns the whole response, including the ETag and `Cache-Control`, and a route's `cache` replaces that `Cache-Control`.
Its root bounds what it can serve, which is why the layout document treats that root as a security boundary.
Every route's root is walked once at start, and section 6 gives the entries that stop the start and the size budget that warns about a tree grown too large.

Table 6 gives the related reading.

| document | covers |
|---|---|
| [`m6-site-layout.md`](m6-site-layout.md) | where m6-file's root belongs relative to other apps |
| [`m6-site-toml.md`](m6-site-toml.md) | m6-http's route table, including `[[route_group]]` |
| [`m6-app-anatomy.md`](m6-app-anatomy.md) | writing a service of your own |

**Table 6: further reading, and what each document covers.**
