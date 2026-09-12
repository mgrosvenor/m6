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

- [~] **Case-insensitive header lookup.** Core is converted; about a dozen
      ad-hoc `.find(|(k,_)| k.eq_ignore_ascii_case(..))` remain in m6-http
      (`cache.rs`, `http2.rs`, `redirect.rs`, `security.rs`, `main.rs`).
- [ ] **Calendar arithmetic hand-rolled in `m6-md`.** `is_leap`, `doy_to_md`,
      days-since-epoch by hand. Core has chrono unconditionally now, so there
      is no dependency argument left.
- [ ] **ETag / conditional in `m6-file`.** Its own `If-None-Match` comparison
      next to `core::conditional`. Check whether it genuinely differs (strong
      vs weak) or merely duplicates.

### 3b. One app shape, not three

**SCOPE DECISION, 2026-09-12.** The architecture below is agreed and stays on
this list. It is **not** the near-term work. The near-term work is the minimal
set that makes the shapes roughly agree, in §3b-now. Everything else is
**deferred and tracked**, not dropped, and should not be re-litigated each time
it comes up.

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

- [ ] **Wildcard route segment.** `Segment` is `Literal|Param` and
      `match_route` requires exact segment-count equality, so no router can
      express a static file server. Only needed to migrate m6-file fully into
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

- [ ] **No read timeout on accepted connections.** Neither `App` nor
      `m6_core::server` sets one. `m6-file` and `m6-auth-server` each set 30s
      in their own mains; `m6-html`, `m6-monitor` and the three renderers have
      none. A peer that connects and sends nothing parks a worker in
      `parse_request`'s blocking `read()`, and the pools are two workers.
      **Migrating the two stragglers onto `App` as it stands would delete the
      only two read timeouts in the fleet.** Note this is a **stopgap**: see
      the item below for why a timeout is the wrong shape of fix.
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
- [ ] **`send_with_length` has zero callers.** It exists for "a HEAD answered
      without reading the file" and nothing calls it, so m6-file's HEAD path
      does a full `fs::read` + minify + brotli-6 and then discards the body at
      `h1.rs:700`. `HEAD /assets/vditor/dist/js/lute/lute.min.js` is 3.6MB of
      work to return a header. `Response` cannot express it either.
- [ ] **No wildcard route segment.** `Segment` is `Literal|Param` and
      `match_route` requires exact segment-count equality, so `App` cannot
      express a static file server. This is the whole of m6-file's reason to be
      a different shape.
- [ ] **No streaming response body.** `Responder`'s three senders all take
      `&[u8]` and `Response.body` is a `Vec<u8>`, so core cannot serve a body it
      has not fully materialised. m6-file is not choosing to buffer.
- [ ] **No socket-permissions config key.** `m6-auth-server` sets `0666` by
      hand.

Then, and only then, the two migrations below.

- [ ] **`m6-file` should be an `App` service.** Its own `poll(2)` accept loop is
      **22 of 33 lines byte-identical** to `App`'s at `app.rs:1759`; the
      remainder are the same expressions with renamed locals (`pfd_ino`/`i`
      against `pfd_w`/`w`, `.unwrap_or(-1)` against an `Option`). It already
      calls `m6_core::server::serve_connection` per connection. The stated
      reason for the copy, needing the watcher fd and the listener in one wait
      set, is a thing `App` already does.
- [ ] **`m6-auth-server` should be an `App` service.** It binds through
      `UnixServer` directly. The only capability it needs that `App` lacks is
      `chmod 0666` on the socket, which wants a config key, not a bespoke main.
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
- [ ] **`ConfigWatcher` has no tests, on any platform.** Found while confirming
      the above. No `#[cfg(test)]` and no `#[test]` anywhere in `watcher.rs`;
      `m6-core/tests/log_reload.rs` covers `LogHandle::reload` and not the
      watcher. Two production consumers, `m6-file/src/main.rs:229` and
      `app.rs:1748`, and config hot reload on Linux has never been exercised by
      a test. So the item above closed "never compiled", not "verified": the
      remaining half is behaviour.
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
- [ ] **Two flaky tests, same shape.** `redirect_lifecycle::sigterm_...` and
      m6-auth-cli's `test_token_create_prints_jwt`. Both spawn external
      processes, both failed exactly once inside a loaded full-workspace run,
      neither reproduces in isolation, and in both cases the assertion text
      was lost to a re-run. Capture the full output of the next failing
      full-suite run *before* running anything else.
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
