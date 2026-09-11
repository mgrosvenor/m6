# Handover

State of play for the next session. Written 2026-09-11.

**Read this, then `docs/CONSOLIDATION-TODO.md`.** This file is what is true;
that one is what is left, as a list to tick off.

---

## 1. Where the work is

**Branch `main`, clean, 40 commits ahead of the deployed `b32e837` here and 6
in the site repo. Nothing is deployed. The freeze holds until the migration is
finished.**

- 923 workspace tests pass at default features. Zero warnings.
- h1spec **32/32 on all four HTTP/1.1 targets**, with a CI ratchet
  (`tools/conformance.sh`, floors in `tools/conformance-scores.txt`) wired into
  `check.sh` as a blocking gate.
- h2spec and h3spec are **not installed on the laptop** and skip on every local
  run. They must run on the build box before deploy.

### Migration status (`docs/m6-core-implementation-plan.md`)

| phase | what | status |
|---|---|---|
| 0 | Prerequisites | done |
| 1 | Small consolidations | done |
| 2 | `m6_core::testkit` | done |
| 3 | Semantics | done, 3.3 dropped |
| 4 | HTTP/1.1 | done, 32/32 |
| 5 | Service loop | done |
| 6 | Consumer apps link `m6-core` only | done |
| 7 | Decouple the repositories | **next**, not started |
| 8 | Backend examples | not started |

**`m6-render` no longer exists.** m6-core is the only crate a service links.

---

## 2. Standing constraints that do not lapse

- **Never use em dashes** in prose written for the owner.
- **Zero compiler warnings**, pre-existing included, checked on Linux.
- **Test locally, commit, then deploy. Never deploy from an uncommitted tree.**
- **Secrets never enter git.** Only `.example` files, paths, documented shape.
- Any change to **layout, copy, or rendering** needs individual approval before
  it ships.
- Image resizing is the owner's job.
- Firewall blocks are **per-IP only**, no CIDR rules.
- Crawler sightings are reported **explicitly, every run**, even a quiet one.
- The build box is **not backed up**. Everything done to a node is in git.
- Keep dynamic allocations to an absolute minimum. **Latency is the key
  metric.**
- "Clean and consistent is the only way forwards. Apps should deviate only
  where functionality demands it."
- "Clean simple code with lots of reuse out of core. This is not the place to
  get clever or inventive."

### The architectural rule, restated 2026-09-11

**m6-core is the PHP of m6: a box of blocks a service is assembled from.**
Anything we can reasonably expect to generalise to other sites and instances
belongs in core, and core should be **the only thing a service needs to link**
to build its own m6-compatible service. mgrosvenor.com is the first instance of
a general system, not the thing the system is for.

A default service being nearly a no-op on top of core is the result we want,
not a smell. `m6-html` is the whole renderer:

```rust
use m6_core::prelude::*;
fn main() -> anyhow::Result<()> { App::new().run()?; Ok(()) }
```

---

## 3. What m6-core is now

12,088 lines across 30 modules. This is the reference a new service is written
against, and **writing that reference properly is an outstanding task**
(`CONSOLIDATION-TODO.md` item 2).

| module | what |
|---|---|
| `app` | the service loop: thread pool, bounded queue, 503 backpressure, routing, config reload |
| `h1`, `parse`, `http` | HTTP/1.1 parsing, framing, the one response writer |
| `server` | unix socket server, `serve_connection` |
| `request`, `response` | the handler-facing types, form/query/cookie parsing, percent coding |
| `cookie` | the one `Set-Cookie` formatter |
| `headers` | case-insensitive access, repeated fields, RFC-correct combining |
| `config` | TOML config loading |
| `ndjson` | newline-delimited JSON, read and written |
| `telemetry` | analytics records, `periodic stats`, traffic classification |
| `host` | load, memory, disk, thermal, uptime. Reading only |
| `monitoring` | `/health` and `/perf`, shared by producer and consumer |
| `render`, `template` | the renderer seam, and Tera behind it |
| `conditional`, `negotiate`, `mime`, `compress`, `minify` | caching semantics, content negotiation |
| `signal`, `watcher`, `log`, `random`, `path`, `util`, `error` | the rest of the runtime |
| `testkit` | the shared harness, behind a feature |

Services: `m6-http` (edge/proxy), `m6-file`, `m6-html`, `m6-auth-server`,
`m6-md`, `m6-monitor`, plus `m6-auth`/`m6-auth-cli`.

---

## 4. Immediate next steps, in order

1. **Document m6-core in full.** Owner's request. Start with the list of every
   component available, then detail each component and its interface. Core is
   now the only crate a service links and there is no reference to write one
   against.
2. **Finish header to dict.** `FrameworkState::build_dict` is private and is
   where the real knowledge lives: twelve ordered steps, and the ordering is
   load-bearing (built-ins go in *after* params files so a params file cannot
   override them). The dict-to-header half landed in `3e7a7d8`.
3. **Compile the watcher fix on the build box.** `m6-core/src/watcher.rs` is
   inside `#[cfg(target_os = "linux")]`, no Linux target is installed here, and
   the inotify alignment fix in `8bcebac` has **never been through a
   compiler**.
4. **Prove `m6-monitor` against the real fleet.** It is tested and has never
   polled a real node. Those are different claims.
5. **The m6-http header sweep**, about a dozen ad-hoc lookups left.
6. **HTTP Garden** (arxiv 2405.17737), the differential fuzzer. Needs Docker,
   so the build box.
7. **h2spec and h3spec on the build box** before any deploy.
8. **Benchmark Phases 5 and 6.** The plan requires a delta per phase and
   neither has one. The whole request path moved between crates.
9. **Phase 7**, then **Phase 8**.

---

## 5. The hourly health check

**It is a program now: `tools/health-check.py`.** One ssh per node, run in
parallel, all five parts across all three nodes, read-only, exit 1 on faults.
`--load` also reads a loaded window and labels it GENERATED.

`docs/health-check.md` is still worth reading, but as *why*, not *how*. Every
trap it describes is encoded in the script.

Three nodes, always all three:

| node | role | service | WireGuard | analytics file |
|---|---|---|---|---|
| syd | origin | `m6-http-origin` | 10.0.0.1 | `/var/www/dr-grosvenor-site/logs/analytics.ndjson` |
| lon | cache | `m6-http-cache` | 10.0.0.4 | `/var/www/m6-cache/logs/analytics.ndjson` |
| chi | cache | `m6-http-cache` | 10.0.0.5 | `/var/www/m6-cache/logs/analytics.ndjson` |

Most of this is now reachable over HTTP instead of ssh, because `/perf` carries
the host's load, memory, disk and temperature. What still needs a shell: the
**log-target histogram** (Part A) and **ufw block counts**. A log-target count
on `/perf` would remove Part A outright and is worth doing.

---

## 6. What this session produced

### Phases 5 and 6: m6-render is gone

`app.rs` (2,651 lines), `request`, `response`, `config`, `server`, `util`,
`error`, `multipart` and `template` all moved into core. m6-render deleted.

The render seam is `m6_core::render`: `Renderer`, `RendererFactory`,
`RenderError`, `NoTemplates`. Core routes, builds the dictionary, compresses
and writes; it does not depend on Tera at the type level, only by default.
`App::new()` gives you Tera; `.renderer(NoTemplates)` opts out.

**`{{ not_found() }}` is typed now.** It used to fail the render with the magic
string `__M6_NOT_FOUND__` and the loop recovered the intent with
`msg.contains(..)`.

**The plan's justification for Phase 6 was wrong**, and it is worth not
repeating: "three of four consumers have zero template files yet link Tera" is
true about files and false about need. All three render templates out of the
*site* directory and use `| asset` and `img_dims()`. Giving them `NoTemplates`
would have compiled and 500'd every page.

### New in core

`ndjson`, `telemetry`, `host`, `monitoring`, `cookie`, `headers`. See §3.

`AnalyticsRecord` is the definition the analytics format never had: m6-http
emits rows through `tracing::info!` so the JSON shape was whatever the
subscriber produced, and every consumer re-derived it by squinting at a sample.

### `m6-monitor`, new service

Polls every node's `/health` and `/perf`, serves `/` as a page and `/digest` as
JSON. Central node only. `deploy/ORIGIN-NODE.md` in the site repo is the
runbook.

**The WireGuard mesh is not a path to those endpoints.** Measured: origin's
`10.0.0.1:80` is h2c-only and does not answer HTTP/1.1 at all, and the cache
nodes have *no* backbone listener. It polls the per-node public names instead,
which means it measures through each node's public edge and is **not**
comparable with the loopback TTFB.

### Bugs fixed, all awaiting the same deploy

Carried forward from Phase 4: the `:80` redirect SIGTERM bug, the dropped query
string, HEAD-with-a-body on error paths, `OPTIONS *`/`CONNECT` 400s, the
WireGuard client-IP attribution, `conformance.sh --update` deleting floors.

New this session:

7. **`/health` was publishing the internal topology.** Every backend pool by
   name with worker counts, plus `uptime_s` and `url_backends`, on an
   unauthenticated public URL. Now `{"status","node"}`; the detail moved to
   token-gated `/perf`. The test that should have caught it checked an
   allowlist that the leaked fields were *on*.
8. **The inotify read buffer was unaligned.** Cast a `[u8; 4096]` straight to
   `*const libc::inotify_event` and dereferenced it. Undefined behaviour that
   happened to work. **Still never compiled.**
9. **`looks_like_injection` matched the raw path**, so `UNION%20SELECT` never
   matched `union select`. SQLi in a URL is always percent-encoded, so the
   detector read clean against exactly the traffic it exists to catch.
10. **Two stale `csrf` tests** had not compiled since Phase 4. `run-tests.sh`
    runs default features, `csrf` is not one, so nothing ever built them.
11. **`check-templates.sh` gated on a log line that no longer exists**
    (`"m6-render started"`, now `"routes loaded"`), so the template gate
    reported FAILED on a clean start. Run by hand, which is why nobody noticed.

### Site repo

The container gutter asymmetry (`#curriculum-value` set `padding-left` and
never `padding-right`, 7px against 25px at ≤768px, experience page only), the
1144px max-width, `style.css` normalised to LF with a `.gitattributes`, and the
renderers switched to m6-core.

---

## 7. Open questions, honestly unresolved

- **The cache-hit p50 baseline is unverified.** Recorded as 1.7-2.2us; four
  separate readings today put every node at 2.9-3.4us, flat across hundreds of
  windows. The baseline is the thing in doubt, not the measurement. Re-derive
  before treating a miss as an incident. `tools/health-check.py` deliberately
  asserts no p50 threshold for this reason.
- **24h hit rates**: syd ~0.58, lon ~0.16-0.19, chi ~0.19-0.21. Stable across
  the day. The earlier reconciliation that looked wrong was my own error:
  origin never sees what a cache node answers from its own cache.
- **`redirect_lifecycle::sigterm_shuts_down_rather_than_being_ignored` is
  flaky.** Failed once in a loaded full-suite run, would not reproduce in 21
  further runs, and I never captured the assertion text. Ruled out: the obvious
  startup race, because `redirect::run` installs `ShutdownHandle` *before*
  `bind_plain`. It guards a bug that shipped.
- **A coordinated probe hit chi** at 05:47-05:50 UTC: `34.91.241.0` (GCP), 890
  requests in under three minutes rotating 526 user agents, targeting SSRF
  (`/fetch`, `/proxy`), cloud credentials (`.aws`, `.azure`, gcloud ADC) and
  LFI (`/@fs/...`). It got 795 404s, 85 429s and ten 200s, all of them `/`.
  Nothing sensitive served. Candidate for a per-IP block; not blocked, because
  the check is read-only.

---

## 8. Lessons that cost something to learn

The first twelve are from Phase 4 and are in the plan too. These are the ones
worth carrying.

1. **Measure the candidate before consolidating onto it.** The plan named
   `m6-core/src/parse.rs` as the consolidation target for HTTP/1.1; measured,
   it was the *worst* of the four at 14/32.
2. **A test can pin wrong behaviour as firmly as right behaviour.** Both
   `/health`'s allowlist test and `empty_body_is_unchanged` passed for as long
   as the defect existed.
3. **A number measured from traffic you generated is not a production number.**
4. **An absent file at the path you expected is not evidence the feature is
   off.** Check where the process actually writes.
5. **Conformance harnesses produce fake scores.**
6. **Kill by PID. Never `pkill -f <pattern>` naming a port or config path.**
7. **Benchmark paired and interleaved against a fixed baseline.**
8. **Do not hand-roll a conformance tester.**
9. **Derive a security boundary structurally, not from config.**
10. **Safe by default, opt in explicitly.**
11. **Blocking a signal without installing a handler makes a process
    unkillable.**
12. **Silently swallowing a parse error looks like health.**

New this session:

13. **A feature gate that hides code from the default test run is how tests
    rot.** Two `csrf` tests stayed broken from Phase 4 to Phase 5 because
    `cargo test --workspace` never compiled them. This is why chrono and lru
    are unconditional dependencies of core rather than gated: the owner chose
    the dependency over the blind spot.
14. **One sample is not a measurement, and a monitoring tool will measure its
    own effect.** The health check flagged its own load generator as an
    incident, then reported a 50% latency regression from a single sample that
    landed during that load. Both were fixed in the tool. Watch for a third.
15. **A claim can be literally true and support a false conclusion.** "Zero
    template files" was true of the three renderers and did not mean they
    needed no template engine.
16. **Read the full user-agent list; a keyword list invents crawlers.** One IP
    rotating 526 user agents would have been reported as a dozen AI crawlers
    visiting. `UA_ROTATION_THRESHOLD` makes that a property of the data.
17. **Encoding and decoding are two halves of one block.** Core could
    percent-decode and not encode, so callers wrote their own encoder. The same
    shape as having four cookie formatters and no cookie type.
18. **Check the transport before writing the runbook.** The monitor was
    designed against the WireGuard mesh because that is the obvious answer;
    origin's backbone listener is h2c-only and the cache nodes have none.
