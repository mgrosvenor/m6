# m6-file

m6-file serves files from disk.
It is an *App* (a service built on m6-core's application framework) with one handler, registered under the name `files`.
Every route it answers comes from its own config, not from m6-http's.
Until now that was readable only in the source.
This document covers its two route tables, how it resolves a path, what its handler owns, and how to configure it.

## Contents

1. [The shape of a request](#1-the-shape-of-a-request)
2. [Two route tables](#2-two-route-tables)
3. [How a path is resolved](#3-how-a-path-is-resolved)
4. [What the handler owns](#4-what-the-handler-owns)
5. [Refusing to serve outside its root](#5-refusing-to-serve-outside-its-root)
6. [Configuration](#6-configuration)
7. [Summary](#7-summary)

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

**Table 1: the two route tables serving one static file request.** Each config answers a different question, and neither process reads the other's table.

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

**Table 2: the three inputs to a resolved filesystem path.** None of the three is the request URL.

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

Table 3 gives the header for each case. Take from it that a versioned request is pinned for a year and everything else is not.

| request | header |
|---|---|
| `?v=<hash>` present | `public, max-age=31536000, immutable` |
| anything else | `public, max-age=60, s-maxage=86400, stale-while-revalidate=60` |

**Table 3: `Cache-Control` by request, decided from the query string alone.** A versioned request is pinned for a year. Everything else is held for 60 seconds by browsers and 86400 seconds by shared caches.

A `?v=` URL addresses one exact version, because changed bytes mean a changed hash and so a different URL.
That is what makes a year and `immutable` safe, and it also stops a browser revalidating on reload.

For everything else the two audiences are split deliberately:

- `max-age` and `stale-while-revalidate` are honoured by browsers, and no invalidation can reach a browser cache, so they stay short and a deploy is visible promptly.
- `s-maxage` is honoured only by shared caches, per *RFC* (Request for Comments) 9110 section 5.2.2.10, so it lengthens just the edge copy, which an invalidation can evict.

No route can currently override this, and three things that look like they would do not work:

Table 4 lists the three ways to override the header and why each fails. Take from it that no route input reaches the decision.

| attempt | result |
|---|---|
| `cache` on the m6-file route | not read for this |
| `cache` on a `[[route_group]]` in `site.toml` | ignored, with a warning |
| `headers` on the route | appended, so two `Cache-Control` headers |

**Table 4: three ways to override `Cache-Control` per route, none of which work.** The handler computes the header itself, and no route input reaches that decision.

That gap matters for anything short-lived served from disk, such as a one-time download or an *ACME* (Automatic Certificate Management Environment) challenge token.
It is tracked as issue #126.

So the handler decides the bytes, the encoding, the validator and the caching, and a route decides none of them.
What a route does bound is which files can be reached at all, which is the next section.

## 5. Refusing to serve outside its root

m6-file returns 404 for a path that is, or traverses, a symbolic link resolving outside its root.
The check pays for canonicalisation only when a link is present, so ordinary files are not slowed.
A path that cannot be resolved at all counts as escaping, because it would return 404 either way.

This is a backstop, not the boundary.
The boundary is the root the process was started with, so that directory should hold only what this service serves.
[`m6-site-layout.md`](m6-site-layout.md) covers how to arrange that.

## 6. Configuration

m6-file accepts two arguments, a root and a config path, and reads its thread pool size and routes from the config.

```toml
[thread_pool]
size = 32

[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

Three facts about that config:

- the pool defaults to the *CPU* (Central Processing Unit) count. A page requesting more assets at once than the pool has threads queues the surplus, which surfaces as "pool empty" backend errors.
- a route naming a handler the binary does not have is fatal, so the service exits 2 at startup and a reload is refused with the previous routes left serving.
- routes are rebuilt on every config reload, so an asset tree can be added without a restart.

m6-file answers `GET` and `HEAD`, and returns 405 for anything else.

Those two arguments and this one config are the whole interface.
The summary below collects what they imply.

## 7. Summary

m6-file is a small service with one handler and its own route table.
The table is the part to remember, because a static file needs a route in `site.toml` to reach m6-file and a route in `m6-file.conf` to reach the disk.
Its handler owns the whole response, including the ETag and `Cache-Control`, which no route can currently override.
Its root bounds what it can serve, which is why the layout document treats that root as a security boundary rather than a convenience.

Related reading:

| document | covers |
|---|---|
| [`m6-site-layout.md`](m6-site-layout.md) | where m6-file's root belongs relative to other apps |
| [`m6-site-toml.md`](m6-site-toml.md) | m6-http's route table, including `[[route_group]]` |
| [`m6-app-anatomy.md`](m6-app-anatomy.md) | writing an App of your own |

**Table 5: further reading, and what each document covers.**
