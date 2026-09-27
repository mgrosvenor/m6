# Laying out a site

A site running more than one m6 app must give each app its own root directory.
That is the conclusion of this document, and the reason is that a root is a security boundary rather than a convenience.
We begin by giving the layout.
We then show why one shared root is wrong, why m6-http is the exception, and the two traps that catch people building this for the first time.

## Contents

1. [The layout](#1-the-layout)
2. [Why not one root for everything](#2-why-not-one-root-for-everything)
3. [Why m6-http gets the top](#3-why-m6-http-gets-the-top)
4. [Two traps](#4-two-traps)
5. [Building the tree](#5-building-the-tree)
6. [Summary](#6-summary)

## 1. The layout

Every app gets its own directory under `apps/`, and nothing else.
Figure 1 gives the whole scheme.

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

**Figure 1: one root per app, with m6-http given the site root.** Each app reads only the directory it is started with. `configs/` and `bin/` sit outside every app root.

Three properties follow from that shape:

- each app can reach its own material and no other app's.
- `configs/` sits outside every app root, because each config is passed by absolute path.
- m6-http alone is given the top, for the reason in section 3.

The rest of this document argues for those three.

## 2. Why not one root for everything

One shared root throws away the only boundary you have.
Take a site with a contact form, whose renderer needs a mail relay password.
The password is therefore a file in the tree.
Now add m6-file, which serves static assets and whose routes point only at `assets/` and `static/`.

Root both apps at the site root and one thing keeps the password away from the file server: no route in `m6-file.conf` happens to point at it.
That is confinement by configuration.
m6-file does refuse a path traversing a symbolic link out of its root, and its tests cover the traversal cases, so the defence is real.
It is still four `root` values in a config file, and nothing structural.

Give each app its own root and the question stops being asked.
m6-file cannot serve the password, because the password is not under its root.
m6-file cannot read the password either.
A path-handling regression therefore has nothing to find.

The same argument runs in every direction.
m6-html has no reason to read the file server's assets, and m6-file has no reason to read templates.
Neither app needs the other's material, so neither app should see it.

### 2.1. It costs nothing in configuration

Splitting the roots changes no app config at all.
Each config already names paths relative to its own app's root:

```toml
# m6-file.conf        root = "assets/"
# m6-html.conf        template = "templates/page.html"
# render-contact.conf secrets_file = "keys/relay-secrets.toml"
```

That is the test for whether the boundary is in the right place.
If splitting the roots forces you to rewrite paths, those paths were encoding the shared root rather than the app's own.

### 2.2. Enforce it in the supervisor

Systemd makes the separation structural rather than conventional.

```ini
ExecStart=/usr/local/bin/m6-file /srv/site/apps/file /srv/site/configs/m6-file.conf
ReadOnlyPaths=/srv/site
InaccessiblePaths=/srv/site/apps/contact /srv/site/apps/html
```

The root argument stops m6-file serving another app's files.
`InaccessiblePaths` stops it reading them.
Together they move the boundary out of config and into the process.

## 3. Why m6-http gets the top

m6-http is the one app given the whole site, because it serves nothing from disk.
It reads `site.toml`, globs filenames to work out which routes exist, then forwards every request to a backend over a unix socket.
Seeing the tree is its job.
Giving it a narrow root would only mean symbolic links into view, which section 4 explains is the trap to avoid.

## 4. Two traps

Two mistakes in this layout fail quietly rather than loudly.
The first produces a route that exists over a backend that returns 404.
The second disables cache warming without any error.

### 4.1. A glob and the file it names are resolved by different processes

`[[route_group]]` lives in `site.toml`, so its glob is relative to m6-http's root.
The file it names is resolved by m6-file, relative to m6-file's root and that route's own `root`.
Those are different directories in this layout, so the glob must say so.

```toml
# site.toml: the glob is relative to m6-http's root, the site root
[[route_group]]
glob    = "apps/file/assets/**/*"
path    = "/assets/{relpath}"
backend = "m6-file"
```

```toml
# m6-file.conf: root is relative to m6-file's root, apps/file
[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

Table 1 separates the two failure modes.

| mistake | result |
|---|---|
| wrong glob | the routes never exist |
| wrong `root` | the routes exist and every request returns 404 |

**Table 1: a wrong glob and a wrong `root` fail differently.** A wrong glob removes the route. A wrong `root` keeps the route and empties the response.

Neither log says which.
[`m6-file.md`](m6-file.md) covers the resolution in full.

**If you find yourself adding a symbolic link so a glob can see another app's directory, this is the trap.**
The link works, and its failure mode is silent.
A reload with the link missing expands the glob against an empty directory, so every route disappears and the reload reports success.
Fix the glob instead.

### 4.2. Prefer a route_group to a wildcard for static assets

A `[[route_group]]` expands at config load into one concrete route per matched file.
A wildcard route is one route holding a parameter.
That difference decides whether the cache can be warmed.

`is_warmable` skips any route whose path contains `{`, naming `/assets/{*relpath}` as its example, because a pattern is not a *URL* (Uniform Resource Locator) and there is nothing to fetch.
A wildcard therefore means static assets are never pre-warmed.

Table 2 gives the cost of a wildcard by deployment size.

| deployment | cost of a wildcard |
|---|---|
| one node | one slow first request per file |
| edge nodes | one round trip to the origin per file, per edge, paid by the first visitor after a restart |

**Table 2: the cost of an unwarmed asset, by deployment.** A single node pays once per file. A fleet with edges pays once per file per edge, after every restart.

Use a wildcard only where enumeration is impossible, such as a directory whose contents appear at runtime.
*ACME* (Automatic Certificate Management Environment) challenge tokens are the clear case, and a short-lived path is one you would not want warmed anyway.
Everything else takes a `route_group`.

## 5. Building the tree

Build the layout as an output, never maintain it by hand.
Compile it from your sources into an output directory, then deploy that directory, the way a Makefile produces a build.
A node then holds a built site rather than a checkout, and nothing on it is a source file.

The practical test is what deploying means.
Copying one directory is right.
Copying a repository and then rearranging it in place, creating directories and moving and linking files, means the layout is being assembled on the target.
Every step of that assembly is a step that can half-fail on a live box.

## 6. Summary

A site running more than one m6 app must give each app its own root directory, because a root is a security boundary rather than a convenience.
One shared root leaves a mail relay password one config line away from a static file server, and splitting the roots costs no config change at all.
m6-http is the single exception, because it serves nothing from disk and its job is to see the tree.
Two traps remain: a glob and its file are resolved by different processes, and a wildcard route silently disables cache warming.

Related reading:

| document | covers |
|---|---|
| [`m6-file.md`](m6-file.md) | the static file service and its own route table |
| [`m6-site-toml.md`](m6-site-toml.md) | m6-http's route table |
| [`m6-app-anatomy.md`](m6-app-anatomy.md) | writing an app of your own |
| [`m6-user-guide.md`](m6-user-guide.md) | the worked examples, which build this layout up one piece at a time |

**Table 3: further reading, and what each document covers.**
