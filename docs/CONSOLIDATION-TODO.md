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
- [x] **`80.94.95.211` blocked**: 843 paths, 1,553 requests, got nothing.

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

- [ ] Owner's request, **not started**. Every component listed, then each
      component and its interface. Core is now the only crate a service links
      and there is no reference to write one against. The largest outstanding
      item.

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

- [ ] **Compile `m6-core/src/watcher.rs` on the build box.** The inotify
      alignment fix in `8bcebac` is `#[cfg(target_os = "linux")]` and no Linux
      target is installed here. It has never been through a compiler.
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
- [ ] **The 94.154.46.x operator is still cycling addresses.** `.250` appeared
      on 2026-09-11 running the same spoofed-Googlebot credential sweep, two
      days after `.243`-`.249` were blocked, exactly as BLOCKLIST.md predicted.
      It is not blocked: it was seen once and stopped. Watch for `.251`.

---

## Deployment state

**Nothing from the m6 migration is deployed.** 48 commits ahead of `b32e837`
here, 12 in the site repo. The freeze holds until the migration finishes;
that is the owner's standing instruction.

The production changes listed as done above are deliberate exceptions, applied
on instruction. They change how services are confined and what the firewall
denies, not what code runs.
