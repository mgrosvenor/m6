# m6 — Design Discussion

Key decisions and their rationale. For the flat list see `m6-decisions.md`.

---

## systemd Instead of Internal Process Management

**Chosen:** systemd manages all processes. m6-http is a pure proxy — it connects to backends over Unix sockets and expects them to be running.

**Rejected:** m6-http spawning, monitoring, and restarting renderer processes.

**Why:** systemd is a better process manager than anything m6-http could implement. It handles restart policies, resource limits, dependency ordering, log capture, and service isolation. Reimplementing this inside m6-http would add significant complexity for worse results. The right tool for process management already exists on every modern Linux system.

**Development experience:** A shell script starts all processes. This is simple, explicit, and debuggable. Developers understand shell scripts.

---

## Socket Pool Glob Discovery

**Chosen:** Backend pools declared as socket globs: `sockets = "/run/m6/m6-html-*.sock"`. m6-http watches the socket directory via inotify. Sockets appearing or disappearing update the pool automatically.

**Rejected:** Explicit socket list in config. Explicit list requires a config change and reload to scale.

**Why:** Scaling should not require touching config. Start a new systemd unit — a socket appears — m6-http picks it up within milliseconds. Stop a unit — socket disappears — removed from pool. The operational ceremony of editing config to scale is eliminated. inotify on `/run/m6/` is trivial to implement alongside the existing inotify watch on the site directory.

---

## Auth Completely Absent from Hot Path

**Chosen:** Auth is structurally absent, not conditionally skipped, on routes without `require`. Routes compile into distinct types at startup. A public route is a different code path from a protected route. The hot path — cache hit on a public route — never executes any auth code.

**Rejected:** Conditional check (`if route.require.is_some()`) on every request.

**Why:** A static site is the base case and must be as fast as possible. A conditional branch on every request adds cost even when the branch is never taken — branch predictor pressure, option unwrapping, cache line usage. The right model is structural absence: if there's nothing to check, there's no checking code on the path at all.

**Consequence:** A site with no `require` declarations does not need `[auth]` in `site.toml`, does not need the public key file, and does not need m6-auth running. Auth is an absent feature, not a disabled one.

---

## Local JWT Verification

**Chosen:** m6-http verifies JWT signatures locally using m6-auth's public key. No network call per request.

**Rejected:** m6-http calling m6-auth to verify each token.

**Why:** A network call per authenticated request would add latency on every protected route. JWT signature verification is a local cryptographic operation — fast, deterministic, no I/O. m6-http holds the public key and verifies independently. m6-auth is only contacted for stateful operations: login, refresh, logout, user management.

**Key rotation:** m6-auth signs with the new key immediately on rotation. m6-http reloads the new public key via inotify. Previously issued tokens signed with the old key expire naturally — they are short-lived (15 minutes) so the window of dual-key validity is small.

---

## Auth Enforcement at m6-http Plus Fine-Grained Renderer Checks

**Chosen:** m6-http enforces route-level auth (`require`) before forwarding. Renderers can additionally inspect JWT claims for resource-level decisions.

**Rejected:** Auth enforcement only at the renderer level.

**Why:** Route-level enforcement at m6-http means unauthenticated requests never reach the renderer. This is a hard security boundary that does not depend on every renderer implementing auth correctly. It also means auth is declared once in `site.toml` and enforced consistently, regardless of which renderer handles the route.

Renderer-level fine-grained checks are needed for cases `require` cannot express: "can this user access this specific document?" depends on the document's ACL, not the route. Both levels are needed. m6-http handles coarse-grained (route-level); renderers handle fine-grained (resource-level).

**Claims forwarding:** Verified claims are forwarded in `X-Auth-Claims` so renderers receive them without re-verifying the JWT.

---

## No Static Error Fallback File

**Chosen:** `[errors] mode` — three options for what m6-http returns when no backend responds. No static file.

**Rejected:** `error-fallback.html` loaded into RAM at startup.

**Why:** A static fallback file is complexity that systemd makes unnecessary. If a renderer crashes, systemd restarts it in seconds. The window where all pool instances are down is very short. During that window, returning a clean HTTP status code (503) with an optional description is sufficient. Maintaining a static HTML file, validating its presence at startup, loading it into RAM, and serving it correctly for all content types is overhead that solves a problem that barely exists in a systemd-managed deployment.

---

## One m6-html Process Handles All HTML Routes

**Chosen:** One m6-html process (per pool instance) with a `[[route]]` table in its config.

**Rejected:** Multiple m6-html instances, one per route type.

**Why:** One process, one config, one log stream, one systemd unit (per instance). The config gives a complete picture of what m6-html does — all paths, all templates, all data dependencies in one file. Multiple instances multiply operational surface area with no architectural benefit.

---

## No Build Step in m6

**Chosen:** m6 has no build step. Content JSON is produced by external tools or user renderers. m6 is a runtime system only.

**Rejected:** m6-build (Makefile generator), m6-site (build.rs crate), m6-content (Markdown processor).

**Why:** Markdown processing is a content authoring concern, not a web serving concern. Coupling them forces m6 to have opinions about content formats, build tools, and file processing pipelines. A separate tool that understands the m6 site directory structure can handle content generation without any dependency on m6 internals. m6 stays focused on what it does: routing, caching, and rendering.

---

## build.rs Over a Dedicated Build Tool (Historical)

This decision was superseded — m6 has no build step at all. Recorded for completeness.

A dedicated build tool (`m6-build`) was rejected in favour of a `build.rs` crate (`m6-site`) because `build.rs` eliminated `make` as a dependency and called the m6-content library directly. `m6-site` was then rejected entirely when the decision was made to remove the build step from m6 altogether.

---

## Single `/error` Route

**Chosen:** One `/_errors` route (configurable path). m6-http makes an internal request to that path, passing error context as query params `status` and `from`. m6-html injects these as built-in template keys. m6-http returns the rendered HTML with the original status code.

**Rejected:** Separate `/error/404`, `/error/500` routes; `status_from` key.

**Why:** One route, one template, error context passed as headers rather than query params — the URL stays clean, nothing appears in logs or browser history. Adding new error context fields requires no config change. Recursion detection is trivial: if the request path matches the error path, don't fetch again.

---

## Single-Threaded Epoll in m6-http

**Chosen:** Single-threaded epoll. No Tokio. No async/await.

**Why:** A cache hit is a hash map lookup and a write. An async runtime adds overhead on this path. inotify, backend fds, and network fds all sit in one epoll set — one thread, no synchronisation on the hot path. The implementation is harder to write correctly but far simpler to reason about under load.

---

## TLS Always Required

**Chosen:** No plain HTTP. `mkcert` for development.

**Why:** HTTP/3 requires TLS. Supporting plain HTTP means two code paths. `mkcert` makes local TLS a two-command setup. Development and production are identical in protocol.

---

## Socket Communication Between m6-http and Renderers

**Chosen:** Unix sockets. No TLS between m6-http and renderers.

**Why:** Unix sockets are local — no network exposure, no TLS overhead, faster than TCP loopback. m6-http terminates external TLS; internal communication is trusted and local.

---

## Config Layering: Site Config vs System Config

**Chosen:** System config as a required positional argument: `m6-http <site-dir> <system-config>`. Contains only `[server]` — the bind address and TLS paths that differ between environments. Development uses a minimal checked-in system config (e.g. `configs/system-dev.toml`). Production uses `/etc/m6/my-blog.toml`. Everything else comes from `site.toml`. Renderer secrets declared via `secrets_file` key pointing to an arbitrary TOML file.

**Rejected:** Optional second argument. Required is simpler to spec and test — one code path, no "if provided" branches. The dev system config is trivial (three keys) and can be committed to the repo without issue.

**Rejected:** Convention-based discovery `/etc/m6/<site-dir-name>.toml`. Explicit is better — the systemd unit spells out exactly what config it uses. No naming magic, no ambiguity if site directory names change.

**Rejected:** `.m6ignore` to exclude sensitive files from rsync. An ignore file is invisible — a developer using a different deploy tool silently overwrites production credentials. Structural separation enforces the boundary regardless of deploy method.

**Why only `[server]` in the system config:** `[server]` is the only section that genuinely differs per environment — bind address for localhost vs public interface, mkcert for dev vs Let's Encrypt for production. Everything else (routes, backends, auth config, log level) is the same. Restricting the system config to `[server]` keeps its scope minimal and its purpose obvious.

**`secrets_file` for renderer credentials:** SMTP passwords and similar secrets are declared as a path in the renderer config. The path points outside the site directory to a file that never gets deployed. In development, `secrets_file` is absent — the renderer config provides fallback values (localhost SMTP mock). In production, `secrets_file` is present and wins on conflict. One explicit key, visible in the config, points to exactly where the secrets live.

**Why system config wins on `[server]` conflict:** The server operator needs confidence that their TLS cert path and bind address are used regardless of what the developer deploys. A bad rsync cannot revert production to `localhost.pem`.

**`site.toml` is clean:** No passwords, no cert paths, no environment-specific values. The repo can be public. The developer never touches production credentials; the operator never touches routing.
