# The ledger: what is done, and what is owed

**`HANDOVER.md` is what is true. This is what is owed.** Read that first; it is
written for someone with no prior context.

Status block below rewritten 2026-09-13. The sections after it are the detailed
history, kept because the reasoning in them is usually the only record of why
something is shaped the way it is.

Ordering principle, the owner's: **m6-core is the PHP of m6, a box of blocks a
service is assembled from.** Anything that can reasonably be expected to
generalise belongs in core, and core should be the only thing a service links.

---

## STATUS, 2026-09-17

**Branch state:** `develop` is at `be5456e`. `main` is at `40bd699`, **2 commits
behind develop**, and those two are the only unreleased work:

```
be5456e  m6-http warms its own cache on startup (#63)
551cda3  Run shellcheck, and fix the 16 findings it had (#61)
```

**Production runs 1.7.0** on syd, lon and chi, identical binaries
(`f4e5b0c2b97d92bffb66b10342045259`), verified uniform by `cargo xtask verify-fleet`
against the build artefact. The freeze is long over; releases now go out through
`tag.sh`, which publishes the GitHub release, and the site repo's pin is the only
thing that decides what reaches production.

**1.0 was cut and is five releases behind us.** The road-to-1.0 table that used to be
here is kept below under "The road to 1.0, as it turned out" because the reasoning in
it is still the only record of why some things are shaped as they are.

### What is owed, 2026-09-17

| # | item | issue |
|---|---|---|
| 1 | **Cut 1.8.0.** Two commits sit on develop, and one of them is what lets the deployment delete its `warm-local.sh` and the timer that runs it on every node. Needs a CHANGELOG section first. | #64 |
| 2 | **Log footprint in the report.** Journal disk usage and analytics file size on `/perf`. This is the coverage the retired python health check had and `m6-monitor` does not: on-box loopback TTFB and the TLS split, analytics ndjson size, `journalctl --disk-usage`. Every health check run has to say these are not measured. | #34 |
| 3 | **m6-auth-server path resolution.** `[storage]` resolves against the config file's directory and `[keys]` against the site root, which is two rules for one idea. Not running in production, so there is no live migration. | #16 |
| 4 | **Debian package from CI.** Blocked: needs a GPG key from the owner. | #25 |
| 5 | **Branch protection on `main`.** Confirmed absent 2026-09-17 (`/branches/main/protection` returns 404). `.githooks/pre-push` refuses a direct push, which is a local convention and not enforcement: it protects whoever installed the hook. | #65 |
| 6 | **Four `cargo deny` advisories** whose reachability has never been established. | #3 |
| 7 | **`FrameworkState::build_dict` is private**, so the twelve ordered steps are not reusable by a service not using `App`. §1. | #66 |

### Closed since the last status, and how

Six issues were open on 2026-09-17 with the work already finished and merged to `main`.
`Closes #N` only fires on a default-branch merge, so they had never closed themselves:

| issue | what shipped | first released in |
|---|---|---|
| #26 | `m6-monitor --check` is the health report; the ssh script is gone | v1.0.0 |
| #28 | certificate chain compression (RFC 8879), amplification factor back to the conforming 3 | v1.4.0 |
| #32 | the monitor polls on a schedule and serves the stored snapshot | v1.2.0 |
| #50 | `tag.sh` publishes the release instead of printing a URL to one that does not exist | v1.6.0 |

#60 and #62 stay open: their work is on `develop` and not in `main`, which is a
different case and is now noted on both.

### Items that were "still owed" and are not any more

- ~~`deploy/health-check.py` is not retired yet.~~ **Retired.** Neither
  `deploy/health-check.py` nor `tools/health-check.py` exists. `m6-monitor --check`, or
  `GET /check` over the build host's socket, produces the whole A-to-G report with no ssh
  to any production node. The residual coverage gap is #34, above.
- ~~Staging cannot exercise the cache role.~~ **It can, as of 2026-09-17.** The build host
  runs `m6-http-cache` on `:8443` in front of its own origin, from **production's unit
  file** rather than a staging copy, and the site's 19-check suite passes 19/19 through it.
  Before that, staging wrote its own units and three of five had drifted into being more
  permissive than production, which is the failure mode staging exists to prevent.
- ~~The hourly prompt's `hit_p50_ns` baseline is wrong now that §3a is understood.~~
  **Corrected in the standing order**, which now states that ~3.9us at 50-70 hits is
  expected and that latency is load-dependent, and requires the sample count beside every
  percentile.
- ~~`m6-monitor` and the firewall stats collector are deployed nowhere.~~ Both deployed
  2026-09-15.

### Deferred by the owner, not in 1.0 and still deferred

The **event loop** and the **handler contract**, explicitly. The IO layer is in scope as
low-touch consolidation but is not started. See §3b.

---

## The road to 1.0, as it turned out

Kept verbatim from the 2026-09-13 status block. Items 3 and 5 read as "not started" and
"blocked" and both are **done**: the renderers take m6-core as a git dependency pinned to
a tag (the deployment's `Cargo.toml` pins `v1.7.0`), and the deploy happened, five
releases ago. The rest is accurate and is the only record of the reasoning.

### The road to 1.0, in agreed order

1.0 is **not cut until the consolidation work is done**. Owner's decision,
recorded beside the version in `Cargo.toml`.

| # | item | state |
|---|---|---|
| 1 | **Drive clippy to zero** | **done 2026-09-13, issue #5.** 0 on both toolchains. `tools/clippy.sh` is now `-D warnings` and the two ceiling files are deleted, so there is no number left to maintain. The per-platform ceiling turned out to be per-clippy-version: 45 findings on the laptop at 0.1.95 against 123 on the build host at 0.1.98, identical source, so `--fix` had to run on the build host. Along the way it found three misattached doc comments and one pre-NLL `drop()`. §4 of HANDOVER.md. |
| 2 | **quiche 0.26.1 → 0.29.3, re-measure h3** | **done 2026-09-13, issue #4.** Bumped and re-measured on the build host: h3 stayed at 37/49, unmoved by three releases. 0.29.3 is the newest plain release; the higher tags in that repo are tokio-quiche. The twelve are quiche's and **none is reachable through its public API**, so none is fixable in m6. Ten are open upstream bugs with open fix PRs: **#2515** with PR **#2521** for the eight transport parameter cases, **#2526** and **#2652** with PR **#2575** for the two reserved-bit cases. **h3 is now 47/49 and the floor is 47.** m6-http pins `mgrosvenor/quiche` by revision, branch `m6-h3-conformance`, which is quiche master plus PRs #2521 and #2575. Measured both ways first, tag plus cherry-picks and master plus merges, and both scored 47/49; master was taken as the rebasable base. **Drop the fork for a tag once upstream releases those fixes.** The remaining two are QPACK, which is upstream's choice rather than a bug, and are accepted: owner's decision 2026-09-13, "47/49 is good enough, it's not going to block 1.0.0". Everything, including the rebuild recipe and the one merge conflict to expect, is in `tools/conformance-scores.txt`. |
| 3 | **Renderers onto a git dependency pinned to a tag** | not started. This is Phase 7. Owner's call 2026-09-13: **a git tag, NOT crates.io.** Publishing would mean owning a public API, a name and maintenance for other people. m6-http already takes quiche this way. |
| 4 | **Phase 8: six `/status` implementations** | **done 2026-09-13, issue #6.** Six examples under `m6-http/tests/backends/`, all conforming, 13 shared tests in the gate (7 on the socket, 6 behind a real edge), Go installed on the build host, and `deploy/run-tests.sh` fails if any runtime is missing. The measurement it existed for: **linking m6-core costs 36% of throughput and +37us p50**, plus 8.8x RSS and 56.7x binary size, reproduced within 3%. `docs/BENCHMARKS.md` has the conditions. It also found five places where normative documents and the code disagree, `docs/m6-backend-examples.md` §10; two of those are decisions for the owner, not tasks. |
| 5 | **Deploy, lifting the freeze** | blocked on 1-4 and on the m6-file config/binary sequencing. Not a code task. |

### Done in the 2026-09-12 and -13 sessions

Everything here is on `develop` or on the unmerged CI branch. None of it is
deployed.

| area | what landed |
|---|---|
| **Config-driven routes** | `App::handler(name, f)` plus `handler = "..."` on `[[route]]`, rebuilt on every reload. The owner's *"dynamicly reload the file list"*. Unknown handler name is fatal at startup and refuses a reload. |
| **Both migrations** | `m6-file` and `m6-auth-server` are `App` services. m6-file lost 969 lines including a second router and a second config parser; m6-auth-server's main is 116 lines. |
| **Streaming bodies** | `Response.body` is `Bytes` or `Stream { len, reader }`. A stream structurally has no bytes for the minifier, compressor or default ETag to touch. |
| **Copy elimination** | Core's per-request copying went from ~442us to ~1.58us. `m6-core/src/dict.rs` is a shared base plus a per-request overlay. See `docs/PERFORMANCE.md`. |
| **§3a resolved** | The cache-hit p50 was never a code regression. It is load-dependent. |
| **`watcher.rs` on `nix`** | Zero `unsafe` in production code, down from 390 lines of raw libc. Retires the alignment UB lesson. |
| **Real conformance checks** | h1, h2 and h3 all measured, against a two-backend edge. Previously h2 and h3 were skipped silently on every run. |
| **CI on GitHub Actions** | Every push and PR. Found four real problems on its first three runs. |
| **Branch model** | `main` releases only, `develop` integration, one branch per issue, enforced by `.githooks/pre-push`. |
| **Project hygiene** | rustfmt accepted, `cargo deny`, MSRV 1.88 declared and tested, versions aligned at 0.2.0, CHANGELOG by release, CONTRIBUTING and SECURITY. |
| **§3d decided** | A configured-but-failed bind is fatal; an unconfigured listener is not a bind. |
| **Production hardening** | `UMask=0027`, `LimitNOFILE=65535` (site repo, undeployed). |
| **Documentation** | 16 module docs written, `docs/PERFORMANCE.md`, `docs/LESSONS.md`, `docs/SESSION-NOTES.md`, `CLAUDE.md`. |

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

### 0. Latency: the h3 handshake round trip, and 0-RTT

Added 2026-09-15. Three jobs that belong together, in this order. Recorded here
because the first is a TEMPORARY workaround that must not be allowed to become
permanent, and the third is the line that deletes it.

- [ ] **m6 issue #27 — quiche fork: reject CRYPTO frames in 0-RTT packets.**

      Blocks turning 0-RTT on. All the m6-side work is done and verified on
      staging; this one check in the fork is what holds it back.

      Isolated by running the conformance gate with `cfg.enable_early_data()` in
      and out and changing nothing else:

      | configuration | h3spec |
      |---|---|
      | amplification factor 4, early data OFF | 47/49 PASS |
      | amplification factor 4, early data ON | 46/49 FAIL |

      The single regression is "MUST send PROTOCOL_VIOLATION if CRYPTO in 0-RTT
      is received [TLS 8.3]". RFC 9001 8.3 forbids CRYPTO frames in 0-RTT
      packets; quiche accepts them once early data is enabled.

      Owner's decision, 2026-09-15: fix it in the fork rather than lower the
      floor or abandon 0-RTT. `cfg.enable_early_data()` sits commented out in
      `m6-http/src/main.rs` with the reasoning beside it. **Uncomment it** once
      the fork carries the fix and the gate reads 47/49 with it enabled.

- [ ] **m6 issue #28 — quiche fork: certificate compression, RFC 8879.**

      The proper fix for an extra round trip on every new h3 connection.

      A QUIC server may send only `factor x bytes received` before validating
      the client's address. A 1200-byte client Initial gives a 3600-byte budget
      at factor 3; our handshake flight is 4082 bytes, nearly all certificate
      chain. Measured with `m6-probe-h3` against a 4.85ms RTT, the server sent
      3600, stopped with 482 bytes left, and waited a full round trip.

      We are the ordinary case: Fastly measured 40-44% of uncompressed chains
      exceeding the budget and compression taking it to 1-9%; other work puts it
      at 61% for a 1352-byte Initial. Nothing in that literature proposes
      raising the factor.

      Measured on our own chain, zlib takes it from 3400 to 2345 bytes, saving
      1055 where 482 is needed. Not reachable today: quiche binds 12 `SSL_CTX_*`
      functions and `SSL_CTX_add_cert_compression_alg` is not among them, though
      BoringSSL underneath implements it.

      Cheap and separate: **rustls already supports this for h1 and h2**, behind
      its `brotli` and `zlib` features, which this build does not enable. No
      round-trip win over TCP, just fewer bytes. One line in
      `m6-http/Cargo.toml`.

- [ ] **Then revert the amplification factor to 3.**

      `cfg.set_max_amplification_factor(4)` in `m6-http/src/main.rs` is a
      deliberate, temporary deviation from RFC 9000 8.1, which says MUST NOT
      exceed 3. Owner's decision, 2026-09-15: ship 4 now so production gets the
      round trip back, remove it once compression makes it unnecessary.

      It costs nothing measurable today: h3spec does not test the amplification
      limit, so the gate still reads 47/49, and staging went from ~12.5ms to
      ~6.8ms. The argument for accepting it is that 3x and 4x are the same
      practical outcome for a reflection amplifier, where DNS gives ~50x and
      memcached ~50,000x, and at either factor the attacker burns a third or a
      quarter of the attack on their own upstream.

      **DELETE THE LINE when #28 lands.** It is one line and it is commented as
      temporary in the source.

- [x] **DONE 2026-09-15: raise the amplification factor to 4.** Shipped in 1.2.0.
      lon and chi now complete the h3 handshake in 1.02 round trips where they took
      two, saving ~308ms per new connection from London and ~211ms from Chicago.
      Conformance unchanged at 47/49, measured either side. **This line is deleted
      when #28 lands**, and the source says so.

- [ ] **Put `amplification_limited_count` on `/perf`.**

      quiche already counts "the number of times send() was blocked because the
      anti-amplification budget was exhausted". That is the direct server-side
      signal for this whole class of problem, and it would have identified the
      cause immediately instead of by inference from a client-side timeline. It
      also verifies #28 actually engaged rather than trusting the timing.


### 0a. Packaging: publish m6 as a Debian package, install prod from it

Added 2026-09-15, m6 issue #25. **Filed deliberately unstarted.** Owner's words:
"The package plan needs to be thought through carefully." This changes how
production is deployed, so the design is the work, not the packaging.

- [x] **The five open questions are DECIDED. Owner, 2026-09-15**, recorded in
      full on the issue. In short:

      1. **One package**, `m6`, holding every binary, the core library, docs and
         "headers or whatever the Rust equivalent is".
      2. **Hosted on GitHub Pages**, which settles it as an apt REPOSITORY rather
         than a release asset, so `apt upgrade` works.
      3. **Validation: whatever makes sense.** Taking the safer option: download,
         extract to a temporary directory, `--dump-config` every config on the
         node against the NEW binary, and only then `apt install`. A config the
         new binary rejects is found while the old one still serves.
      4. Apt repo in this repository's `gh-pages`, unless it collides with
         something already published there.
      5. **Backup and deploy model**, below. This is the substantial one.

- [ ] **What m6 must provide for the deployment model.** The owner's target
      shape is: install the m6 package, install the site package, then run one
      deploy command taking a single JSON file of all per-node config and secrets,
      plus the node name. **The model itself, the file layout and the deploy
      command belong in the deployment repository's own docs**, not here, because
      m6 is generic and does not know about any one fleet. This entry exists only
      to record what m6 has to offer so that model can work:

      - every binary must accept its config from a path given on the command
        line, which they already do
      - `--dump-config` must validate without starting, which it already does,
        and is what makes validate-before-install possible
      - nothing may require state that is neither in a package nor in that one
        JSON file. The four loose secret files listed below are exactly what the
        model replaces.

      One warning worth carrying, because it is a property of the design rather
      than of any deployment: **a single JSON holding every production secret for
      every node is a single high-value target.** It needs encryption at rest
      independent of the laptop's disk encryption, and it must sit outside any git
      working tree so it cannot be committed by accident.

- [ ] **Set up the apt repository signing.** `Packages`, `Release` and a detached
      GPG signature. **The signing key's private half must live in GitHub Actions
      secrets, and that step needs the owner at a keyboard** — it cannot be done
      from here. Note also that an apt repo on Pages is **public**: anyone can
      `apt install m6`. That follows from the hosting choice rather than being a
      separate decision.

- [ ] **Decide whether the systemd units ship in the site package.** Left open by
      "whatever makes sense". The argument for: it would have prevented the
      leftover disabled `m6-http-origin` on the cache nodes that aborted a fleet
      deploy on 2026-09-15. The package would ship all units and each node enables
      its own role's.

- [ ] **Build `m6_<version>_amd64.deb` in CI on merge to `main`**, holding the
      seven installed binaries: `m6-http`, `m6-file`, `m6-html`, `m6-md`,
      `m6-auth-server`, `m6-auth-cli`, `m6-monitor`. All four boxes are amd64
      Ubuntu 26.04, so there is one target and `ubuntu-latest` builds it
      natively. No cross-compilation.

- [ ] **A second package in the deployment repository** ships renderers,
      templates, content, assets and units, declaring `Depends: m6 (>= version)`.

      Note the correction that matters: **a `.deb` cannot make the site "build
      against" m6.** The renderers link `m6-core` as a Rust library and Rust has
      no stable ABI, so there is nothing useful to ship for compilation. Build
      time keeps taking `m6-core` from git at the release tag; run time is what
      the dependency expresses, since m6-http serves the site and m6-html renders
      it. Debian's `Build-Depends` against `Depends` says this correctly.

      **A correction to an earlier version of this entry, which was wrong.** It
      said Rust has no stable ABI so there is nothing installable another crate
      can link against. That conflated two different things:

      - **Rust-to-Rust linking** (`rlib`, Rust `dylib`) genuinely has no stable
        ABI: the consumer must be built with the identical rustc and identical
        dependency versions. That is the only part the claim was true of.
      - **`crate-type = ["cdylib", "staticlib"]`** with `extern "C"` and
        `#[repr(C)]` produces an ordinary `libm6core.so` or `.a` with a C ABI,
        which IS stable, and `cbindgen` generates real headers. That is exactly
        "library and headers" in the Debian sense and is completely standard.

      So there are three workable ways to satisfy "the core library and headers",
      not zero:

      **And the identical-rustc point does not rule out an `rlib` either.** One
      CI builds both packages with one pinned toolchain, so "the consumer must be
      built with the same rustc" is satisfied by construction here. That objection
      was raised and correctly dismissed by the owner.

      **What actually decides the shape is a cargo limitation, not an ABI or a
      version one: cargo cannot consume a prebuilt `rlib` as a dependency.** It
      can be linked by driving `rustc --extern m6_core=/usr/lib/m6/libm6_core.rlib`
      by hand, but that means leaving the cargo workflow for the renderers, and
      cargo will otherwise insist on building `m6-core` from source.

      | option | works? | what it costs |
      |---|---|---|
      | vendored `m6-core` source + rustdoc, `[patch]` override, `cargo build --offline` | yes | compile happens locally, which for Rust is normal |
      | prebuilt `rlib` | links fine, but **cargo cannot consume it as a dependency** | abandon cargo for the renderers |
      | `cdylib`/`staticlib` with a C ABI + cbindgen headers | yes, genuinely stable and linkable by anything | an FFI surface to design and maintain |

      **So: ship the source in the package** at something like
      `/usr/share/m6/vendor`, and have the site's build use a `[patch]` or path
      override pointing there with `cargo build --offline`. That delivers the
      actual goal — **no git fetch at build time, and the version tied to the
      installed package** — while staying inside cargo.

      Keep the `cdylib` route for if a non-Rust consumer ever appears, at which
      point it is the right answer.

**What must not be lost, and this is the part a naive version would break.**
`deploy-platform.sh` does work `apt install` does not, and all of it was earned
by something going wrong:

- validates **every** config against the **new** binary before installing it, so
  a config the new binary rejects is found while the old one still serves
- restarts in a fixed order, edges before origin, verifying nothing until every
  unit on the node has restarted
- asserts nested assets actually serve, cache-busted, because a healthy process
  can serve 404s for every asset
- checks the fleet ran byte-identical artefacts at the end

A package changes *how the bytes arrive*. It replaces none of the above, and a
design that quietly dropped them would be a regression that looked like a
simplification.

**Why it is worth doing anyway.** It makes "byte-identical on every box" a
property of the artefact rather than of whoever ran the deploy, which is
currently satisfied only by nobody invoking the script per node. Rollback becomes
`apt install m6=1.0.0` rather than `mv /usr/local/bin/m6-http.prev`. And the
boxes become disposable: with both packages installed, everything on a node is in
a package or rendered from `params/` in git except four files.

    /etc/m6/perf-token
    /etc/m6/cloudns.env
    /etc/m6/auth.pem
    <site>/keys/render-contact-secrets.toml

That is the irreducible per-box state, secrets and nothing else. The build host
is not backed up and the standing rule is that everything done to a node is in
git; this is what would make that literally true.

### 0b. m6-monitor: the four jobs left after it became the report

Added 2026-09-15. `m6-monitor --check` was replaced by `GET /check` in 1.2.0 and
`deploy/health-check.py` is deleted, so these are what remain.

- [ ] **#34: the coverage that went with the old script.** Filed BEFORE deleting
      it, so the gaps are missing on purpose. Two have teeth:

      - **render-contact's SMTP lines.** The old script grepped for `rejected`,
        `message sent` and `SMTP`. The monitor knows render-contact as a unit to
        watch for liveness and never reads its log, so a run of SMTP rejections is
        invisible. This is the contact form, so the failure mode is silent mail
        loss -- and 2026-09-15 showed that path is fragile: it was broken twice in
        an hour, once by a config filter that dropped TOML table headers.
      - **On-box loopback TTFB and its TLS split.** The monitor measures round trip
        from the build host and says so; there is no on-box request-path figure any
        more. Section G replaces the TLS half and improves on it, with real client
        handshakes instead of one synthetic loopback one.

      Also: `journalctl --disk-usage` and the analytics file size, which the
      monitor reports only as a disk percentage; and load generation, which the
      monitor does not do by design, so no loaded-window `hit_p50_ns` can be taken.
      That last one needs a decision, not code: a monitor that generates traffic to
      measure itself is a different kind of tool.

- [ ] **#32 leftovers.** `--check` still exists and still re-polls the fleet to do
      what `/check` does from cache. Remove it once `/check` has been the normal
      route for a release. And `--help` does not list it, because the usage string
      is generated by core's `App` and a service cannot declare its own flags --
      which is why I concluded the flag did not exist and wasted a step.

### 1. Header to dict

- [x] **Closed 2026-09-14, by deciding rather than by changing the API.**

      This said `FrameworkState::build_dict` is private and that a service not
      using `App` cannot reuse any of it. Both halves are still true, and
      neither is a problem any more:

      - **Nothing wants it.** `grep build_dict` across `m6-http`, `m6-file`,
        `m6-html` and `m6-md` returns nothing. The three binaries that do not
        use `App` do not build request dictionaries; they have no templates to
        render against one.
      - **The knowledge is no longer only in the private function.** The
        layering it depended on now lives in `crate::dict`, which is public, and
        the ordering is written down twice: in `app`'s module doc, next to the
        code, and in `docs/m6-core-reference.md`. Step 8 staying after the
        params files is stated as a rule with the reason, in both.

      Making it public to satisfy a caller that does not exist would be an
      interface to maintain for nobody. Reopen this the day a service outside
      `App` needs a dictionary. The dict-to-header half was done in `3e7a7d8`.

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
- [x] **Every module has a module-level doc comment. DONE 2026-09-14.**

      This entry said seventeen modules had none. **Sixteen of the seventeen
      already did**, and had for some time: `compress`, `config`, `error`,
      `http`, `log`, `mime`, `minify`, `multipart`, `parse`, `path`, `request`,
      `response`, `server`, `signal`, `template`, `util` and `watcher` carry
      between 7 and 20 lines each, and they carry the *why* rather than the
      interface, which is what this item asked for. The list was written once
      and never re-read against the source.

      Counted, not estimated: `head -20` on each module's file, `grep -c '^//!'`.

      `app` was the real gap, and it was the worst one to have: one line for
      4,477 lines, and it is the module every service goes through and every
      other module is reached from. It now documents the four builders and why
      there are four, route specificity deciding matches rather than
      declaration order, the base-plus-overlay dictionary and which of the
      twelve steps still run per request, **why step 8 must stay after the
      params files**, thread state, and draining on shutdown. Every claim in it
      was checked against the code rather than against this ledger.

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
- [x] **Calendar arithmetic hand-rolled in `m6-md`. ALREADY DONE; this row was
      stale.** Verified 2026-09-14: `is_leap`, `doy_to_md` and the
      days-since-epoch arithmetic are gone from `m6-md/src/` entirely, and
      `file_mtime_iso` calls `m6_core::util::iso_date_from`. `util`'s own module
      doc records why it moved: the hand-rolled version was wrong for 7,281 days
      out of 29,200, because the era was anchored at 1970 instead of being
      shifted to March, so the last day of every leap year became the first of
      the next and the whole following year was a day late.
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

- [x] **Rewritten on `nix`'s safe wrappers. DONE 2026-09-13.**
      **Zero `unsafe` in the production code**, down from 390 lines of raw
      libc across three `#[cfg]` arms.

      The manual walk over the inotify read buffer is the part worth naming:
      it cast offsets into a `[u8; 4096]` straight to `*const inotify_event`
      and dereferenced them, which is where the unaligned-read UB came from.
      `nix` copies each header into an aligned `MaybeUninit`. `EventBuf`, its
      `#[repr(align(8))]` and the `align_of::<inotify_event>() <= 8`
      compile-time assertion all go with it, **which retires the standing
      lesson attached to them**.

      The macOS arm holds its descriptors in `std::fs::File` rather than raw
      fds with a `Drop` impl, so closing them is the borrow checker's job. It
      gives up `O_EVTONLY`, which is not in `nix`'s `OFlag`: the practical
      difference is that the open descriptor holds the volume against unmount,
      which is not something that happens to a config file's volume under a
      running service. Reaching past the wrapper for that flag is what this
      rewrite exists to stop.

      **`nix` went 0.27 to 0.31, and that is what made the kqueue arm
      possible.** 0.27's `Kqueue` wraps an `OwnedFd` with no accessor, so it
      cannot hand the poll loop a descriptor, and a pollable fd is the whole
      design constraint. 0.31 adds `AsFd for Kqueue`. The upgrade surface was
      two imports across the workspace and cost one change: `PollTimeout` is a
      distinct type rather than a bare `i32`, which is an improvement, since
      -1 for "block forever" and 0 for "return immediately" were two magic
      values in one integer.

      **m6-file and m6-http declared `nix` and used none of it.** Those
      dependencies are gone, so the workspace resolves one version rather than
      two.

      Compiled and exercised on Linux via the build-host gate, which matters
      here more than usual: the inotify arm is `#[cfg]`-gated off on the
      laptop, and it is the one that runs in production.

**Constraint the rewrite must not break:** the watcher exposes a pollable file
descriptor and the *service's own* poll loop waits on it. The obvious crate,
`notify`, spawns a background thread and delivers over a channel, which would
reintroduce exactly what `e6ba278` removed. A wrapper is wanted, not a runtime.

This also retires the standing lesson about the unaligned `inotify_event` read:
that bug existed because the code was doing pointer arithmetic it had no
business doing.

### 3d. A failed bind is a warning, and it should not be

**FOUND 2026-09-12 while fixing the port race. Not changed, because it is a
production behaviour change and the fleet is frozen.**

`m6-http/src/main.rs:3331`:

```rust
Err(e) => {
    warn!(error = %e, "HTTP/1.1 TCP listener bind failed, HTTP/1.1 disabled");
    None
}
```

A server that cannot bind its listener comes up anyway, with the listener set
to `None`. systemd sees a running process, `/health` answers on whatever else
is listening, and **nothing is serving 443**. It is the shape the handover
calls the recurring one: artefact wrong, process healthy, failure deferred and
invisible.

`SO_REUSEADDR` (done, below) removes the most likely *cause*, which was a
restart inside the `TIME_WAIT` window. It does not address the response.

- [ ] **Decide what a failed bind should do.** Exiting non-zero is the obvious
      answer: systemd restarts, the failure is visible, and `Restart=on-failure`
      already exists. The reason this is a decision and not a patch is that the
      same arm may be load-bearing for a node that legitimately runs without
      one of the listeners; that needs checking against `site.toml` for all
      three roles before it changes.

### 3a. Cache-hit p50: RESOLVED 2026-09-13, and it was never a code regression

**It is a load-dependent measurement, not a regression.** The metric tracks how
tightly spaced the requests are, because on a near-idle single-core VM the hit
path goes cold between them.

**The decisive measurement.** Same binary, same node, same counter, one window:

| window | cache hits in it | hit p50 | hit p99 |
|---|---:|---:|---:|
| routine traffic | ~50-70 | **3,900-4,000ns** | 4,200-4,400ns |
| a tight burst | **1,200** | **1,064ns** | 3,782ns |

1,064ns is *below* the 1.7-2.2us band recorded on 2026-09-06 and treated as the
baseline ever since. Nothing was deployed between those two readings; they are
minutes apart on the same process.

**What the measured span actually is.** `hit_p50_ns` is taken at
`m6-http/src/main.rs`, and the timer starts immediately before the cache lookup
and stops immediately after it, *before the response is written*. So it spans
exactly two in-memory operations: `make_lookup_key` and `Cache::lookup_with`.
The `ctx.start` timers elsewhere in that file belong to the **miss** path and do
not feed this number.

**The paired, interleaved A/B that §3a asked for, run 2026-09-13** across
`084f89e`, `438bdb3`, `b32e837`, `22ee3a4` and `06c176d`, five interleaved
rounds, one host:

| commit | lookup p50 |
|---|---:|
| all five | **125ns**, identical |

Flat. And flat across cache size too, which was the other candidate: 125-166ns
from 1 entry to 20,000. So the code in the measured window did not change and
does not scale with what is cached.

**Ruled out along the way**, each cheaply and each read-only: a slow
clocksource (all three nodes are `kvm-clock` at **20.9 ns/call**, measured);
steal time (**0.02%** on syd, load 0.16 on an idle box); an accounting change in
`22ee3a4` (ruled out previously); cache growth.

**Why it looked monotonic across four deploys.** Each reading was taken from
whatever traffic happened to be in the window, and the site is quiet. Density
drifted; the number followed. That also explains the fact the earlier notes
found most puzzling, that the *same binary* read 2.95us on 2026-09-10 and
3.6-3.9us two days later, and it explains why three sessions looking for a
culprit commit found nothing: there is no culprit commit.

**The earlier sessions were half right for the wrong reason.** They concluded
the baseline was wrong; the baseline is not wrong, it is *conditional on load*,
and comparing two readings taken at different request densities compares two
different things. Withdrawing the withdrawal is not the outcome: the deviation
was real and worth chasing, and chasing it is what produced the measurement.

- [x] **Stop treating `hit_p50_ns` as a cross-day regression signal.** It is
      only comparable between windows of similar hit count. The hourly check
      should report the hit count beside it, or the number invites exactly this
      mistake again.
- [ ] **Amend the hourly prompt** so the baseline reads "1.7-2.2us at ~40-70
      hits/window; ~1.0us under sustained load", and so a reading is reported
      with its window's hit count. Not done here because the prompt is the
      owner's.
- [ ] **Optional, if a load-independent number is wanted**: measure the hit
      path with a fixed synthetic burst rather than ambient traffic, which is
      what the burst above does and could be a `--bench` mode on the health
      check.

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

      **It did not work end to end, and this row said it did.** Found and fixed
      2026-09-12 (later session). A capture spanning more than one segment was
      answered **400**, which is every use the feature exists for:
      `/assets/css/main.css` produced `relpath = "css/main.css"`, and step 4 of
      `build_dict` validates every path param with `allow_slash = false`, so
      the slash failed validation before any handler saw it.
      `request::validate_path_param` even carried the reason in its own doc
      comment: "this crate's router has no catch-all support ... a parameter
      here captures exactly one path segment and can never contain a slash."
      That was true when it was written and `Segment::Wildcard` made it false.
      The fix is `validate_wildcard_param` plus `CompiledRoute::is_wildcard_param`,
      so which validation applies is decided by the **route** rather than by
      the parameter's name, which is the same mistake the old m6-render code
      made in the other direction by exempting anything called `relpath`.
      Traversal is still refused: `..`, a leading or trailing slash, and every
      character outside the set are unchanged, so what a wildcard gains over an
      ordinary parameter is the separator and nothing else.

      **Why six passing tests did not catch it:** all six stop at
      `match_route`. The matcher was right; nothing took a capture through to
      the wire. Lesson 30 in the handover is the general form and this is
      another instance of it.
- [x] **Streaming response body. DONE 2026-09-12 (latest session).**
      `Response.body` is now `Body`, a sum type of `Bytes(Vec<u8>)` and
      `Stream { len, reader }`, and `Response::send` dispatches a stream to
      `Responder::send_stream`. `Response::stream(status, len, reader)` builds
      one. `Response::send` takes `self` by value, because a stream owns its
      reader and a response goes on the wire once.

      **A sum type rather than a `Vec<u8>` beside an optional reader**, because
      the difference is load-bearing: `as_bytes()` is `None` for a stream, so
      minification, compression and the default content-hash ETag have nothing
      to act on and cannot silently run against empty bytes sitting next to the
      real body. Structural, not a flag.

      Alongside it, `Response::verbatim()` for the buffered case: a handler
      that has already negotiated the coding and built an ETag naming that
      representation. Core skips minify and compress for it. m6-file needs this
      because re-compressing its output downstream would put brotli bytes on
      the wire under a tag asserting identity, which is what its `-br`/`-gz`
      suffixes exist to prevent.

      **This row said "NOT a blocker for m6-file's migration" and it was
      wrong.** The claim was written at 14:32 on 2026-09-12 (`a979390`,
      "checked against the source") and 23 minutes later `8c79ee7` gave
      m6-file `send_stream` for its identity path. Both entries were true when
      written; nothing reconciled them, and the migration row inherited the
      stale one. Migrating onto a byte-only `Response` would have put the
      3.6MB-per-miss `fs::read` back on the service that serves every asset.
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

- [x] **`m6-file` IS an `App` service. DONE 2026-09-12.** Its own `poll(2)` accept loop
      was 22 of 33 lines identical to `App`'s; that block is now
      `server::poll_listener_and_watcher` and both call it. It already uses
      `server::serve_connection` per connection.

      **Wildcard routing landed 2026-09-12, and streaming was never a blocker**
      (it buffers everything, see above).

      **The route-reload blocker is gone as of 2026-09-12 (later session).**
      `App` now grows config-driven routes that survive a reload, which was the
      owner's instruction (*"And dynamicly reload the file list."*). See §6
      below for the shape. **Streaming and `verbatim` landed the same session**,
      so a handler can now serve a file without materialising it and without
      having its negotiated representation re-encoded downstream.

      `main.rs` is 34 lines and one of them is
      `App::new().handler("files", handler::serve).run()`. **969 lines went**:
      `route.rs` (a second router), `config.rs` (a second config parser),
      `http.rs`, and the CLI parsing, logging setup, socket bind, accept loop,
      worker pool, graceful drain and reload handling in `main.rs`. All had an
      equivalent in core, and most had been extracted from here in the first
      place.

      **The end-to-end reload proof now exists**, which is what the core-side
      work could not provide on its own: `m6-file/tests/dynamic_routes.rs`
      writes a config, lets the watcher fire, and gets a 200 on a path that
      404'd a moment earlier. It also pins the reverse (a removed route stops
      serving) and the refusal (a reload naming an unregistered handler is
      rejected and the previous routes keep serving).

      **Four behaviour changes, none of them silent:**

      1. **Traversal answers 404 everywhere, where an ordinary parameter used
         to answer 400.** m6-file was inconsistent: 404 for `..` in a
         catch-all, 400 for the same `..` in a single-segment parameter,
         because the two went down different arms of its matcher. Core now
         splits on the *reason*: `PathParamError::Traversal` is 404,
         `InvalidChars` is 400. 404 is the disclosure-safe answer and is what
         m6-file's own spec (§l2) documented; a merely malformed value gives
         nothing away by being named as malformed.
      2. **A burst sheds instead of queueing.** m6-file fed a fixed worker set
         through an **unbounded** `mpsc` channel; `App` uses a bounded pool and
         answers 503 when the queue is full. The bounded form is the better
         design, but it has to be sized: production's `size = 32` gives
         `queue_size = 256` by default, against a gallery page that fires
         dozens of concurrent images. `l5_concurrent_requests` fires 100 and
         needed the fixture sized to match.
      3. **Core supplies `Cache-Control` as a default, not an override.** It
         used to append unconditionally, so a handler that set its own would
         have put two on the wire. m6-file sets its own per request, because
         `?v=` is `immutable` and everything else is a short window.
      4. **Config format.** All 15 production routes gained `handler = "files"`
         and `/assets/{relpath}` became `/assets/{*relpath}`. The wildcard has
         to be explicit because m6-file's matcher made a bare trailing
         parameter greedy and core's does not; that difference is why the core
         wildcard defect had to be fixed first. Verified by serving the real
         production config against the new binary: 15 routes, 32 threads,
         assets 200, missing file 404.
### 7. Copy costs: the audit, and the jobs to reach zero

**OWNER'S INSTRUCTION, 2026-09-12:** *"I think we need a full audit of copy
costs throughout the system."* *"The right number is zero."* *"Things like
config should obviously be a read only reference throughout."* *"Let's make App
clean and robust and copy free."*

The goal is **zero**. Where practical reality imposes a floor, the floor is
named and measured rather than folded into the claim.

#### The audit

Audited `m6-http` (edge, cache, forward), `m6-file`, `m6-core`'s `App` request
path, and the four `App` services. Release, laptop, production input.
`m6-core/src/app.rs`'s `copy_audit`:

```sh
cargo test --release -p m6-core copy_audit -- --ignored --nocapture
```

**Two of the three services already do it right, and the framework they are
meant to migrate onto is the one that does not.**

| crate | per-request copying | verdict |
|---|---|---|
| `m6-http` | `CachedResponse` is `Arc<Vec<(String,String)>>` headers and `bytes::Bytes` body, so a cache hit is a refcount bump. No clones on `forward.rs`'s hot path. | **already zero** |
| `m6-file` | `Arc<Config>` and `Arc<Vec<Route>>` per connection as refcounts; per-request is a root `String`, param names, an ETag. | **already near zero** |
| `m6-core` `App` | below | **~0.63ms per page** |

Serving one HTML page with the real `data/content.json` (68KB, 1,364 nodes):

| copy | ns | job |
|---|---:|---|
| `route.clone()` | 167 | J6 |
| `site_dir.clone()` x2 | 83 | J1 |
| `config.compression.clone()` | 208 | J1 |
| `config.minification.clone()` | 167 | J1 |
| `build_dict`, three copies inside | 222,708 | J2, J3, J4 |
| `raw.clone()` into `Request` | 458 | J6 |
| `dict.clone()` into `Request` | 108,583 | J3 |
| `dict.clone()` in `render_response` | 109,500 | J5 |
| `tera::Context` build, in the engine | 189,000 | J7 |
| **total per request** | **630,874** | |

**`content.json` is deep-copied six times per request** and none of it varies
between requests: it is the config file and the content file, identical until
the next reload. m6-html renders every page on the site this way, on a 1-core
VM, at a stated ~6ms.

It is **not** an explanation for §3a, which is m6-http's cache-hit p50 and
never reaches m6-html. Do not conflate them.

#### The jobs

Ordered so each stands alone and the cheap, no-risk ones land first.

- [x] **J1. Config by reference, never by value. DONE.** `Arc<RendererConfig>` in
      `FrameworkState`; the pipeline and handlers borrow it. Removes the
      `compression`, `minification` and `site_dir` rows outright. No type
      change outside core. *The owner's stated rule, and the cheapest job here.*
      **458ns of deep copies became 42ns of refcount bumps.** `FrameworkState`
      holds `Arc<RendererConfig>` and `Arc<PathBuf>`; the read lock hands out
      `Arc` clones, and the pipeline borrows from them.
- [x] **J2. A per-route base dict, built once per reload. DONE.** Steps 1 to 3 of
      `build_dict` produce the same map for every request on a given route:
      config keys, global params, static params files. Precompute it per
      `CompiledRoute` as an `Arc<Map>` at `FrameworkState::build` time.
      **Done**, as `CompiledRoute::base_dict`. Routes are keyed on their static
      params-file list and share the merged result, so fifteen routes over one
      content file hold one copy rather than fifteen.
- [x] **J3. A layered `Dict`: `Arc` base plus a per-request overlay. DONE.** `get`
      checks overlay then base; only the overlay is ever built per request, and
      it holds a handful of entries. Removes the two big `dict.clone()` rows as
      well, because a `Dict` clone is an `Arc` bump plus a small map.

      **The twelve-step precedence survives, and this is the part to get
      right.** Built-ins (step 8) live in the overlay and params files in the
      base, so built-ins still win, which is what step 8's comment calls
      load-bearing. Dynamic params files, those whose path holds a
      `{placeholder}`, resolve per request and so go in the overlay *ahead of*
      the built-ins, preserving their order too. **Write the precedence test
      first**: a params file trying to override `year`, `datetime` and
      `request_path` must still lose.

      **Done**, as `m6-core/src/dict.rs`. The precedence test was written
      first and is `a_base_entry_can_never_override_an_overlay_one`; a
      params file trying to set `year`, `datetime` or `request_path` still
      loses. `Request`, `Response::template_dict` and the `Renderer` seam all
      take a `Dict` now, and `From<Map> for Dict` keeps every caller that hands
      core an owned map working unchanged.

      **`build_dict` went from 222,708ns to 1,125ns. The `Request` clone went
      from 108,583ns to 542ns.**
- [x] **J4. Do not merge the same params file twice. DONE.** Steps 2 and 3 both
      insert `data/content.json`, because the production config names it as
      `global_params` *and* as the route's `params`, and the second pass
      overwrites the first with identical values. Fixed in the base-dict
      build: a static params file already merged as a global param is skipped.
      **Still worth asking separately whether the config needs to say it
      twice**, which is a site-repo question and is not answered here.
- [x] **J5. DONE.** `render_response` cloned the dict to merge a handler context that
      was usually absent, and for a config template route the context it merged
      was a copy of the very dict it had been passed: the dict was cloned,
      merged into itself, and thrown away. `template_dict` now means "a context
      this response supplied"; `None` means "render against the request's own
      dictionary", which the service loop already holds.
      **109,500ns became zero.**
- [x] **J6. The small rows. DONE.** `route.clone()`, `raw.clone()` and the two
      `site_dir.clone()`s were ~900ns together, and both turned out to be
      borrows dressed as copies rather than anything inherent:

      - **The route table is shared** (`Arc<Vec<CompiledRoute>>`), so routing
        now happens *outside* the read lock and returns a borrow. The matched
        route was previously cloned out of the guard for no reason other than
        to outlive it.
      - **`serve_connection` hands the request to its handler** rather than
        lending it. It has no use for the request afterwards, and the only
        thing stopping the move was `Responder` holding `method: &'a str` when
        the sole use of it was `eq_ignore_ascii_case("HEAD")`. The responder
        now stores that decision, the lifetime tie goes, and every `App`
        service stops copying the method, path, query, every header and the
        body once per request. `Request::into_raw` gives the loop the request
        back for the coding negotiation, the cookie checks and the access log.
- [~] **J7. Tera's own copy. CLOSED AS ACCEPTED, owner's call 2026-09-12:**
      *"Don't bother with tera. That's just how it is."* Recorded rather than
      struck out, because the number is worth knowing and the reasoning should
      not have to be rediscovered.

      **It is now 99% of what remains, and the proportion is worse than it
      looks.**
      `tera::Context::insert` calls `to_value`, which deep-copies each value
      into the engine's own `BTreeMap`. **143,417ns of the remaining
      145,001ns.**

      Measured against the render itself, on a minimal template, which is the
      case *most* favourable to the copy being negligible:

      | | ns |
      |---|---:|
      | context build | 178,875 |
      | build + render | 179,625 |
      | **render alone** | **750** |
      | **context share** | **99.6%** |

      The cost is proportional to the size of the context, not to what the
      template reads: a page touching three keys still pays to copy all 1,364
      nodes of `content.json`. Three ways out existed, none of them free:

      1. **A per-route `tera::Context` prebuilt once per reload**, with the
         request's overlay inserted before the render and removed after.
         Zero copies of the base. Costs a lock per route, which serialises
         concurrent renders of the same page; on a 1-core origin that is close
         to free, and under the single-threaded target shape it is free, but it
         is contention on the current thread pool. A thread-local per
         (route, reload generation) avoids the lock at the cost of one copy per
         thread per reload rather than per request.
      2. **Patch or fork Tera** so a context can borrow. Upstream's `Context`
         owns a `BTreeMap<String, Value>` and every entry point copies into it.
      3. **A different engine**, one that renders against a borrowed context.

      Option 1 is the only one that takes on no dependency problem, and its
      costs are real: a lock per route serialises concurrent renders of the
      same page, and the thread-local variant that avoids the lock pays
      threads x routes x context size in memory, which on a 950MB origin is
      not nothing. **The owner's call is to leave it.** If it is ever
      reopened, the cheaper lever is probably on the site side rather than in
      core: the whole content file is in every page's context because the
      config puts it there.

#### Where it stands: J1 to J6 done, J7 accepted

Same measurement, same input:

| copy | before | after |
|---|---:|---:|
| config + `site_dir` out of the read lock | 458 | **42** |
| the matched route | 167 | **42** |
| `build_dict` | 222,708 | **1,125** |
| `raw.clone()` | 458 | **0** |
| `dict.clone()` into `Request` | 108,583 | **375** |
| `dict.clone()` in `render_response` | 109,500 | **0** |
| `tera::Context`, in the engine | 189,000 | 143,417 |
| **total** | **630,874** | **145,001** |

**Core's own copying: ~442,000ns to ~1,584ns, a factor of 280.** Everything
that remains is inside Tera, which is closed as accepted.

`App` no longer copies anything that does not vary between requests.

#### The rule this leaves behind

Immutable state is shared, never copied. Anything that does not vary between
requests belongs behind an `Arc` and is read through a reference: config,
params files, routes, templates. A per-request allocation has to earn itself by
holding something that genuinely differs per request.

### 4. Phase 7, decouple the repositories

- [ ] The site's renderers carry `m6-core = { path = "../../m6/m6-core" }`: a
      filesystem layout hard-coded across a repo boundary with no version
      constraint. Replace with a git dependency pinned to a revision.
      **Gate:** the site builds with no `m6` checkout beside it, and
      `deploy.sh` stops syncing the tree.

### 5. Phase 8, backend examples

- [x] Six implementations of the same `/status` payload. **DONE 2026-09-13**,
      issue #6. Go installed on the build host (1.26.0) and on the laptop
      (1.27.1). All six conform, 13 shared tests run them in the gate, and
      `deploy/run-tests.sh` fails if a runtime is absent rather than skipping a
      language silently.

      **The measurement:** linking m6-core costs **-36.8% throughput and
      +38.5us p50**, with 8.8x resident memory and 56.7x binary size, reproduced
      within 3% across two runs at concurrency 2 from tmpfs. §5.3 of the examples
      doc says that if the delta is not close to zero then core has a problem
      worth knowing about. It is not close to zero, AND it is measured on the
      shape that maximises it: behind the edge cache most requests never reach a
      backend. `docs/BENCHMARKS.md` carries the conditions.

      The first attempt reported core as 72% FASTER, because the control spawned
      a thread per connection while core used a pool. A control that differs from
      its subject in two ways measures neither.

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
- [ ] **Deploy `m6-monitor`.** **Verified 2026-09-12: installed on no machine
      at all** — not syd, lon, chi, nor the build host. This entry used to say
      it "has now been run against the real fleet from the laptop and works"
      while `HANDOVER.md` said it "has never polled a real node". The two
      disagreed, neither had been checked, and the checkable part is that
      nothing is installed anywhere. `deploy/FLEET-MONITOR.md` is the runbook.
      The build host is off-fleet, so installing it there breaks no freeze.
- [x] **Deploy the firewall stats collector. Done 2026-09-15.** Installed on
      syd, lon and chi via `deploy/install-firewall-stats.sh`, so `/traffic` now
      carries real `firewall` data instead of `null`. m6-monitor renders it as of
      `f3fff92`, which it had not before because until the collector existed
      every node returned `null` and there was nothing to print.
- [ ] **Retire the deployment's `deploy/health-check.py`** (it was
      `tools/health-check.py` here until 2026-09-14). Owner's instruction,
      2026-09-15: "I want it on the todo list to retire the old script."

      **The two blockers this entry used to name were both stale and are gone.**
      It said `/traffic` 404s and `/perf` carries no `pools` field, measured on
      syd 2026-09-12. Re-measured 2026-09-14 on all three nodes: `/traffic`
      answers **200** and `/perf` **does** carry `pools`. Both need
      `Authorization: Bearer` from `/etc/m6/perf-token`; a token passed as a
      query parameter returns 401, which reads exactly like a broken endpoint and
      is probably how the original reading was taken.

      A third claimed blocker was also wrong: that only the ssh script could see
      the config-reload logging defect, because `journalctl` is not exposed over
      HTTP. `m6_core::monitoring::LoggingHealth` measures it **in-process**, which
      is strictly better, and `PulseLayer` sits inside the reloadable filter so a
      reload that silences the main layer stops the pulse. That claim reached the
      owner's standing health-check prompt from `docs/OPERATIONS.md` in the
      deployment repository, so it cost more than a wrong sentence.

      **What genuinely remains**, tracked in m6 issue #26:

      - [x] render the firewall section — `f3fff92`
      - [x] cache-header assertions from the deployment's own declaration — `f3fff92`
      - [x] per-channel handshake timing published by m6-http — `d56cedc`, split by
            resumption in the commit after it
      - [x] m6-monitor prints the handshake figures — section G
      - [ ] **deploy a post-1.0.0 m6-http to the nodes**, or section G reports
            "no handshakes recorded" because the old binary does not publish them
      - [ ] **compare the two reports field by field on one window** and record
            what only the ssh script can still see. This is the step that
            justifies retirement rather than assuming it.
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
`the deployment repository/docs/RELEASES.md` names. Recompute from it; the previous
figure here named `b32e837`, which the 2026-09-10 18:47 deploy had already
superseded.

The production changes listed as done above are deliberate exceptions, applied
on instruction. They change how services are confined and what the firewall
denies, not what code runs.
