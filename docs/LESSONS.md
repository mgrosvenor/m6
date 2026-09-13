# Lessons

Things this project learned the hard way, kept because each one cost something
and because most of them are not specific to m6.

Moved out of `HANDOVER.md` on 2026-09-13, when that file was rewritten to be
readable by someone with no prior context. The handover carries the dozen that
come up most often; this is all of them.

The numbering is historical and runs in the order they were learned, so the
low numbers are the oldest.

---

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
14. **A monitoring tool will measure its own effect, and it will keep doing
    it.** Three times in one day, three different mechanisms: the health check
    flagged its own load generator as a security incident; it read a 50%
    latency regression off a single sample taken during that load; and it timed
    a 4ms loopback call immediately after a 24-hour `journalctl` scan on a
    two-core VM, reporting 266ms maxima that did not reproduce in twelve clean
    samples a minute later. The fixes, in order: require failure as well as
    volume, take the median of five, measure before the scan rather than after.
    Assume there is a fourth.
15. **A fault list only works if everything on it is a fault.** The same rule
    fired on 185 requests from the operator's own address and on one 404 to
    `/.git/config`. Both are noise and both teach the reader to skim. What
    counts on its own is what is deliberate: injection, user-agent rotation,
    scanning across several distinct paths. Volume counts only together with
    failure, and a single refused probe is recorded without being escalated.
16. **A claim can be literally true and support a false conclusion.** "Zero
    template files" was true of the three renderers and did not mean they
    needed no template engine.
17. **Read the full user-agent list; a keyword list invents crawlers.** One IP
    rotating 526 user agents would have been reported as a dozen AI crawlers
    visiting. `UA_ROTATION_THRESHOLD` makes that a property of the data.
18. **Encoding and decoding are two halves of one block.** Core could
    percent-decode and not encode, so callers wrote their own encoder. The same
    shape as having four cookie formatters and no cookie type.
19. **Check the transport before writing the runbook.** The monitor was
    designed against the WireGuard mesh because that is the obvious answer;
    origin's backbone listener is h2c-only and the cache nodes have none.
20. **A monitor inside the thing it monitors cannot report the failure that
    matters.** It was specified for the central node until the owner asked
    where it should run. Run it on syd and the fleet digest dies with syd.
    Related: a client that builds a fresh connection per request pays a cold
    TLS handshake each time, which over a long link is most of the measurement
    (828ms to lon, versus 27ms to syd, and the difference is the handshake).
21. **Confinement must claim only what the role actually has.** Putting
    `ReadWritePaths=/run/m6` in the shared hardening fragment took London off
    the air for about ninety seconds on 2026-09-11 with `226/NAMESPACE`: a
    cache node proxies to origin over h2c, runs no socket backends, and so has
    no `/run/m6` at all, and an absent `ReadWritePaths` target fails mount
    namespace setup outright. The fix is per-role fragments plus
    `RuntimeDirectory` to guarantee what must exist, never a `-` prefix to
    excuse an absent path: tolerance turns a misconfigured node into one that
    starts anyway with weaker isolation than intended. Written up in the site
    repo at `deploy/systemd/hardening/_common.conf` and
    `deploy/systemd/hardening/m6-http-cache.conf`.

    The second half of this lesson is that the warning was *already* in
    `deploy/systemd/m6-html.service`, read earlier the same session, and the
    mistake was made anyway. A caution that lives only next to the code it
    guards will be read and not retained. That is why it is here.

New 2026-09-12:

22. **Counts rank a source; identity decides what it is.** `15.177.23.18` sent
    371 requests to chi, 100% refused, for three hours without adapting, and
    was reported across three consecutive hourly checks as the strongest
    malicious block candidate of the day. Reading one field settled it:
    `UA: Amazon-Route53-Health-Check-Service`, path `/route53-health/index.php`,
    every 30 seconds to the second. It is the eighth orphaned health check, and
    seven siblings were already in the ledger. **Every trait that made it look
    like a determined attacker is what a dead health check looks like**: high
    volume, total failure, infinite persistence, no adaptation. An attacker
    varies paths when refused; nothing was reading these results to vary them.
    Rank by counts, then read the user agent and the path before naming it.
23. **A gate that cannot measure must fail, not pass.** `run_h3` in
    `tools/conformance.sh` never starts the server it tests, and records a
    score only `if got > 0`, so a run that measured nothing printed **PASS**.
    That is lesson 12 living inside the thing built to enforce lesson 5. Any
    check whose failure mode is silence needs an explicit "did I actually
    measure anything" assertion.
24. **The same defect wears different clothes at every layer.** Four copies of
    `socket_path_from_config`, five hand-written copies of the lifecycle
    assertion, two accept loops 22/33 identical, four I/O idioms, two
    concurrency models. Each was found by asking "how many implementations of
    this are there?" rather than by reading any one of them. That question is
    the most productive one available in this codebase and it has not stopped
    paying yet.
25. **Check which mechanism is running before explaining why it cannot work.**
    Asked to reschedule the hourly health check, I explained at length why a
    cloud routine could not reach production over ssh. The check was a local
    session cron using my own shell and keys, and the answer was one field in
    a cron expression. `CronList` first, theory second.

26. **A safety net added at one layer becomes a defect at the next.** Giving
    `App` a read timeout was scoped as three lines and a config key. Setting
    the option was indeed three lines. What the scoping missed is that nothing
    downstream had ever seen a read time out: the timeout surfaced as
    `ParseError::Io(WouldBlock)`, whose status is 400, and `serve_connection`
    dutifully wrote that 400 to a peer that had sent nothing. m6-http pools
    backend connections, so a response written into an idle socket is read as
    the answer to the *next* request on it, which is manufactured response
    smuggling on a code path added to improve safety. The fix is the split the
    parser already made for `Ok(0)`, on whether any byte arrived: nothing means
    an idle peer leaving, so close silently; a stalled part-request gets 408.
    **When adding a deadline, follow the new error all the way to the wire**,
    and ask what the peer does with whatever gets written.
27. **`testkit::binary()` prefers `target/release`, and will happily hand a
    test a binary from yesterday.** `cargo test` rebuilds the lib and the test
    binary from current source, then spawns a service binary that may be hours
    old. On 2026-09-12 a correct fix measured as broken twice, and the wire
    said the opposite of the test: a manual run closed the connection at
    exactly 1.00s while the suite insisted nothing happened. The release-first
    rule is deliberate and should not be flipped (it was itself a fix, see the
    doc comment), and there is no sound mtime check to bolt on, because in the
    intended order cargo relinks the test binary *after* the release binaries.
    So it is procedural: **`cargo build --workspace --release` before
    `cargo test`**, and when an end-to-end test contradicts what the source
    plainly says, `ls -la` the binary before debugging the code.
    `deploy/run-tests.sh` builds release itself and is not exposed.
28. **Skipping a read means skipping what the read was rejecting.** m6-file's
    HEAD fast path answers from `std::fs::metadata`, which succeeds on a
    directory and reports its size, so `HEAD /assets/css` returned `200` with
    `Content-Length: 128` while the GET beside it returned 404. The `fs::read`
    the fast path removed was doing two jobs: producing the bytes, and failing
    on anything that was not a file. Only the first was obvious. **Before
    skipping work, list what that work was implicitly validating.** Same family
    as the conditional-request defect: a correct function in one crate and a
    wrong inline copy in another, invisible until the cache state changed.
29. **A control assertion is what makes a test mean anything, and it earns its
    place on the machine you did not think about.** The HEAD test chmods a file
    to `000` and requires the HEAD to answer anyway. The Linux build host runs
    the suite as **root**, root ignores permission bits, the file stayed
    readable, and what fired was the control: "the file must really be
    unreadable or this test proves nothing". Without that line the test would
    have gone green on a box where it demonstrates nothing, which is the
    "never been red" failure wearing a uid. It now skips explicitly when it can
    read a `0000` file. **A green tick that depends on who ran it is worse than
    an absent one.**
30. **A method-equivalence test only covers the inputs it is given.** The HEAD
    against GET comparison walks identity, minified and brotli and asserts
    identical headers, and it stayed green through the directory defect above,
    because every path it asks for is a file. Equivalence is not coverage.
31. **A test helper that cannot fail is not a probe.** Four readiness helpers
    in the e2e suites `.unwrap()`ed a transport error while being called from
    inside a 30-second retry loop written to tolerate exactly that. Two of them
    panicked on a refused connect *after* asking whether the service was alive,
    and printed the answer in the panic: `failed with m6-http alive`. **When a
    failure message contains its own refutation, believe the message.**
32. **"It does not reproduce" is a statement about the harness, not the bug.**
    Five separate intermittent failures were each reproduced deliberately once
    the mechanism was guessed: a listener that accepts and drops for the RST, a
    50 ms window on each side of a global static for the race. The one that
    took longest, `a_fresh_flag_is_clear`, ran clean 300 times in isolation and
    300 times under artificial CPU load, and was still a hard race. **Narrow is
    not the same as rare, and neither is the same as acceptable.**
33. **A gate must run where production runs, and the difference will find you.**
    Three things only showed up on the Linux build box: clippy was not
    installed, the clippy count differed by 13 from the laptop's (different
    version, plus cfg-gated code that only compiles there), and the
    unreadable-file test could not work because the box runs as **root**, which
    ignores permission bits. None of it was visible locally.


34. **A doc comment that justifies a decision by naming a premise becomes a lie
    the day the premise changes, and nothing anywhere checks it.**
    `validate_path_param` said, correctly and at length, that every parameter
    is validated with `allow_slash = false` *because* "this crate's router has
    no catch-all support ... a parameter here captures exactly one path segment
    and can never contain a slash". Adding `Segment::Wildcard` falsified that
    sentence and left the code it was explaining in place, so the one capture
    defined to hold slashes was answered 400 by the validator. The comment was
    the best possible warning and it was in the one file the change did not
    touch. **When adding a capability, grep for the assumption it invalidates**,
    not just for the code it calls: `allow_slash`, `exact segment count` and
    `can never` were each one search away. Same family as lesson 21, where the
    caution lived next to the code it guarded and was read and not retained.
35. **Two of this session's three findings were in work already marked DONE.**
    Wildcard routing shipped with six green tests that all stopped at the
    matcher, and the `Last-Modified` loop contradicted the comment at its own
    emit site. Neither was found by reading the ledger, which said both were
    finished; both were found by using the feature for the next thing. **The
    cheapest audit of a completed item is the first real consumer**, and until
    there is one, "done" means "written", which is what §6's closing note now
    says out loud about the reload chain itself.


36. **Two ledger entries written 23 minutes apart contradicted each other, and
    the later work inherited the earlier claim.** `a979390` at 14:32 recorded
    "streaming never blocked m6-file, checked against the source"; `8c79ee7`
    at 14:55 gave m6-file `send_stream`. Both were accurate when written. The
    migration row kept pointing at the first one for the rest of the day, and
    a session later it was still being quoted, by me, to the owner. **A
    "checked against the source" note is a measurement with a timestamp, not a
    fact**, and it expires the moment the source changes. When a row cites a
    check, cite the commit it was checked at, so the next reader can see
    whether anything has landed since.
37. **Measure the cost of the thing you are migrating onto, not just its
    capabilities.** The gating question for m6-file looked like a list of
    features `App` lacked, and all of them got built. What actually stops the
    migration is that `App` deep-copies its whole static config into a fresh
    map on every request. No capability list would have surfaced it; one
    `#[ignore]`d measurement did. Lesson 1 said measure the candidate before
    consolidating onto it, and that was about *correctness* scores; this is
    the same rule about cost.
38. **A synthetic benchmark measures the shape you imagined, not the one that
    runs.** The first figure was 3.08us, from a config with twenty short
    string keys, and it was reported as the finding. The owner's reply was
    that 3us sounded wrong for building a small map, which was the right
    instinct twice over: the map and the parser are 41 to 583ns, so the number
    was all copy; and the real config loads a 68KB JSON file **twice**, making
    the true figure ~323us, seventy times larger. **Take the input from
    production before quoting a number**, and when a measurement looks too big
    for what it claims to measure, that gap is the finding.
39. **Making a warning fatal does not create the bug it reveals.** The e2e
    suites raced on ports. `SO_REUSEADDR` had been added and was believed to
    have closed it, and it had: it fixes rebinding a port in `TIME_WAIT`, a
    socket closed but lingering. It cannot fix a port another process is
    actively **listening** on, and refusing that is the entire point of the
    check. Those are two different races and were being treated as one.

    The second one was in the test harness, not the server. `PortClaim::drop`
    removed its marker file immediately, but the marker only guarantees no
    other **test** picks the port. It says nothing about whether the
    **service** that was using it has exited. A claim dropped while its
    process was still shutting down freed the marker, the next test claimed
    the port, and its service could not bind.

    It stayed survivable while a failed bind was a warning: the process came
    up with no listener and the test failed later with "never served a backend
    request", naming the symptom and not the cause. A comment in `http11.rs`
    had predicted exactly that outcome in exactly those words. Making a failed
    bind fatal turned a confusing late failure into an immediate honest one,
    which is the only reason it was ever found. **When a change to error
    handling starts producing failures, the first question is whether it
    created them or stopped hiding them.**

    Fixed in the primitive rather than in each test: `PortClaim::drop` now
    waits until the port genuinely binds before releasing the marker, bounded
    at five seconds so something outside the suite cannot hang the run.
    Containing a race in the type means it cannot come back through a test
    that happens to declare its fields in the wrong order.
