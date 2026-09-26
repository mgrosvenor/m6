# Laying out a site: one root per app

An m6 site is usually several processes: m6-http routing, m6-file serving static
files, m6-html rendering pages, and whatever custom renderers the site needs. Each of
them is started with a directory.

This document is about which directory, because the obvious answer — give them all the
site root — is the wrong one, and the reason is worth understanding before you build
the tree.

## The layout

```
<site>/                 m6-http's root: the whole site
  site.toml             the route table
  configs/              each app's config, passed by absolute path
  apps/
    html/               templates/ data/                 <- m6-html's root
    file/               assets/ static/ .well-known/     <- m6-file's root
    contact/            keys/                            <- a renderer's root
    <app>/              whatever that app reads          <- one root per app
  bin/                  custom renderer binaries
```

Each app is started with its own directory under `apps/` and can reach nothing else.

## Why not one root for everything

Because a root is a boundary, and sharing one throws the boundary away.

Take a site with a contact form. The renderer behind it needs a mail relay password, so
the password is a file somewhere in the tree. Now consider m6-file, which serves static
assets and whose routes are narrowly scoped to `assets/` and `static/`.

If both are rooted at the site root, then the only thing standing between the file
service and the mail password is that no route in `m6-file.conf` happens to point at
it. That is a real defence and it is tested — m6-file refuses a path that traverses a
symlink out of its root, and its test suite covers the traversal cases — but it is
**confinement by configuration**. Four `root` values in a config file are what keeps
the password unreachable, and nothing structural does.

Give each app its own root and the question stops being asked. The file service cannot
serve the password because the password is not under its root, and it cannot read it
either. A path-handling regression has nothing to find.

The same argument applies in every direction: the HTML renderer has no business reading
the file service's assets, and the file service has no business reading templates.
Neither needs the other's material, so neither should be able to see it.

### It costs nothing in configuration

Each app's config paths are already relative to that app's own root:

```toml
# m6-file.conf        root = "assets/"
# m6-html.conf        template = "templates/experience.html"
# render-contact.conf secrets_file = "keys/relay-secrets.toml"
```

So moving from one shared root to one root per app changes **no app config at all**.
That is a useful sign you have the boundary in the right place: if splitting the roots
forces you to rewrite paths, the paths were encoding the shared root rather than the
app's own.

### Enforce it in the supervisor as well

Systemd can make the separation structural rather than conventional:

```ini
# the file service
ExecStart=/usr/local/bin/m6-file /srv/site/apps/file /srv/site/configs/m6-file.conf
ReadOnlyPaths=/srv/site
InaccessiblePaths=/srv/site/apps/contact /srv/site/apps/html
```

The root argument means it cannot *serve* another app's files. `InaccessiblePaths`
means it cannot *read* them.

## Why m6-http gets the top

m6-http serves nothing from disk. It reads `site.toml`, globs filenames to work out
which routes exist, and forwards every request to a backend over a unix socket. It is
the one process that legitimately needs to see the whole tree, and giving it a narrow
root would only mean symlinking things into view.

## The two traps

### 1. A route_group's glob and the file it names are resolved by different processes

`[[route_group]]` lives in `site.toml`, so its glob is relative to **m6-http's** root.
The file it eventually names is resolved by **m6-file**, relative to m6-file's root and
that route's own `root`. In this layout those are different directories, and the glob
has to say so:

```toml
# site.toml — the glob is relative to m6-http's root, which is the site root
[[route_group]]
glob    = "apps/file/assets/**/*"
path    = "/assets/{relpath}"
backend = "m6-file"
```

```toml
# m6-file.conf — root is relative to m6-file's root, which is apps/file
[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

Get the glob wrong and the routes never exist. Get `root` wrong and the routes exist
while every request 404s. Neither log says which. See [`m6-file.md`](m6-file.md).

**If you ever find yourself symlinking one app's directory into another's so a glob can
see it, that is this trap.** The symlink will work, and its failure mode is silent: a
reload with the link missing expands the glob against an empty directory, so every
asset route disappears and the reload reports success.

### 2. Prefer a route_group to a wildcard for static assets

A `[[route_group]]` is expanded at config load into one **concrete** route per matched
file. A wildcard route is one route with a parameter. That difference decides whether
the cache can be warmed:

`is_warmable` skips any route whose path contains `{` — its own comment names
`/assets/{*relpath}` as the example — because a pattern is not a URL and there is
nothing to fetch. So a wildcard silently means static assets are never pre-warmed.

On a single node that costs one slow first request per file. On a deployment with edge
nodes it costs a round trip to the origin per file per edge, paid by whichever visitor
arrives first after a restart.

Use a wildcard where enumeration is impossible — a directory whose contents appear at
runtime, such as ACME challenge tokens — and a `route_group` everywhere else. A
short-lived path is exactly the one you would not want warmed anyway.

## Building the tree

The layout is an output, not something maintained by hand. Build it from your sources
into an output directory and deploy that directory, the way a Makefile produces a
build: then a node holds a built site rather than a checkout, and nothing on it is a
source file.

The practical test is whether deploying means copying one directory. If it means
copying a repository and then rearranging it in place — creating directories, moving
things, symlinking — the layout is being assembled on the target, and every step of
that is a step that can half-fail on a live box.

## See also

- [`m6-file.md`](m6-file.md) — the static file service and its own route table
- [`m6-site-toml.md`](m6-site-toml.md) — m6-http's route table
- [`m6-app-anatomy.md`](m6-app-anatomy.md) — writing an app of your own
- [`m6-user-guide.md`](m6-user-guide.md) — the worked examples, which build this
  layout up one piece at a time
