# The ledger: what is done, and what is not

Rewritten 2026-09-11 after an audit against the commit log rather than from
memory. Three items here were marked open while already done, and about ten
pieces of work were not recorded at all.

**`HANDOVER.md` is what is true. This is what is owed.**

Ordering principle, the owner's: **m6-core is the PHP of m6, a box of blocks a
service is assembled from.** Anything we can reasonably expect to generalise to
other sites and instances belongs in core, and core should be the only thing a
service links.

---

## Done, 2026-09-11

Verified by commit, gate green at each step unless noted.

### The migration

- [x] **Phase 5**, the service loop into `m6-core`. `15f6f18`, `66f4611`,
      `fff59e2`.
- [x] **Phase 6**, `m6-render` deleted; m6-core is the only crate a service
      links. `c6999a1`, site side `02ca1ad`.
- [x] **Percent-coding consolidated**, m6-auth-server's five duplicate helpers
      deleted, core gained `url_encode`/`url_encode_path`/`cookie`. `bafcded`.
- [x] **One cookie formatter, one header accessor.** Four `Set-Cookie`
      `format!` calls became `m6_core::cookie`; `m6_core::headers` carries the
      RFC 9110 5.3 / RFC 6265 3 combining rule. `3e7a7d8`.

### New in core

- [x] `ndjson`, `telemetry`, `host`, `monitoring`, `render`, `cookie`,
      `headers`, `firewall`. `53021db`, `c5b56a4`, `74b7d53`.
- [x] **`/traffic`**: the node summarises its own analytics and publishes it,
      so the log never moves. `dd196c0`.
- [x] **`LogPulse`**: Part A answered from inside the process, no `journalctl`.
      `dd196c0`.
- [x] **Host counters**: net, disks, PSI, TCP (`ListenOverflows`), fds.
      `74b7d53`.
- [x] **Firewall state from `nft -j`**, native JSON, no parser invented.
      `74b7d53`.

### Services and tooling

- [x] **`m6-monitor`**, fleet digest. `c5b56a4`. Runs off-fleet, on the build
      host: a monitor on syd cannot report that syd is down. `74a6c10`.
- [x] **`m6-monitor --check`**, the health check as a binary. `d524334`.
- [x] **`tools/health-check.py`**, the script it replaces. `98dcb53`.

### Production changes (applied on instruction, outside the freeze)

- [x] **systemd hardening, fleet-wide.** 1.7 OK from `systemd-analyze` on
      every unit. Site repo `4e4caa8`. Cost London ~90s of downtime on the
      first attempt; see HANDOVER lessons.
- [x] **SMTP verified post-hardening** by a real submission. Site `792d7ed`.
- [x] **Block ledger reconciled**, 26 identical rules on all three nodes, with
      `sync-blocks.sh` to keep them that way. Site `178f171`.
- [x] **`80.94.95.211` blocked**: 843 paths, 1,553 requests, got nothing. It
      had been probing since 2026-09-05, six days rather than one. Verified
      11:57: last reached the application at 10:08:22, before its rule, and
      not since.
- [x] **Blocks verified as effective**, not merely installed. The orphaned
      Route53 checks are dropping ~1,350 packets on chi across six addresses.

### Bugs found and fixed

- [x] `/health` was publishing internal topology. `9dbc9a6`.
- [x] Unaligned inotify read buffer, UB. `8bcebac`. **Never compiled**, below.
- [x] `looks_like_injection` matched the raw path, so percent-encoded SQLi
      read clean. `53021db`.
- [x] Two `csrf` tests had not compiled since Phase 4. `15f6f18`.
- [x] Three health-check self-measurement defects. `751e311`, `72551ef`,
      `f10bcc9`.
- [x] A forged single-identity crawler passed as genuine. `5db0a39`.
- [x] `check-templates.sh` gated on a log line that no longer exists. Site
      `302f602`.
- [x] Site container gutters and LF normalisation. Site `a1ff3b7`, `08ffbae`.

---

## Not done

### 1. Header to dict

- [ ] `FrameworkState::build_dict` is private and is where the real knowledge
      lives: twelve ordered steps, and the ordering is load-bearing (built-ins
      go in *after* params files so a params file cannot override them). A
      service not using `App` cannot reuse any of it. The dict-to-header half
      is done, `3e7a7d8`.

### 2. Document m6-core in full

- [x] Owner's request. **`docs/m6-core-reference.md`**, written 2026-09-11
      against the source rather than against the older docs. All 30 modules
      grouped by task, the interface for each, the twelve ordered steps of
      `build_dict` including why step 8 is load-bearing, and a closing section
      of known gaps rather than a stop at the good parts.
- [x] `m6-render-lib.md` marked **SUPERSEDED**: it documents a deleted crate.
      Kept rather than removed, because it is the only description of the
      renderer lifecycle written while someone was using it.
- [x] `m6-core.md` §9 marked **historical**. It was the pre-migration gap
      analysis and read as current state.
- [ ] **Seventeen of thirty modules have no module-level doc comment**: `app`
      has a one-line stub, and `compress`, `config`, `error`, `http`, `log`,
      `mime`, `minify`, `multipart`, `parse`, `path`, `request`, `response`,
      `server`, `signal`, `template`, `util`, `watcher` have none. The thirteen
      that do are the best documentation in the repository, which makes the gap
      sharper rather than softer. The reference covers the interface; these
      would carry the *why*, next to the code.

### 3. Remaining audit findings

- [x] **Case-insensitive header lookup.** **DONE 2026-09-12.** m6-http used
      **none** of `m6_core::headers`; it is now the only implementation.
      **25 sites, not "about a dozen"**, across `analytics.rs`, `cache.rs`,
      `forward.rs`, `http11.rs`, `http2.rs`, `redirect.rs`, `security.rs` and
      twelve in `main.rs`. Zero remain in non-test code, checked by pattern.

      Name-against-a-constant-list comparisons (`HOP_BY_HOP`,
      `UNTRUSTED_INBOUND`) were deliberately left alone: those are not lookups,
      and `eq_ignore_ascii_case` is the right call there.

      **Two sites were taking the first of a repeatable field** and are now
      `get_all`, which is the real win rather than the tidiness:
      `analytics.rs`'s Set-Cookie scan, the exact field the headers module
      documents as the one everybody folds by mistake, and `http11.rs`'s
      `Connection` token check, which would have missed a token sent on a
      second field line.
- [ ] **Calendar arithmetic hand-rolled in `m6-md`.** `is_leap`, `doy_to_md`,
      days-since-epoch by hand. Core has chrono unconditionally now, so there
      is no dependency argument left.
- [x] **ETag / conditional in `m6-file`.** **ALREADY DONE; this row was stale.**
      Closed 2026-09-11 in `f4bdfed` and never struck off here. Verified
      2026-09-12: `m6-file/src/handler.rs` calls
      `m6_core::evaluate_preconditions` and reads `m6_core::Precondition`; the
      only trace of the old copy is a comment quoting the line it replaced.

      It did genuinely differ, which is why it mattered: the inline version was
      `inm.split(',').any(|tag| tag.trim() == etag)`, byte equality, which is
      *strong* comparison, and `If-None-Match` requires weak (RFC 9110
      8.8.3.2). `If-Match` and `If-Unmodified-Since` were not consulted at all.

### 3c. `watcher.rs` is hand-rolled unsafe libc and should not be

**OWNER'S INSTRUCTION, 2026-09-12: put it on the list.** The verdict was
"standard OS interfaces to watch a file and poll to wake up when there's a
change; extra threads are totally unnecessary; there are standard Unix wrappers
around all of these."

**The threads are already gone** (`e6ba278`): macOS now registers on a single
kqueue and returns that descriptor from `raw_fd`, so the service's existing
`poll(2)` waits on it directly, as Linux already did with inotify. That fixed a
startup race and a thread leak, and cut the watcher tests from 5.01s to 0.31s.

**What is still wrong is the level it is written at.** 390 lines of raw
`unsafe` libc across three `#[cfg]` arms: `inotify_init1`, manual pointer
arithmetic over `inotify_event` (which is where the unaligned-read UB came from
in the first place), `kqueue`, `kevent`, `open(O_EVTONLY)`, hand-managed
descriptors and three `Drop` impls. None of that is m6's problem to solve.

- [ ] **Rewrite on `nix`'s safe wrappers.** `nix::sys::inotify` and
      `nix::sys::event` cover both platforms, and **`nix` is already a
      dependency** of m6-core, since it is where `nix::poll` comes from. No new
      dependency, and the unsafe blocks and the manual event-buffer walk both
      go.

**Constraint the rewrite must not break:** the watcher exposes a pollable file
descriptor and the *service's own* poll loop waits on it. The obvious crate,
`notify`, spawns a background thread and delivers over a channel, which would
reintroduce exactly what `e6ba278` removed. A wrapper is wanted, not a runtime.

This also retires the standing lesson about the unaligned `inotify_event` read:
that bug existed because the code was doing pointer arithmetic it had no
business doing.

### 3a. Cache-hit p50 regression: 2.1x in six days, cause unknown

**TRACKED 2026-09-12. Open, not started, and deliberately not closed by
re-baselining.**

Production cache-hit p50 on the origin, all from m6's own `hit_p50_ns` over
loaded windows, same node, same method:

| date | deploy | p50 | p99 |
|---|---|---|---|
| 2026-09-06 | s-maxage verify (41 + 29 hits) | **1.7us** | 2.0us |
| 2026-09-10 | Rapid Reset `438bdb3` | 2.5us | - |
| 2026-09-10 | flow control `b32e837` | 2.95us | 3.55us |
| 2026-09-12 | `22ee3a4` loaded, 76 hits | **3.61us** | 4.52us |
| 2026-09-12 | `22ee3a4` 24h, 268 windows | 3.90us | 4.20us |

Latency is the owner's stated key metric, so this is not a cosmetic drift.

**Already ruled out** (2026-09-12): the `22ee3a4` monitoring-accounting change
(monitor polls were excluded before and are still absent from `hit_p50_ns`);
machine pressure (syd idle, load 0.08, 601MB free, no swap traffic); cache
growth (process up 15 hours).

**Still unknown:** which commit, or whether it is code. Two unmeasured
hypotheses, Rapid Reset's per-stream accounting and `22ee3a4`'s move from a
substring match to real q-value parsing per request. Against both, the *same
binary* read 2.95us on 2026-09-10 and 3.6-3.9us today.

- [ ] **Paired, interleaved A/B on the build box** across `084f89e`,
      `438bdb3`, `b32e837`, `22ee3a4` and HEAD. One load, one host, medians of
      five, per lesson 7. **This is the same work as item 8's owed "benchmark
      Phases 5 and 6"** and should be done once, for both.

**Do not close this by adjusting the baseline.** Three sessions did that, on
the reasoning "we keep measuring ~3us, so 1.7-2.2 must be wrong", which is
backwards: a regression that lands before the first reading makes every
subsequent reading agree with the others. Consistency is not correctness.

### 3b. One app shape, not three

**SCOPE DECISION, 2026-09-12, NARROWED BY THE OWNER THE SAME DAY.**

The first version of this note said everything outside §3b-now was deferred,
and that was recorded too widely. The owner's correction, verbatim: *"IO layer,
the event loop, the handler contract. These are deferred. Only."* and *"I/o
layer is arguably low touch consolidation work."*

So the deferred set is **the event loop and the handler contract**, with the IO
layer in scope as low-touch. Everything else below is live work:

- **wildcard route segment** and **streaming response body** are in scope, not
  deferred, and they are what m6-file's migration waits on.
- **both migrations are in scope.** They were never in the deferred list; they
  are sequenced after core gains the capabilities they need.
- **m6-auth-server's migration is unblocked as of 2026-09-12.** Its stated
  blocker was `chmod` on the socket, which is now `[server] socket_mode`. Its
  four routes are all literal paths, so it needs no wildcard support.

#### 3b-now. The minimal set

Tier 1, half a day, near-zero risk. Per-connection handling is **already**
consolidated in `server::serve_connection`, so what is left is small:

- [x] **`App` sets a read timeout after accept.** **DONE 2026-09-12**, `d52a51b`.
      `[server] read_timeout_s`, default 30, `0` disables; `App` applies it
      before handing the connection to a worker. m6-file and m6-auth-server had
      each hand-written the same 30 seconds into their own accept path and now
      call `server::apply_read_timeout`, so there is one implementation rather
      than four. Production needs no config change: the default applies.

      **It was not three lines, and the extra part is the interesting one.** A
      timeout reached `serve_connection` as `ParseError::Io(WouldBlock)`, which
      answers **400**. Into an *idle* connection that is a framing bug: m6-http
      pools backend connections, so the 400 sits in the socket buffer and is
      read as the response to the next request sent on it. `parse_request` now
      splits a timeout on whether any byte arrived, exactly as it already split
      `Ok(0)`: nothing yet is an idle peer leaving, closed silently; a stalled
      part-request gets a real 408. Guards in `parse` and in
      `m6-html/tests/read_timeout.rs`, all verified red first.
- [x] **Socket permissions as a config key.** **DONE 2026-09-12.**
      `[server] socket_mode`, an octal string, default `0660`.
      `server::apply_socket_mode` is the one implementation; m6-file and
      m6-auth-server both had the `set_permissions` block, not just
      m6-auth-server, and both now call it. m6-auth-server's main now differs
      in exactly one respect: it drives its own accept loop.

      **Three modes became one, and the fleet's effective mode changes.**
      m6-file and m6-auth-server set `0666` by hand; `App`'s five services set
      nothing and took `0755` from the umask. All seven are now `0660`. Nothing
      is lost: every unit runs `User=m6` and `/run/m6` is `0750` owned by `m6`,
      so the world bits never granted anything the directory did not already
      deny. **Verify the modes after the next deploy** rather than assuming;
      this is the first change here that alters a file permission in
      production.

Tier 2, about a day, moderate risk, optional:

- [x] **Extract the accept/poll block into core.** **DONE 2026-09-12**,
      `cf84546`, and only the poll block. `server::poll_listener_and_watcher`
      returns a `PollReady` of three flags; both callers had built the same
      `BorrowedFd` and `PollFd`, branched on whether the watcher had a usable
      fd, and unpacked `revents` identically, differing only in local names and
      in whether an absent watcher fd was `Option<RawFd>` or the sentinel `-1`.

      **What was deliberately left alone is what comes after the wait.** `App`
      submits to a bounded thread pool and answers 503 when the queue is full;
      m6-file sends down a channel to a fixed worker set and counts in-flight
      requests itself, drains every ready connection per wake, and resets
      `O_NONBLOCK` because its listener is non-blocking and `App`'s is not.
      Those are two concurrency models rather than two copies of one. Merging
      them is §3b-later, and forcing it here would be the wrong abstraction
      that `m6-core.md` warns costs more than the duplicate.

Unrelated to shape, free, no baseline needed:

- [x] **`send_with_length` has zero callers.** **DONE 2026-09-12.** m6-file now
      answers a HEAD from `metadata.len()` without opening the file, and
      `send_with_length` is what lets it: the length reported is separate from
      the bytes in hand.

      **Only when the representation is the file**, which is identity coding
      with minification off for the type. That is the constraint the item did
      not state and it is not optional: a HEAD has to report what the matching
      GET would send, so a minified or compressed representation genuinely has
      to be produced to be measured. Dropping the minification half of the
      condition makes a HEAD on `style.css` report 131 where the GET sends 106,
      which is what the guard catches.

      It is still the common case for the assets that cost anything, since
      images are neither minified nor compressed here. Two guards, because
      correctness and the saving are different properties:
      `head_reports_exactly_what_get_would` walks HEAD against GET across all
      three shapes, and `a_head_on_an_unreadable_file_still_answers` chmods the
      file to `000` and requires the HEAD to succeed anyway, so a refactor that
      quietly restores the `fs::read` fails. The first stays green when the
      fast path is disabled; only the second goes red.

#### 3b-later. Agreed, deferred, still on the list

Design is settled and written up in `docs/m6-app-shape-plan.md`. Not scheduled.

- [x] **Wildcard route segment.** **DONE 2026-09-12.** `Segment::Wildcard`,
      spelled `{*name}`, captures the rest of the path joined by `/`. Legal
      only as the last segment; anywhere else it is narrowed to an ordinary
      parameter with a warning rather than taking the service down over a
      pattern that is merely ambiguous.

      **Deliberately explicit rather than implicit.** m6-file spells the same
      idea as a bare `{relpath}` in final position, relying on its own matcher
      making the last parameter greedy. Core does not copy that: making the
      last `{param}` span several segments would have silently changed the
      meaning of every route already written, including every one in
      production. There is a test pinning that `{p}` is still exactly one
      segment, which is the guard that matters here.

      Specificity orders literal > param > wildcard, so adding a catch-all to a
      config cannot quietly capture the traffic of the exact routes beside it.
      `App`.
- [ ] **Streaming response body.** `Responder`'s three senders all take
      `&[u8]` and `Response.body` is a `Vec<u8>`, so core cannot serve a body it
      has not materialised. Matters on a 950MB box.
- [ ] **The IO layer**: one selectable stream interface whatever the transport,
      with blocking handled *inside* it by specific named components, never a
      generic offload. Pilot is unifying `PoolManager { pools, url_backends }`.
- [ ] **The event loop**: reads on the loop, connection state machine, handlers
      inline.
- [ ] **The handler contract** changes meaning from "may block" to "must not
      block". This is what everything above rests on.

---

The owner's standing requirement: **every app has the same general structure,
and that structure is documented well enough to pick up from outside the m6
repo**, with **no performance regression**.

**The target shape is single-threaded: one event loop, a switch over connection
state, everything non-blocking, and anything that must block on its own sync
thread signalling the loop through an fd.** memcached over libevent; QJump's
apps over CamIO. **m6-http is already this shape** and holds h2spec 146/146
with zero `thread::spawn` in the server and zero `.lock()` on its request path,
while `App` takes a mutex per request. `docs/m6-app-shape-plan.md` is the
evidence and the route; `docs/m6-app-anatomy.md` is the snapshot of today.

The case rests on the project's own history rather than on preference: the
SIGTERM defect that went silent for thirty days on syd **cannot exist in a
single-threaded program**, and the assertion in `install_with_hooks` is a guard
against a bug class the other model deletes. On a **1-core, 950MB** origin,
threads buy preemption rather than parallelism, which is worse for tail latency
on a 6ms render, and `m6-file`'s pool was widened from 1 to 32 reactively after
one page exhausted it.

Core is missing five things. Two are live defects in services that are already
the right shape, so they are worth doing whether or not anything is migrated:

- [x] **No read timeout on accepted connections.** **DONE 2026-09-12**,
      `d52a51b`. `[server] read_timeout_s`, default 30, applied by `App` before
      the connection reaches a worker; m6-file and m6-auth-server now call the
      same `server::apply_read_timeout` instead of keeping their own copies.
      Migrating the stragglers onto `App` no longer deletes the fleet's only
      two timeouts. **Still a stopgap for the reason the next item gives.**
- [ ] **The read happens on a worker, not on the event loop.** `app.rs:1872`
      accepts, `:1877` hands the **raw socket** to a worker, `:1920` the worker
      does the blocking read. The part whose timing an untrusted peer controls
      runs where capacity lives, so a 30s timeout on a two-worker pool still
      surrenders half the node for 30s. Moving the read to the loop, and
      dispatching only a **complete parsed `Request`** to a worker, is immune by
      construction and leaves the handler contract unchanged. It also leaves the
      project with **one I/O model**: `h1::parse_request(buf) -> ParseResult` is
      already incremental and m6-http already drives it from an
      `H1State::Reading { buf }` state machine; `parse.rs` is only the blocking
      adapter. This is the destination, deliberately sequenced last in
      `m6-app-shape-plan.md` §6 because everything else makes it smaller.
- [x] **`send_with_length` has zero callers.** **DONE 2026-09-12**,
      `a094851` and `7932e43`. m6-file answers a HEAD from `metadata.len()`
      without opening the file, but **only when the representation is the file**
      (identity coding, minification off, and `is_file()` — a directory also has
      metadata, which is how the first version answered 200 to `HEAD
      /assets/css`). A compressed or minified representation still has to be
      produced to be measured, because a HEAD must report what the GET would
      send. `Response` still cannot express it.
- Wildcard routing and streaming bodies were listed again here and are the
  same two items as in §3b-later above. Recorded once, there, so that closing
  one closes it. The short version: `Segment` is `Literal|Param` with exact
  segment-count matching, so `App` cannot express a static file server; and
  `Responder`'s senders all take `&[u8]`, so core cannot serve a body it has
  not fully materialised. **Together they are the whole of m6-file's reason to
  be a different shape**, and neither is deferred.
- [x] **No socket-permissions config key.** **DONE 2026-09-12**, `4fc33da`.
      `[server] socket_mode`, octal string, default `0660`. It was m6-file *and*
      m6-auth-server setting `0666` by hand, and `App` setting nothing at all,
      so its five services took `0755` from the umask. All seven are now one
      call to `server::apply_socket_mode`.

Then, and only then, the two migrations below.

- [ ] **`m6-file` should be an `App` service.** Its own `poll(2)` accept loop is
      **22 of 33 lines byte-identical** to `App`'s at `app.rs:1759`; the
      remainder are the same expressions with renamed locals (`pfd_ino`/`i`
      against `pfd_w`/`w`, `.unwrap_or(-1)` against an `Option`). It already
      calls `m6_core::server::serve_connection` per connection. The stated
      reason for the copy, needing the watcher fd and the listener in one wait
      set, is a thing `App` already does.
- [ ] **`m6-auth-server` should be an `App` service.** It binds through
      `UnixServer` directly. **Its stated blocker is gone**: the capability it
      needed that `App` lacked was `chmod` on the socket, and that is now
      `[server] socket_mode` (2026-09-12). What remains is that it drives its
      own accept loop, which is the same question as m6-file above.
- [x] **`m6-monitor` lifecycle is now proven.** It was always structurally
      correct (`App::new().route_get(..).run()`), but had no `tests/` directory
      at all, so nothing had ever started the binary. `m6-monitor/tests/lifecycle.rs`.
- [x] **`assert_app_lifecycle` moved into core's testkit.** The lifecycle
      contract was opt-in and hand-written in five integration suites, which is
      why the sixth service never got one. It is now one call.
- [x] **`socket_path_from_config` and `M6_SOCKET_OVERRIDE` consolidated.** Four
      copies: the derivation existed in `m6-core/src/server.rs` *and*
      `m6-file/src/config.rs` (differing fallback stem, `m6-file` against
      `m6-default`), and the override was wrapped identically in `app.rs`,
      `m6-file` and `m6-auth-server`. One implementation now, in core.

**`m6-http` is not on this list and should not be.** It is the edge: public TCP
and UDP, TLS, h2, h3, proxying, the cache. It is what `App` services sit behind.

### 4. Phase 7, decouple the repositories

- [ ] The site's renderers carry `m6-core = { path = "../../m6/m6-core" }`: a
      filesystem layout hard-coded across a repo boundary with no version
      constraint. Replace with a git dependency pinned to a revision.
      **Gate:** the site builds with no `m6` checkout beside it, and
      `deploy.sh` stops syncing the tree.

### 5. Phase 8, backend examples

- [ ] Six implementations of the same `/status` payload. Needs Go on the build
      host. **This phase is also the measurement:** Rust-without-core against
      Rust-with-core says whether core is worth linking.

### Owed, and easy to lose

- [x] **Compile `m6-core/src/watcher.rs` on the build box.** Done 2026-09-11 via
      `deploy/run-tests.sh`: m6 workspace on Linux, **962 passed, 0 failed, 0
      warnings** on the release and test builds, same test count as macOS so
      nothing is cfg'd out. The inotify alignment fix in `8bcebac` has now been
      through a compiler, and the compile-time assertion
      `align_of::<libc::inotify_event>() <= 8` was **evaluated** for the first
      time and holds. That was the premise behind `#[repr(align(8))]` and it
      was an untested assumption about the target's libc until now.
- [x] **`ConfigWatcher` has no tests, on any platform.** **DONE 2026-09-12**,
      `e6ba278`. Four tests, waiting on the watcher's own fd with `poll(2)` the
      way `App` does rather than sleeping: a write is seen and matched by name,
      an idle watcher stays quiet, an absent directory is skipped rather than
      fatal, and the Linux/macOS name-precision divergence is pinned in both
      directions.

      **Writing them found two defects in the macOS implementation**, both now
      fixed in the same commit: `new` returned before its watcher threads had
      registered their kevents, so an edge-triggered change in that window was
      lost silently; and those threads never exited, waking once a second to
      write to a closed descriptor for the life of the process. Both went away
      with the threads, which should never have existed: a kqueue descriptor is
      pollable, so it now goes straight onto the service's own poll loop, as
      Linux already did with inotify. See §3c for what is still owed there.
- [ ] **Staging cannot exercise the cache role.** `setup-staging.sh` is the same
      shape as production and states three deliberate differences, one of which
      has a sharper edge than it reads: staging is a **single origin, no cache
      nodes, no WireGuard**. The 90s London outage was a cache-role fault
      (`/run/m6` exists on an origin, not on a cache node), so staging would
      have started that unit cleanly and proved the change safe. A green
      staging run validates the origin role only. Worth writing next to lesson
      21 in the hardening fragment.
- [ ] **Benchmark Phases 5 and 6.** The plan requires a delta per phase and
      neither has one. The whole request path moved between crates.
- [ ] **Deploy `m6-monitor`.** It has now been run against the real fleet from
      the laptop and works; it has never been installed on the build host.
      `deploy/FLEET-MONITOR.md` is the runbook.
- [ ] **Deploy the firewall stats collector.** Written and unit-tested, on no
      node. Until then `/traffic` reports `firewall: null`.
- [ ] **Retire `tools/health-check.py`.** `m6-monitor --check` covers every
      part and `/traffic` now covers ufw via the collector. Blocked only on
      the two deployments above.
- [ ] **Raise the fd soft limit.** m6-http runs at 1024 against a 524288 hard
      limit. Harmless at 11 open, and the failure mode is `EMFILE` in an
      accept loop at 3am with nothing saying why.
- [ ] **`UMask` on the services.** Unset, so files are created world-readable.
      The one thing `systemd-analyze` still flags, and it matters for the
      analytics stream, which holds client IPs, session ids and user agents.
      Staging first, then lon, chi, syd.
- [~] **Two flaky tests, same shape.** **Five separate intermittent failures
      were diagnosed to root cause on 2026-09-12 and none was chance**; see
      HANDOVER open questions for the detail. Four helpers panicked on a
      transport error inside the retry loop written to tolerate it, and two unit
      tests raced over the process-global `SHUTDOWN_FLAG`. Each was reproduced
      deliberately before being fixed.

      **The two named here still have no explanation** and did not recur. What
      has changed is that the next one will be readable: the Linux gate returns
      the whole `failures:` section instead of a bare `panicked at <file>:<n>`,
      and the rule is to run the suite as
      `cargo test --workspace > /tmp/run.txt 2>&1` and grep the file, never the
      pipe.
- [ ] **`185.19.40.146`, block candidate, decision owed.** Three identical
      `//xmlrpc.php` sweeps on 2026-09-11 (06:34, 10:24, 11:48), ~20 requests
      each, 90% refused. Meets the same bar as `103.168.67.253` already in the
      ledger. One command: `./deploy/block-ip.sh 185.19.40.146 "..."` then
      `./deploy/sync-blocks.sh --apply`.
- [ ] **The 94.154.46.x operator is still cycling addresses.** `.250` appeared
      on 2026-09-11 running the same spoofed-Googlebot credential sweep, two
      days after `.243`-`.249` were blocked, exactly as BLOCKLIST.md predicted.
      It is not blocked: it was seen once and stopped. Watch for `.251`.

---

## Deployment state

**Nothing from the m6 migration is deployed.** 78 commits ahead of `22ee3a4`
here, 15 ahead of `d6ebfa5` in the site repo. The freeze holds until the
migration finishes; that is the owner's standing instruction.

The deployed commit is whatever the newest entry in
`~/dr-grosvenor-site/docs/RELEASES.md` names. Recompute from it; the previous
figure here named `b32e837`, which the 2026-09-10 18:47 deploy had already
superseded.

The production changes listed as done above are deliberate exceptions, applied
on instruction. They change how services are confined and what the firewall
denies, not what code runs.
