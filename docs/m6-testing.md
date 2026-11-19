# Testing m6

Four layers check m6, and each layer has a different judge.
Section 1 names the four, section 2 gives the commands, section 3 describes the shared test harness, section 4 covers the conformance testers, section 5 lists the integration suites, and section 6 shows the end-to-end run.
Section 7 states what a green run covers.

## Contents

1. [The four layers](#1-the-four-layers)
2. [Running the checks](#2-running-the-checks)
3. [The shared harness](#3-the-shared-harness)
4. [Conformance: h1spec, h2spec and h3spec](#4-conformance-h1spec-h2spec-and-h3spec)
5. [Where the tests are](#5-where-the-tests-are)
6. [End to end](#6-end-to-end)
7. [Summary](#7-summary)

## 1. The four layers

Table 1 gives the four layers.
Only the first runs fully on a laptop.

| layer | what it proves | where it runs |
|---|---|---|
| unit and integration tests | m6's code does what m6 intends | laptop, build host, *CI* (continuous integration) |
| conformance, h1spec h2spec h3spec | m6 does what the *RFC* (Request for Comments) documents say | build host and CI, needs the testers installed |
| the examples | m6's interfaces work for code outside m6 | build host and CI, built from `examples/` in the same checkout |
| performance | a change did not cost latency | build host, needs a quiet machine |

**Table 1: the four layers, what each proves, and where each runs.**
A layer runs only where what it needs is present.

Three testers nobody on m6 wrote judge conformance.
A test written by the author of the code repeats the author's misreading of an RFC in both places and passes.

## 2. Running the checks

Table 2 gives the commands.
Only the last one runs everything.

| command | what it runs |
|---|---|
| `cargo test --workspace --no-fail-fast` | every unit and integration test |
| `./check.sh` | dev build, clippy, tests, formatting, conformance, benchmarks |
| `./check.sh --no-bench` | the same without the benchmarks |
| `tools/conformance.sh [h1\|h2\|h3\|rfc\|resume]` | one stage, or all of them with no argument |
| `M6_BUILD_HOST=user@example.com ./tools/build-host-tests.sh` | all of it, on Linux, with the testers installed |

**Table 2: the five commands, and what each one runs.**
`cargo test` is the loop while writing code, `check.sh` is the local gate, and `build-host-tests.sh` gates a merge, because it adds the performance check, the examples and conformance without `--allow-missing-tools`.

**On Linux, raise the memlock limit for your account before running them.**
m6-http pins its memory at start (`[server] lock_memory`, on by default), and the pin needs an unlimited soft `RLIMIT_MEMLOCK`.
Without it every test that starts m6-http stops at start with an error that says `Pinning memory needs RLIMIT_MEMLOCK unlimited` and exits 2.
Add two lines to a file under `/etc/security/limits.d/`, replacing `you` with the account, and log in again:

```
you  soft  memlock  unlimited
you  hard  memlock  unlimited
```

CI runs `sudo prlimit --pid $$ --memlock=unlimited:unlimited` at the top of each Linux step that starts m6-http, and every process the step's shell starts inherits the limit.
macOS has no pin, so it needs nothing.
`lock_memory_with_too_low_a_limit_exits_2_naming_the_limit_and_the_fix` in `m6-http/tests/startup_errors.rs` lowers the limit itself with `ulimit -l 0` in the shell that starts m6-http.
`a_warm_server_is_resident_within_its_reservation` in `m6-http/tests/heap_resident.rs` measures a warmed m6-http's resident set against its start-up size plus its configured reservation, and counts its minor page faults over 1,000 cache hits.

Three rules apply to every command in Table 2:

- **Tests share one machine.**
  The suites that start a service claim a free loopback port with `testkit::claim_port` and hold it for the service's lifetime, so they run on the harness's default threads.
  A suite that binds a fixed port, or two test runs at once on one host, produces 502s and bind failures.
- **Run the gate on the build host.**
  The performance check measures latency, and a concurrent build moves its readings.
  The conformance testers are installed on the build host.
- **A check that cannot measure must fail.**
  `tools/conformance.sh --allow-missing-tools` reports an absent tester as NOT TESTED and says the run proves nothing about that protocol.
  `check.sh` passes that flag, then ends on `Checks INCOMPLETE` and `This run is not a gate` when a tester was absent, so a laptop run tells you nothing about h2 or h3 until the last line says `All checks passed.`
  `build-host-tests.sh` runs without the flag, so the path to a release cannot skip h2 or h3.

## 3. The shared harness

Every integration suite does the same four things: find a free port, find the binary cargo built for this run, start a service and wait for it to be ready, then read a response at the byte level.
`m6-core::testkit` holds five modules that do them once, plus `alloc_count` and `check`.
Table 3 gives the five.
A suite that does any of this itself reintroduces a failure the module closes.

| module | provides | the failure it closes |
|---|---|---|
| `paths` | `binary`, `exec_scratch_root` | a suite guessing where the binary is, and a test that builds a release binary the run does not need |
| `port` | `claim_port`, `PortClaim` | `TcpListener::bind(":0")` read back after the listener drops, which frees the port before the child claims it |
| `process` | `Service`, killed on drop, stdout and stderr both drained | a child that logs more than one pipe buffer blocks in `write`, and a dead child's last output goes with the pipe |
| `wait` | `for_tcp`, `for_path`, `for_unix`, `until` | a fixed sleep, long enough on an idle laptop and too short when a dozen stacks start at once |
| `response` | `read_one`, `read_response` | each suite parsing responses at the byte level its own way |

**Table 3: the five testkit modules, and the test failure each one closes.**
Each failure in the third column shows up as something else: a routing bug, a hang, or a 502.
`alloc_count` is the counting allocator behind the allocation tests, and `check` is a stand-in auth check server for tests.

The harness is behind the `testkit` feature of m6-core, off by default and enabled in `dev-dependencies`, so none of it reaches a production binary.

Two rules come with it:

- **Hold a `PortClaim` as long as the service holds the port.**
  Dropping it early reopens the race it closes, which is why it is `#[must_use]` and stored beside its `Service`.
- **`Service` drains stdout as well as stderr**, because m6's services log to stdout.

**`binary` never runs cargo.**
It returns a binary of the test's own package from `CARGO_BIN_EXE_<name>`, and a binary of another workspace package from the profile directory next to the running test executable, where `cargo build --workspace --bins` puts it.
It panics with that command when the binary is missing.
The gate builds `cargo build --workspace --bins` in the dev profile before `cargo test`, so every binary a test needs is present.
The scripts do not use `binary`.
`tools/conformance.sh` runs the dev-profile binaries in `target/debug` (`M6_BIN_DIR` selects another directory) and fails when they are missing, so run `cargo build --workspace --bins` before it.
`tools/perfcheck.sh` measures `target/release/m6-html` by default (`--cand-bin` selects another), so run `cargo build --workspace --release` before it.
`bench.sh` builds the release binaries it needs itself.
`bench.sh --handshake` measures handshakes only, and its header documents `BENCH_KEY` and `BENCH_FEATURES`.

### 3.1. The periodic-tick test hooks

`m6-http` writes its accounting stats every ten seconds (`[stats] interval_s`).
Suites such as `cache_accounting` and `disk_cache` spawn the real binary and set `M6_TEST_STATS_INTERVAL_SECS` to shorten the tick, because a `#[cfg(test)]` default cannot reach a spawned process.
`loop_never_waits` sets `[stats] interval_s` in its `site.toml` instead, because it asserts on wakes per report and must measure the same report in a release run.

A debug build reads `M6_TEST_STATS_INTERVAL_SECS` in `initial_stats()` in `m6-http/src/main.rs`.
It uses that many seconds for both the stats and traffic intervals when the value parses as a positive integer.
Exporting the variable before running a debug `m6-http` by hand shortens that process's tick, and every metrics line's trace window with it.
A release build does not read the variable.

The loop's once-a-second accounting tick is a second period, and it has its own hook.
That tick flushes the log buffers, checks the heap's growth and reads one host source.
A source's reading allocates, and how much depends on the source and the platform.
A test that counts allocations over a run therefore sets `M6_TEST_ACCOUNTING_TICK_SECS` to a value past the run, which keeps the tick out of the window: the counts then hold the requests alone, on a runner of any speed.
`allocations` and `alloc_attribution` do that.
`test_accounting_tick()` in `m6-http/src/main.rs` reads the variable, the value is seconds when it parses as a positive integer, and a release build does not read it.

## 4. Conformance: h1spec, h2spec and h3spec

Three independent testers check m6 against the RFC documents.
Table 4 gives them.
HTTP/1.1 has four targets because the workspace has four HTTP/1.1 implementations, and HTTP/2 and HTTP/3 live in m6-http alone.

| tester | source | covers | targets |
|---|---|---|---|
| h1spec | `github.com/dropseed/h1spec` | RFC 9112 and 9110 | m6-auth-server, m6-file, m6-html, m6-http's redirect listener |
| h2spec | `github.com/summerwind/h2spec` | RFC 9113 | m6-http |
| h3spec | `github.com/kazu-yamamoto/h3spec` | RFC 9114 and QUIC | m6-http |

**Table 4: the three conformance testers and what each is pointed at.**
One tester covers four services, and each of the other two covers one.

### 4.1. The script

`tools/conformance.sh` writes a fixture site, starts a loopback instance, runs the tester, parses the score from the tester's own summary line, and compares it to the floor in `tools/conformance-scores.txt`.

```sh
cargo build --workspace --bins        # the script runs binaries from target/debug
tools/conformance.sh                  # all of it
tools/conformance.sh h2               # one protocol
```

A run below its floor fails.
A run above it prints the new number and asks for the floor to be raised by hand, because a raised floor claims the improvement is permanent and belongs in the commit that earned it.
A target in the scores file that a full run did not measure fails the run.

The script also runs m6's own RFC tests, for the behaviours an external tester cannot see: a backend's header block split across HTTP/2 frames, the field names m6 writes on HTTP/2 and HTTP/3, `Content-Length` as `1*DIGIT`, an `If-None-Match` 304, a backend that answers before it has read the whole request, and the `Cache-Control` and conditional-request rules.
Every test names its RFC section, and `tools/conformance.sh rfc` runs that stage alone.

### 4.2. By hand

Run a tester by hand to diagnose a failure the script has reported.

```sh
h2spec -h 127.0.0.1 -p 8443 -t -k --timeout 5
h3spec -n 127.0.0.1 8443
```

`-t` runs h2spec over *TLS* (Transport Layer Security), `-k` accepts the loopback certificate, and `-n` skips h3spec's certificate name check.

Table 5 gives the five traps.
Four of them make a healthy m6 look broken, with no error to warn you.

| # | trap | what it makes a reader believe |
|---|---|---|
| 1 | h3spec without `-n` | every HTTP/3 test failed, on a certificate name mismatch |
| 2 | the rate limiter left on | a wall of HTTP/2 failures, because h2spec opens a connection per test and the limiter is what gets measured |
| 3 | counting h2spec failures with `grep -c` | double the real number, because the tool redraws each failure line (98 counted is 49 real) |
| 4 | `grep` without `-a` on h2spec output | nothing, because the output carries control bytes and GNU grep says "binary file matches" |
| 5 | pointing a tester at a live site | a score nobody else can reproduce |

**Table 5: five traps in running h2spec or h3spec by hand, and what each one makes a reader believe.**
Trap 1 is a missing flag, trap 2 a configuration default, traps 3 and 4 are output misreadings, and trap 5 is a wrong target.

Two of them need more than a row:

- **Trap 2 is set by a configuration default.**
  `tools/conformance.sh` writes `[rate_limit] enabled = false` into its fixture, and a hand-run instance needs the same, or `requests_per_min` raised and restored after.
- **Trap 5 risks more than the reading.**
  h2spec deliberately sends malformed frames, and a result measured against a private live site cannot be reproduced by anyone else.

### 4.3. Without debug assertions

Every check above builds the dev profile, where a `debug_assert!` argument runs, and a release build compiles that argument out.
A write inside a `debug_assert!` therefore passes every test and vanishes from the shipped binary: HTTP/2's frame writer did that, and the release build of m6-http sent no HTTP/2 frame.
Two checks close that gap.

- **`m6-core/tests/debug_assert_purity.rs`** scans every Rust source in the workspace and fails when a `debug_assert!`, `debug_assert_eq!` or `debug_assert_ne!` argument calls a mutating method such as `put`, `push`, `write` or `send`.
- **The `release-asserts` profile** in the workspace `Cargo.toml` is the dev profile with `debug-assertions = false`. CI's conformance job builds it and runs the HTTP/2 unit tests and h2spec against it, at dev-profile build speed:

```sh
cargo build --workspace --bins --profile release-asserts
cargo test -p m6-http --lib --profile release-asserts http2
M6_BIN_DIR="$PWD/target/release-asserts" ./tools/conformance.sh h2
```

The scan catches a known mutating call, and the profile run catches whatever the scan's list misses.

## 5. Where the tests are

Table 6 gives a selection of the integration suites, by crate.
A behaviour in the table traces to the file that checks it, and each `tests/` directory holds more suites than the table lists.

| crate | integration suites |
|---|---|
| m6-core | `pre_push_hook.rs`, `version_flag.rs`, `startup_record.rs`, `startup_secrets.rs`, `secret_fields.rs`, `debug_assert_purity.rs` |
| m6-http | `integration.rs`, `integration_tests.rs`, `security_e2e.rs`, `security_regressions.rs`, `robustness.rs`, `edge_proxy.rs`, `analytics_e2e.rs`, `backbone_flow_control.rs`, `cache_accounting.rs`, `scale_reuse_port.rs`, `backends_contract.rs`, `backends_through_proxy.rs`, `redirect_lifecycle.rs`, `tls_resumption.rs`, `startup_errors.rs`, `startup_node.rs` |
| m6-html | `integration.rs`, `read_timeout.rs`, `socket_mode.rs` |
| m6-file | `integration.rs`, `two_files.rs`, `woff2.rs`, `security_regressions.rs`, `site_size.rs` |
| m6-auth-server | `integration.rs`, `security_regressions.rs` |
| m6-auth-cli | `integration.rs` |
| m6-monitor | `lifecycle.rs`, `tunnel.rs`, `history.rs` |

**Table 6: a selection of the integration suites, by crate.**
Unit tests are absent from the table because they sit in `src/` beside the code they cover.

Each crate carries its fixtures under `<crate>/tests/fixtures/`.
m6-html's holds `site.toml`, `configs/`, `templates/`, `content/posts/`, `data/` and `route_params/`.
m6-file's holds `site.toml`, `m6-file-test.conf`, `content/posts/` and `assets/`.
m6-http's holds `site/site.toml` and `system.toml`, because m6-http takes both a site directory and a system config.

## 6. End to end

The end-to-end suite is example 05 in `examples/05-cms`, and its `test.sh` checks the whole running stack.
CI runs it on every pull request, and `tools/build-host-tests.sh` runs it on the build host.
[`m6-user-guide.md`](m6-user-guide.md) section 6 describes the example, and a failure in `test.sh` is diagnosed by starting the stack with `setup.sh` and `dev.sh` from `examples/05-cms` and requesting the routes by hand.

Every log line m6 writes is one JSON object (see [`m6-log.md`](m6-log.md)), so `journalctl -u m6-http -o cat | jq . > /dev/null` is a check on a service run under systemd.
A log line that is not valid JSON is a line no log pipeline reads.

## 7. Summary

A green run covers the layers that ran, and Section 1 says which those are.
m6's own tests run anywhere, `m6-core::testkit` keeps them from failing for reasons outside the code, conformance needs the testers installed, and performance needs a quiet machine.
Table 5 applies before any hand run of a tester.

The address and leak sanitisers run in `.github/workflows/sanitizer.yml`, on pushes to `develop`, on pull requests to it, and by hand (`workflow_dispatch`).
Its workspace run does not enable four off-by-default m6-core features (`multipart`, `email`, `flash`, `csrf`), so the sanitisers do not instrument that code.

Nothing in `check.sh`, `tools/build-host-tests.sh` or the CI workflows fuzzes the three hand-written protocol parsers.

Table 7 gives the documents that own what this one leaves out.
Every number omitted here has a file responsible for it.

| document | covers |
|---|---|
| `tools/conformance-scores.txt` | the recorded floors, and the argument behind the HTTP/3 number |
| [`PERFORMANCE.md`](PERFORMANCE.md) | every performance number, how it was measured and on what |
| [`BENCHMARKS.md`](BENCHMARKS.md) | the raw benchmark output behind those numbers |
| [`../CONTRIBUTING.md`](../CONTRIBUTING.md) | what has to pass before a change merges |
| [`m6-user-guide.md`](m6-user-guide.md) | the eleven examples, including example 05's end-to-end suite |

**Table 7: further reading, and what each document covers.**
The first two are the files a conformance or performance claim is checked against.
