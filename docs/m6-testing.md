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
5. [What each service is held to](#5-what-each-service-is-held-to)
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
The third column is a constraint, not a preference: a layer runs only where what it needs is present.

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
| `M6_BUILD_HOST=root@your-linux-box ./tools/build-host-tests.sh` | all of it, on Linux, with the testers installed |

**Table 2: the five commands, and what each one runs.**
`cargo test` is the loop while writing code, `check.sh` is the local gate, and `build-host-tests.sh` is what gates a merge, because it adds the performance check and real conformance.

Three rules apply to all of them:

- **`--test-threads=1` is not caution.**
  Three of m6-http's suites (`edge_proxy.rs`, `security_e2e.rs`, `analytics_e2e.rs`) start real processes on fixed loopback ports.
  Run them concurrently and their test functions race for those ports, then fail with 502s that have nothing to do with the code.
- **The build host, not the laptop.**
  The performance check failed its own second run on a laptop because a release build was going at the same time, which moved the readings by half.
  The conformance testers are not installed on a laptop at all.
- **A check that cannot measure must fail, not pass.**
  `tools/conformance.sh --allow-missing-tools` reports an absent tester as NOT TESTED and says the run proves nothing about that protocol.
  `check.sh` prints `PASS Conformance` and `All checks passed.` anyway, which is issue #166, so a laptop run is not a conformance result whatever its last line says.
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
Take from it that four of them produce a confident wrong answer rather than an error.

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

- **Trap 2 is a default, not a mistake.**
  `tools/conformance.sh` writes `[rate_limit] enabled = false` into its fixture, and a hand-run instance needs the same, or `requests_per_min` raised and restored after.
  The same trap invalidated the first benchmark run this project took.
- **Trap 5 risks more than the reading.**
  h2spec deliberately sends malformed frames, and a three-byte frame could once kill this process outright (F076 in [`CHANGELOG.md`](../CHANGELOG.md)).
  A result measured against a private live site is also unreproducible by anyone else, which makes it an assertion rather than evidence.

## 5. What each service is held to

The lists below are grouped into levels, L1 upward.
They were written before the code, and no row has been checked against the suites that exist, which is issue #165.
Until that is done, read a row as what m6 intends and Table 6 as where to find the test.

**Nothing in this repository runs a sanitiser, and nothing fuzzes it.**
Three rows below name a sanitiser run that no script, workflow or command performs.
Those rows are marked, and the gap is issue #164.

Table 6 gives the suites that do exist.
Take from it where to look for the test behind a row.

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

Each crate carries its own fixtures under `<crate>/tests/fixtures/`, rather than one shared fixture site.

### 5.1. m6-html

m6-html's seven levels are Table 7 start and stop, Table 8 route matching, Table 9 the params merge, Table 10 path parameter expansion, Table 11 built-in keys, Table 12 status and cache, and Table 13 compression.
Take from them that everything m6-html does is decided by config and request, and that it holds no state of its own.

| test | expected |
|---|---|
| Valid config, no `secrets_file` | Starts using config values |
| Valid config, `secrets_file` present | Starts using merged values, secrets file wins on conflict |
| `secrets_file` declared but file absent | Silently ignored, starts with config values |
| Local key overrides global | Starts, warning in stdout |
| SIGTERM | Finish in-flight, exit 0 |
| SIGTERM twice | Immediate exit |
| SIGINT | Same as SIGTERM |

**Table 7: m6-html L1, start and stop.**
A missing secrets file is not an error, and a second SIGTERM stops waiting.

| test | expected |
|---|---|
| `/blog` with `/blog` and `/blog/{stem}` declared | Exact `/blog` matched |
| `/blog/hello-world` | Parameterised, stem is `hello-world` |
| No matching route | 404, empty body |
| Equal specificity tie | First declaration wins, warning logged |

**Table 8: m6-html L2, route matching.**
An exact route beats a parameterised one, and a tie goes to declaration order rather than being left undefined.

| test | expected |
|---|---|
| `global_params` and route `params`, conflicting key | Route params win |
| Three files, left to right | Last file wins |
| Missing params file | 500, error in stdout |
| Built-in key in params file (`site_name`) | Built-in overwrites, injected last |

**Table 9: m6-html L3, the params merge.**
Precedence runs global, then route, then built-in, with built-ins injected last so nothing can shadow them.

| test | expected |
|---|---|
| `/blog/hello-world` to `content/posts/{stem}.json` | Reads `hello-world.json` |
| `{stem}` containing `..` | 400 |
| `{stem}` containing `/` | 400 |
| `{relpath}` with subdirectory | Allowed |

**Table 10: m6-html L4, path parameter expansion.**
A `{stem}` may hold neither a separator nor a parent reference, and a `{relpath}` may hold a separator by design.

| test | expected |
|---|---|
| `site_name` in template | From `[site] name` |
| `request_path` | Matches request path |
| `query.foo` for `?foo=bar` | `"bar"` |
| `/error?status=404&from=/x` | Template receives `error_status` and `error_from` |

**Table 11: m6-html L5, built-in keys.**
Every built-in comes from the request or from `site.toml`, so a template never has to be told them.

| test | expected |
|---|---|
| Route `status = 404` | Response status 404 |
| `cache = "public"` | `Cache-Control: public` |
| `cache = "no-store"` | `Cache-Control: no-store` |

**Table 12: m6-html L6, status and cache.**
A route sets its own status and cacheability, which is what makes an error route a route like any other.

| test | expected |
|---|---|
| `Accept-Encoding: br` | `Content-Encoding: br`, decompresses to correct HTML |
| `Accept-Encoding: gzip` | `Content-Encoding: gzip` |
| No `Accept-Encoding` | Identity |

**Table 13: m6-html L7, compression.**
Brotli is preferred when offered, and no header means no encoding rather than a guess.

L8 is integration: 100 concurrent requests across all routes with no errors, and a content update picked up after modifying `data/site.json` and restarting.
Its sanitiser clause (1000 requests, SIGTERM, no leaks) **does not run anywhere.**

### 5.2. m6-file

m6-file's L1 is m6-html's L1, in Table 7, because start and stop behaviour is shared.
What is specific to m6-file is Table 14 path resolution and Table 15 compression by type.
Take from them that path resolution is the level that matters, because it is where a static file service gets exploited.

| test | expected |
|---|---|
| Existing file | Correct bytes, correct `Content-Type` |
| Nonexistent file | 404 |
| `../` traversal in the URL | 404 |
| `{relpath}` with subdirectory | Correct file |
| Symlink pointing outside the root | 404 |

**Table 14: m6-file L2, path resolution.**
Traversal and a symlink out of the root both answer 404, rather than an error that tells the two apart.

| test | expected |
|---|---|
| `text/css` requested | Compressed, brotli or gzip per `Accept-Encoding` |
| `image/jpeg` requested | Not compressed |
| `font/woff2` requested | Not compressed |

**Table 15: m6-file L3, compression by type.**
Already-compressed formats are served as they are, because compressing them spends time to add bytes.

L4 is `Cache-Control`, and its current behaviour is in [`m6-file.md`](m6-file.md), which is the service reference and is kept current.
L5 is a sanitiser run that **does not run anywhere.**

### 5.3. m6-http

m6-http's ten levels are Table 16 start and stop, Table 17 routing, Table 18 public routes, Table 19 protected routes, Table 20 the login endpoint, Table 21 refresh and logout, Table 22 pool management, Table 23 caching, Table 24 error handling, Table 25 error modes, and Table 26 hot reload.
Take from them that auth is the largest part of m6-http's checking, and that the first thing checked about it is that a public route runs none of it.

| test | expected |
|---|---|
| No arguments | Exit 2 |
| Site dir only, no system config | Exit 2, second argument required |
| Both args, valid system config | Starts, `[server]` from system config |
| Both args, system config has a non-`[server]` key | Warning logged, key ignored, starts |
| `[server]` absent from `site.toml`, present in system config | Starts, validation runs after the merge |
| `[server]` absent from both | Exit 2 |
| System config missing or unparseable | Exit 2 |
| `[auth]` declared, public key not found | Exit 2 |
| `require` on a route with no `[auth]` | Exit 2 |
| `--dump-config` | Effective merged config to stdout, exit 0 |
| SIGTERM | Drain in-flight, exit 0 |
| SIGTERM twice | Immediate |
| SIGTERM during an active request | In-flight completes, then exit |

**Table 16: m6-http L1, start and stop.**
Every configuration error is exit 2 and none is a warning, because a proxy that starts with auth misconfigured is worse than one that refuses.

| test | expected |
|---|---|
| Exact path | Correct backend |
| Parameterised path | Correct backend |
| No match | 404 per `[errors] mode` |

**Table 17: m6-http L2, routing.**
A miss is handled by the error mode rather than by a hard-coded page.

| test | expected |
|---|---|
| Public route, no *JWT* (JSON Web Token) | Forwarded, no auth check |
| Public route, any JWT | Forwarded, no auth check |
| Cached public route | Served from cache, zero auth code executed |

**Table 18: m6-http L3, public routes, the hot path.**
A public route runs no auth code at all, including when a token is present, and this level is verified by instrumenting the verification function and asserting a call count of zero.

| test | expected |
|---|---|
| No token, API client | 401 |
| No token, no refresh cookie, browser | 302 to `/login?next=<path>` |
| No session cookie, valid refresh cookie, browser | 302 to `POST /auth/refresh`, new cookies, original path |
| No session cookie, expired refresh cookie, browser | 302 to `/login?next=<path>` |
| Invalid JWT, bad signature | 401 or 302 |
| Expired JWT, no refresh cookie | 401, or 302 to login |
| Valid JWT in `Authorization` header | Forwarded |
| Valid JWT in session cookie | Forwarded |
| Both header and cookie present | Header takes precedence |
| Valid JWT, wrong group | 403 |
| Valid JWT, correct group | Forwarded with `X-Auth-Claims` |
| `X-Auth-Claims` content | Base64-decoded JSON matches the token claims |

**Table 19: m6-http L4, protected routes.**
An API client gets a status and a browser gets a redirect from the same failure, and a wrong group is 403 while a bad token is 401.

| test | expected |
|---|---|
| `POST /auth/login` form, valid credentials | 302 to `next`, two HttpOnly cookies set |
| `POST /auth/login` form, invalid credentials | 302 to `/login?error=invalid&next=<next>` |
| `POST /auth/login` form, `next` is an external URL | 302 to `/`, `next` ignored |
| `POST /auth/login` form, `next` absent | 302 to `/` |
| `POST /auth/login` JSON, valid credentials | 200, JSON tokens, no cookies |
| `POST /auth/login` JSON, invalid credentials | 401 |
| `POST /auth/login`, rate limited | 429 with `Retry-After` |
| `session` cookie `Path` | Sent on all requests |
| `refresh` cookie `Path` | Sent only to `/auth/refresh` |

**Table 20: m6-http L4, the login endpoint.**
A form login answers with cookies, a JSON login with tokens, and an external `next` is dropped rather than followed, which is what stops a login link becoming an open redirect.

| test | expected |
|---|---|
| `POST /auth/refresh`, valid refresh cookie | 302 to `Referer`, new session cookie |
| `POST /auth/refresh`, expired refresh cookie | 302 to `/login` |
| `POST /auth/refresh` JSON, valid token | 200, new access token |
| `POST /auth/refresh` JSON, expired token | 401 |
| `POST /auth/logout` form | 302 to `/`, both cookies cleared with `Max-Age=0` |
| `POST /auth/logout` API | 204, refresh token revoked |

**Table 21: m6-http L4, refresh and logout.**
Logout revokes the refresh token rather than only clearing the cookies, so a copied token stops working too.

| test | expected |
|---|---|
| Socket appears matching the glob | Added to the pool, requests routed to it |
| Socket disappears | Removed from the pool |
| All sockets gone | 503 per `[errors] mode` |
| One socket fails, others healthy | Traffic shifts to the healthy sockets |
| Failed socket retried after backoff | Rejoins the pool when available |

**Table 22: m6-http L5, pool management.**
A backend joins and leaves by its socket appearing and disappearing, with no restart and no registration step.

| test | expected |
|---|---|
| `Cache-Control: public` | Cached |
| `Cache-Control: no-store` | Not cached |
| Cache key is (path, encoding) | `br` and `gzip` are separate entries |
| Query strings stripped | `?a=1` and `?a=2` share a cache key |
| No eager pre-fetch | One request makes one cache entry |

**Table 23: m6-http L6, caching.**
The backend decides cacheability and m6-http decides the key, and the key is the path and the encoding only.

| test | expected |
|---|---|
| Backend returns 404 | Fetches `/_errors?status=404&from=/original-path`, returns 404 and HTML |
| Backend returns 500 | Fetches `/_errors?status=500&from=/original-path`, returns 500 and HTML |
| No `[errors] path` configured | Returns status per `[errors] mode` |
| The error page fetch itself fails | Falls back to `[errors] mode`, no loop |
| Request already to the error path | Returns status per `[errors] mode`, no recursion |
| Pool unreachable | 503 per `[errors] mode` |

**Table 24: m6-http L7, error handling.**
An error page is fetched like any other page, and every way that fetch can fail ends at `[errors] mode` rather than in a loop.

| mode | response when the pool is unreachable |
|---|---|
| `"status"` | 503, empty body |
| `"internal"` | 503, m6-http's own minimal HTML |
| `"custom"` | 503, error page fetched from `[errors] path` |

**Table 25: m6-http L8, `[errors] mode`.**
The three modes trade a dependency for a better page, and `"status"` is the one that depends on nothing.

| change | expected |
|---|---|
| `site.toml` modified | Route table updated, no restart |
| TLS certificate modified | Context reloaded |
| Data file modified | Affected cache entries evicted |
| New socket appears | Added to the pool, no restart |

**Table 26: m6-http L9, hot reload.**
Config, certificates, data and backends all change without a restart, which is what makes a certificate renewal invisible to traffic.

L10 is load: 1000 concurrent requests over mixed routes and mixed auth, SIGTERM mid-load, no deadlocks, and the pool reflecting socket state throughout.
Its no-leaks clause **does not run anywhere.**

### 5.4. m6-auth-server

Four of m6-auth-server's six levels are Table 27 start and stop, Table 28 login, Table 29 token refresh, and Table 30 verification.
Take from them that verification is checked on m6-http's side, because that is where it happens.

| test | expected |
|---|---|
| Valid config | Starts, socket appears |
| Missing private key | Exit 2 |
| Missing database directory | Exit 2, or create |
| SIGTERM | Exit 0 |

**Table 27: m6-auth-server L1, start and stop.**
A missing key is a refusal to start, in keeping with Table 16.

| test | expected |
|---|---|
| Correct credentials | 200, access and refresh tokens in the JSON body |
| Correct credentials | `Set-Cookie: session=<jwt>`, HttpOnly, Secure, SameSite=Strict |
| Wrong password | 401, no cookie set |
| Unknown user | 401, the same response as a wrong password |
| 6th attempt within 15 minutes | 429 with `Retry-After` |
| After the rate-limit window | Login succeeds again |

**Table 28: m6-auth-server L2, login.**
An unknown user and a wrong password give the same answer, so the endpoint does not confirm which accounts exist.

| test | expected |
|---|---|
| Valid refresh token | 200, new access token |
| Expired refresh token | 401 |
| Invalid token | 401 |
| After logout | 401, revoked |

**Table 29: m6-auth-server L3, token refresh.**
Revocation is checked at refresh, which is what bounds how long a stolen refresh token is worth anything.

| test | expected |
|---|---|
| Token signed with the correct key | Verified locally |
| Token signed with the wrong key | 401 |
| Token `exp` in the past | 401 |
| Token `iss` mismatch | 401 |
| Token groups match `require` | Forwarded |
| Token groups do not match | 403 |

**Table 30: m6-auth-server L4, JWT verification, done in m6-http.**
Verification is local to m6-http and makes no call to m6-auth-server, so a protected route costs a signature check rather than a round trip.

L5 is user and group management.
Every endpoint needs a `role:admin` token and answers 403 without one.
A created user exists, a new group membership appears in the next login token, a deleted user cannot log in, and a deleted group removes its memberships.

L6 is key rotation.
A token issued under key A stays valid until it expires after a rotation to key B.
A token under key B is accepted at once, and a token under neither is rejected.

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
- **The lists in section 5 have never been checked against the suites** in Table 6.
  Issue #165.

Table 31 gives the documents that own what this one leaves out.
Take from it that every number omitted here has a file responsible for it.

| document | covers |
|---|---|
| `tools/conformance-scores.txt` | the recorded floors, and the argument behind the HTTP/3 number |
| [`PERFORMANCE.md`](PERFORMANCE.md) | every performance number, how it was measured and on what |
| [`BENCHMARKS.md`](BENCHMARKS.md) | the raw benchmark output behind those numbers |
| [`../CONTRIBUTING.md`](../CONTRIBUTING.md) | what has to pass before a change merges |
| [`m6-user-guide.md`](m6-user-guide.md) | the eleven examples, including example 05's end-to-end suite |

**Table 31: further reading, and what each document covers.**
The first two are the files a conformance or performance claim has to be checked against.
