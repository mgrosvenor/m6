# Testing m6

Four layers check m6, and they differ in what does the judging.

Numbers are not here.
Conformance floors are in `tools/conformance-scores.txt` and performance numbers in [`PERFORMANCE.md`](PERFORMANCE.md), each beside its argument.

Read section 4 before running h2spec or h3spec by hand.
Four of its five traps report a failure that is not there.

## Contents

1. [The four layers](#1-the-four-layers)
2. [Running the checks](#2-running-the-checks)
3. [The shared harness](#3-the-shared-harness)
4. [Conformance: h1spec, h2spec and h3spec](#4-conformance-h1spec-h2spec-and-h3spec)
5. [Where the tests are](#5-where-the-tests-are)
6. [End to end](#6-end-to-end)
7. [Summary](#7-summary)

## 1. The four layers

Table 1 gives the four.
Take from it that only the first runs usefully on a laptop.

| layer | what it proves | where it runs |
|---|---|---|
| unit and integration tests | m6's code does what m6 intends | laptop, build host, *CI* (continuous integration) |
| conformance, h1spec h2spec h3spec | m6 does what the *RFC* (Request for Comments) says | build host and CI, needs the testers installed |
| the examples | m6's interfaces work for code outside m6 | build host and CI, needs the `m6-examples` checkout |
| performance | a change did not cost latency | build host only, needs a quiet machine |

**Table 1: the four layers, what each proves, and where each runs.**
The third column is a constraint: a layer runs only where what it needs is present.

Conformance is judged by three implementations nobody here wrote.
A test written by the same hand as the code repeats the same misreading of the RFC twice and passes both times.

## 2. Running the checks

Table 2 gives the commands.
Take from it that only the last one runs everything.

| command | what it runs |
|---|---|
| `cargo test --workspace -- --test-threads=1` | every unit and integration test |
| `./check.sh` | release build, clippy, tests, formatting, conformance, benchmarks |
| `./check.sh --no-bench` | the same without the benchmarks |
| `tools/conformance.sh [h1\|h2\|h3\|resume]` | one protocol, or all of them with no argument |
| `M6_BUILD_HOST=user@example.com ./tools/build-host-tests.sh` | all of it, on Linux, with the testers installed |

**Table 2: the five commands, and what each one runs.**
`cargo test` is the loop while writing code, `check.sh` is the local gate, and `build-host-tests.sh` is what gates a merge, because it adds the performance check and real conformance.

Three rules apply to all of them:

- **`--test-threads=1` is not caution.**
  Three of m6-http's suites (`edge_proxy.rs`, `security_e2e.rs`, `analytics_e2e.rs`) start real processes on fixed loopback ports.
  Run them concurrently and their test functions race for those ports, then fail with 502s that have nothing to do with the code.
- **Run the gate on the build host.**
  The performance check failed its own second run on a laptop because a release build was going at the same time, which moved the readings by half.
  The conformance testers are not installed on a laptop at all.
- **A check that cannot measure must fail.**
  `tools/conformance.sh --allow-missing-tools` reports an absent tester as NOT TESTED and says the run proves nothing about that protocol.
  `check.sh` prints `PASS Conformance` and `All checks passed.` anyway, which is issue #166, so a laptop run tells you nothing about h2 or h3 whatever its last line says.
  `build-host-tests.sh` runs without the flag, so the path to a release cannot skip h2 or h3.

## 3. The shared harness

Every integration suite does the same four things: find a free port, find the release binary, start a service and wait for it to be ready, then read a response at the byte level.
`m6-core::testkit` is the five modules that do them once.
Table 3 gives them.
Take from it that a suite doing any of this itself is a suite reintroducing a failure already fixed.

| module | provides | the failure it closes |
|---|---|---|
| `paths` | `binary`, `exec_scratch_root` | each suite guessing where the release binary is |
| `port` | `claim_port`, `PortClaim` | `TcpListener::bind(":0")` read back after the listener drops, which frees the port before the child claims it |
| `process` | `Service`, killed on drop, both streams drained | a child that outlogs one pipe buffer blocks in `write`, and a dead child's last words go with the pipe |
| `wait` | `for_tcp`, `for_path` | a fixed sleep, long enough on an idle laptop and not when a dozen stacks start at once |
| `response` | `read_one` | four byte-level response parsers in the tests, disagreeing |

**Table 3: the five testkit modules, and the test failure each one closes.**
Every failure in the third column happened, and each presented as something else: a routing bug, a hang, or a 502.

The harness is behind the `testkit` feature, off by default and enabled in `dev-dependencies` only, so none of it reaches a production binary.

Two rules come with it, and both are quiet when broken:

- **Hold a `PortClaim` as long as the service holds the port.**
  Dropping it early reopens the race it exists to close, which is why it is `#[must_use]` and stored beside its `Service`.
- **`Service` drains stdout as well as stderr**, because m6's services log to stdout.

## 4. Conformance: h1spec, h2spec and h3spec

Three independent testers check m6 against the *RFC* (Request for Comments) documents.
Table 4 gives them.
Take from it that HTTP/1.1 has four targets, because there are four HTTP/1.1 implementations in the workspace, while HTTP/2 and HTTP/3 live in m6-http alone.

| tester | source | covers | targets |
|---|---|---|---|
| h1spec | `github.com/dropseed/h1spec` | RFC 9112 and 9110 | m6-auth-server, m6-file, m6-html, m6-http's redirect listener |
| h2spec | `github.com/summerwind/h2spec` | RFC 9113 | m6-http |
| h3spec | `github.com/kazu-yamamoto/h3spec` | RFC 9114 and QUIC | m6-http |

**Table 4: the three conformance testers and what each is pointed at.**
The last column is the asymmetry: one tester covers four services, the other two cover one.

h1spec was added on 2026-09-11, and its first run found three conventions for header-name case, none of the four parsers checking `Transfer-Encoding`, and m6-file sending a body on a HEAD that 404s.

### 4.1. The script

`tools/conformance.sh` writes a fixture site, starts a loopback instance, runs the tester, parses the score from the tester's own summary line, and compares it to the floor.

```sh
cargo build --workspace --release     # the script runs binaries, it does not build them
tools/conformance.sh                  # all of it
tools/conformance.sh h2               # one protocol
```

A run below its floor fails.
A run above it prints the new number and asks for the floor to be raised by hand, because raising a floor claims an improvement is permanent and belongs in the commit that earned it.
A target in the scores file that a full run did not measure fails the run.

### 4.2. By hand

Running a tester by hand is for diagnosing a failure the script has already reported.

```sh
h2spec -h 127.0.0.1 -p 8443 -t -k --timeout 5
h3spec -n 127.0.0.1 8443
```

`-t` runs h2spec over *TLS* (Transport Layer Security), `-k` accepts the loopback certificate, and `-n` skips h3spec's certificate name check.

Table 5 gives the five traps.
Take from it that four of them answer confidently and wrongly, with no error to warn you.

| # | trap | what it makes a reader believe |
|---|---|---|
| 1 | h3spec without `-n` | every HTTP/3 test failed, on a certificate name mismatch |
| 2 | the rate limiter left on | a wall of HTTP/2 failures, because h2spec opens a connection per test and the limiter is what gets measured |
| 3 | counting h2spec failures with `grep -c` | double the real number, because the tool redraws each failure line. 98 counted is 49 real |
| 4 | `grep` without `-a` on h2spec output | nothing, because the output carries control bytes and GNU grep says "binary file matches" |
| 5 | pointing a tester at a live site | a score nobody else can reproduce |

**Table 5: five traps in running h2spec or h3spec by hand, and what each one makes a reader believe.**
Trap 1 is a missing flag, trap 2 a configuration default, traps 3 and 4 are reading the output wrongly, and trap 5 is aiming at the wrong target.

Two of them need more than a row:

- **Trap 2 is set by a configuration default.**
  `tools/conformance.sh` writes `[rate_limit] enabled = false` into its fixture, and a hand-run instance needs the same, or `requests_per_min` raised and restored after.
  The same trap invalidated the first benchmark run this project took.
- **Trap 5 risks more than the reading.**
  h2spec deliberately sends malformed frames, and a three-byte frame could once kill this process outright (F076 in [`CHANGELOG.md`](../CHANGELOG.md)).
  A result measured against a private live site is also unreproducible by anyone else, which makes it an assertion.

## 5. Where the tests are

Table 6 gives the integration suites, by crate.
Take from it that every suite is named, so a claim about m6's behaviour can be traced to the file that checks it.

| crate | integration suites |
|---|---|
| m6-core | `log_reload.rs`, `pre_push_hook.rs`, `version_flag.rs` |
| m6-http | `integration.rs`, `integration_tests.rs`, `security_e2e.rs`, `security_regressions.rs`, `robustness.rs`, `edge_proxy.rs`, `analytics_e2e.rs`, `backbone_flow_control.rs`, `backends_contract.rs`, `backends_through_proxy.rs`, `redirect_lifecycle.rs`, `tls_resumption.rs` |
| m6-html | `integration.rs`, `read_timeout.rs`, `socket_mode.rs` |
| m6-file | `integration.rs`, `dynamic_routes.rs`, `security_regressions.rs` |
| m6-auth-server | `integration.rs`, `security_regressions.rs` |
| m6-auth-cli | `integration.rs` |
| m6-monitor | `lifecycle.rs` |

**Table 6: the integration suites, by crate.**
Unit tests are not listed, because they sit in `src/` beside the code they cover.

Each crate carries its own fixtures under `<crate>/tests/fixtures/`.
m6-html's holds `site.toml`, `configs/`, `templates/`, `content/posts/` and `data/`.
m6-file's holds a config and an `assets/` tree.
m6-http's holds a `site/` directory with `site.toml` and a certificate, plus `system.toml`, because m6-http takes both a site directory and a system config.

### 5.1. What this document does not list, and why

A per-service list of behaviours used to sit here, about 120 rows over four services, written before the code as a plan.
No row had ever been checked against the suites above, so a reader took a line like "SIGTERM twice, immediate exit" as a test that runs.
Three of those rows named a run under the address and leak sanitisers, and **nothing in this repository runs a sanitiser**, which is issue #164.

**That list is issue #165 now.**
An unverified specification in `docs/` reads as a statement about what m6 does, and this one was wrong in at least four places.
It is kept in full on the issue, and it comes back here row by row as each row is checked against a suite in Table 6.

Until then, Table 6 is the answer to what checks m6: the suites, and the code in them.

## 6. End to end

The real end-to-end suite is example 05 in `m6-examples`, which CI runs over the whole running stack on every pull request.
Below is the shape of it, for running by hand while diagnosing a failure.

```sh
# Start the stack. m6-http takes a site directory AND a system config.
m6-html        site/ site/configs/m6-html.conf &
m6-file        site/ site/configs/m6-file.conf &
m6-auth-server site/ site/configs/m6-auth.conf &
m6-http        site/ site/system.toml &

# Every public route answers 200.
for path in / /blog /blog/hello-world /assets/style.css; do
  curl -sk -o /dev/null -w "%{http_code} $path\n" "https://localhost:8443$path"
done

# A protected route with no token answers 401.
curl -sk -o /dev/null -w "%{http_code}\n" "https://localhost:8443/admin/dashboard"

# Log in, then use the token.
TOKEN=$(curl -sk -X POST "https://localhost:8443/auth/login" \
  -d '{"username":"admin","password":"..."}' | jq -r .access_token)
curl -sk -o /dev/null -w "%{http_code}\n" \
  -H "Authorization: Bearer $TOKEN" "https://localhost:8443/admin/dashboard"

# An unknown path answers 404 with the error route's HTML.
curl -sk -o /dev/null -w "%{http_code}\n" "https://localhost:8443/does-not-exist"

# Every log line is valid JSON.
journalctl -u m6-http -o cat | jq . > /dev/null
```

That last line is a check in its own right.
A log line that is not valid JSON is a line no log pipeline will read, and it stays invisible until something needs the logs.
Section 7 closes on what none of the four layers reaches.

## 7. Summary

A green run means what it means only if you know which of the four layers ran.
m6's own tests run anywhere, `m6-core::testkit` is what stops them failing for reasons that are not the code, conformance needs the testers installed, and performance needs quiet.
Running a tester by hand needs Table 5 first.

Two gaps are open, and neither is a documentation problem:

- **No sanitiser and no fuzzing**, anywhere in `check.sh`, `tools/build-host-tests.sh` or CI, in a codebase that hand-writes three protocol parsers.
  Issue #164.
- **The per-service behaviour lists are issue #165**, because no row had been checked
  against the suites in Table 6 and at least four were wrong.

Table 7 gives the documents that own what this one leaves out.
Take from it that every number omitted here has a file responsible for it.

| document | covers |
|---|---|
| `tools/conformance-scores.txt` | the recorded floors, and the argument behind the HTTP/3 number |
| [`PERFORMANCE.md`](PERFORMANCE.md) | every performance number, how it was measured and on what |
| [`BENCHMARKS.md`](BENCHMARKS.md) | the raw benchmark output behind those numbers |
| [`../CONTRIBUTING.md`](../CONTRIBUTING.md) | what has to pass before a change merges |
| [`m6-user-guide.md`](m6-user-guide.md) | the eleven examples, including example 05's end-to-end suite |

**Table 7: further reading, and what each document covers.**
The first two are the files a conformance or performance claim has to be checked against.
