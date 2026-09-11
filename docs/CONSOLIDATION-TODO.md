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

The owner's standing requirement: **every app has the same general structure,
and that structure is documented well enough to pick up from outside the m6
repo.** `docs/m6-app-anatomy.md` is the document. Two services still do not
match it, and in both cases the divergence is historical rather than designed.

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

**Nothing from the m6 migration is deployed.** 69 commits ahead of `22ee3a4`
here, 14 ahead of `d6ebfa5` in the site repo. The freeze holds until the
migration finishes; that is the owner's standing instruction.

The deployed commit is whatever the newest entry in
`~/dr-grosvenor-site/docs/RELEASES.md` names. Recompute from it; the previous
figure here named `b32e837`, which the 2026-09-10 18:47 deploy had already
superseded.

The production changes listed as done above are deliberate exceptions, applied
on instruction. They change how services are confined and what the firewall
denies, not what code runs.
