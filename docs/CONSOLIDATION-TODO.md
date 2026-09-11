# Consolidation: what is left

Live list. Phases 0-6 of `m6-core-implementation-plan.md` are done; this is
what remains, including things agreed in conversation that were not in the
plan. Tick items off here rather than remembering them.

Ordering principle, the owner's: **m6-core is the PHP of m6, a box of blocks a
service is assembled from.** Anything we can reasonably expect to generalise to
other sites and instances belongs in core, and core should be the only thing a
service links. A default service being nearly a no-op on top of core is the
result we want, not a smell.

---

## 1. Header to dict, dict to header: IN PROGRESS

The asymmetry matters: one needs exposing, the other needs building.

- [ ] **Header -> dict.** The primitives are public (`parse_query_string`,
  `parse_form_body`, `parse_cookies`, `parse_auth_claims`, `url_decode`), but
  the *assembly* is `FrameworkState::build_dict`, a private method bound to
  config and routes. It is where the real knowledge lives: twelve ordered
  steps, and the ordering is load-bearing (built-ins go in *after* params
  files so a params file cannot override them). A service not using `App`
  cannot reuse any of it.
- [x] **Dict -> header.** Done, `3e7a7d8`. `m6_core::cookie` is the one
  `Set-Cookie` formatter, replacing four `format!` calls whose attribute sets
  differed in security-relevant ways. `m6_core::headers` is the accessor and
  writer, including the RFC 9110 5.3 / RFC 6265 3 rule that `Set-Cookie` must
  never be folded into a comma-separated line.

## 2. Document m6-core in full: NOT STARTED

Owner's request. Start with the list of every component available, then detail
each component and its interface. Core is now the only crate a service links,
so this is the reference a new service is written against, and it does not
exist.

## 3. Remaining findings from `CORE-DUPLICATION-AUDIT.md`

Re-checked 2026-09-11 against the current tree. Findings 1, 3, 5, 6 and 10 are
resolved by Phases 1-6. Still live:

- [~] **7. Case-insensitive header lookup.** Half done, `3e7a7d8`.
  `m6_core::headers` now carries `get`, `get_all`, `set`, `append`,
  `set_if_absent`, `remove` and the RFC rule about combining. Core's own sites
  are converted. **About a dozen remain in m6-http** (`cache.rs`, `http2.rs`,
  `redirect.rs`, `security.rs`, `main.rs`), left for a separate pass so a
  mistake in a mechanical change stays isolated.
- [ ] **8. Calendar arithmetic hand-rolled in `m6-md`.** `is_leap`,
  `doy_to_md`, days-since-epoch by hand in `m6-md/src/main.rs`. Core has had
  chrono unconditionally since Phase 6, so there is no dependency argument
  left for keeping it.
- [x] **9. Percent-decoding.** Done, `bafcded`. m6-auth-server's copy deleted;
  core gained `url_encode`, `url_encode_path` and `cookie`.
- [ ] **2. ETag / conditional requests.** `m6-file/src/handler.rs` does its own
  `If-None-Match` comparison next to `core::conditional`. Needs checking
  whether it genuinely differs (strong vs weak comparison) or merely
  duplicates.

## 4. Phase 7, decouple the repositories: NOT STARTED

`dr-grosvenor-site/render-*/Cargo.toml` carries
`m6-core = { path = "../../m6/m6-core" }`: a filesystem layout hard-coded
across a repo boundary with no version constraint, so the site links whatever
is on disk. Replace with a git dependency pinned to a revision.

**Gate:** the site repo builds with no `m6` checkout beside it; `deploy.sh`
stops syncing the `m6` tree and the `touch` workaround is deleted.

This changes the release relationship between the two repos and should be a
recorded decision, not just a commit.

## 5. Phase 8, backend examples: NOT STARTED

Six implementations of the same `/status` payload from
`m6-backend-protocol.md`. Needs Go on the build host.

**This phase is also the measurement.** Rust-without-core against
Rust-with-core is what says whether core is worth linking. If the difference
is small, that is worth knowing.

---

## Not consolidation, but owed and easy to lose

- [ ] **Compile `m6-core/src/watcher.rs` on the build box.** The inotify
  alignment fix in `8bcebac` is inside `#[cfg(target_os = "linux")]` and no
  Linux target is installed on the laptop. It has never been through a
  compiler.
- [ ] **Benchmark Phase 5 and 6.** The plan requires a delta per phase and
  neither has one. The whole request path moved between crates and one virtual
  call per templated response was added.
- [ ] **Deploy `m6-monitor` on the build host and prove it.** Tested, never
  polled a real node. It runs off-fleet: a monitor on syd cannot report that
  syd is down. `deploy/FLEET-MONITOR.md` is the runbook, and the build host
  already reaches all three nodes and already holds the `/perf` token.
- [ ] **A log-target histogram on `/perf`.** Would remove Part A of the hourly
  health check outright, which is the last thing in it that needs ssh apart
  from ufw counts.
- [ ] **Two flaky tests, same shape.** Both spawn external processes and both
  have failed exactly once inside a loaded full-workspace run:
  `redirect_lifecycle::sigterm_shuts_down_rather_than_being_ignored` and
  m6-auth-cli's `test_token_create_prints_jwt`. Neither reproduces in
  isolation, and in both cases the assertion text was lost. Capture the full
  output of the next failing full-suite run before re-running anything.
- [ ] **Retire `tools/health-check.py`.** `m6-monitor --check` replaces every
  part of it except ufw counts, which are the firewall's data rather than
  m6's. It goes once the nodes run a binary carrying `/traffic`.
