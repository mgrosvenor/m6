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
    where it should run. Run it on origin and the fleet digest dies with syd.
    Related: a client that builds a fresh connection per request pays a cold
    TLS handshake each time, which over a long link is most of the measurement
    (828ms to edge-a, versus 27ms to origin, and the difference is the handshake).
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
    371 requests to edge-b, 100% refused, for three hours without adapting, and
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

40. **Correct attribution is not a diagnosis.** h3 conformance sat at 37/49 for
    months with the twelve failures recorded as quiche's, on sound reasoning:
    QUIC transport parameter validation, packet reserved bits and QPACK are all
    below the layer m6-http works at. The 1.0 list therefore carried the remedy
    that followed from it, "one dependency bump and one CI run", and nobody
    doubted it because the attribution was right.

    The bump was done: 0.26.1 to 0.29.3, three releases, with a feature rename
    on the way because 0.29 dropped the vendored BoringSSL build. The score did
    not move by one test.

    Reading quiche's source explained the mechanism. Every one of the twelve is
    worded "MUST **send** \<error\>" rather than "MUST reject". For the eight
    transport parameter cases quiche does detect the problem and calls `close()`
    itself, queueing the right code, but `close()` calls `mark_closed()` when no
    packet has yet been fully processed, and `recv_count` only increments at the
    end of `recv_single`, so a bad parameter in the client's first Initial makes
    the queued close unsendable. For the two reserved-bit cases it does not
    detect anything: there is no reserved-bit validation in `packet.rs` at all.
    The two QPACK codes, 0x201 and 0x202, appear nowhere in its h3 module.

    **Then the second mistake, and it is the one worth the entry.** Having read
    the source, I concluded these were deliberate anti-DoS and anti-MITM design
    choices and wrote that into the record: unfixable, and moving the ceiling
    would mean carrying a patched quiche or replacing the QUIC layer. The owner
    did not believe it, said so, and asked the obvious question I had not asked:
    are there outstanding issues, is there a newer version.

    Ten of the twelve are open upstream bugs with open fix pull requests.
    Issue #2515 describes the first-flight unsendable CONNECTION_CLOSE in the
    same terms I had just derived, down to `recv_count == 0` and the expected
    TRANSPORT_PARAMETER_ERROR 0x08, and PR #2521 proposes the fix. Issues #2526
    and #2652 cover the reserved bits, with PR #2575 active. So the honest
    position is not "unfixable by design", it is "a known defect waiting on
    upstream, watch two PRs and re-measure". The version check I had run was
    also filtered to the 0.2x line and could not have seen a newer one.

    Applying both PRs and measuring settled it: h3 goes from 37/49 to **47/49**,
    with only the QPACK pair left. So the thing written off as possibly needing
    the transport layer replaced was three clean cherry-picks and forty-nine
    lines, and m6 now runs on a fork carrying them.

    Two things, then. **Knowing which component owns a defect tells you nothing
    about whether it is fixable, or about what would fix it.** A remedy inferred
    from an attribution is still a guess, and it reads as a plan for as long as
    nobody tries it. The bump was the cheapest way to find out, and watching the
    number refuse to move is what started the real investigation.

    And: **a dependency's source tells you what it does, not whether its
    authors think that is correct.** A comment explaining a behaviour reads
    exactly like a comment endorsing it. The issue tracker is where intent
    actually lives, it takes one search, and skipping it turned a two-PR wait
    into a false claim that the transport layer might have to be replaced.

41. **Two repositories that build separately, with nothing building them
    together, is not a risk. It is a defect already present, waiting to be
    looked at.** m6-examples had not compiled for days: every renderer crate
    still pointed at `m6-render`, a crate m6 had deleted. The binaries left in
    `target/release` from before the deletion meant running an example still
    looked fine, so nothing announced it.

    Getting it to build took four small edits. What that bought was the ability
    to ask questions, and the answers were five more defects sitting in plain
    sight: every asset in every example returned 502 because m6-file's config
    schema had changed and ten configs were left naming no handler; PATCH was
    refused at the edge by `allowed_methods`, which looks exactly like a missing
    route; unpublishing a CMS post answered `{"unpublished": true}` and left the
    post listed and readable; starting an example ran `pkill -x m6-http` and
    killed the seven-node fleet running on the same laptop, which it did again
    that afternoon.

    None of them were subtle. Every one was a single request away from being
    obvious. They survived because **the only thing that asks whether an
    interface still works is code that uses it**, and m6's own checks contain no
    site. The same session had already found the identical shape in the
    deployment repository's renderers, by hand, and treated it as an incident
    rather than as a category.

    The fix is not vigilance, it is that m6's own checks build the examples and
    run their end-to-end suite. That is also the honest reading of the earlier
    lesson about a check that lives in one place only: this was a check that
    lived in no place at all.

42. **A check that accepts a range of answers is a comment.** The CMS example's
    suite had five: "302 or 401 or 403", "may require a valid token", "session
    may persist on server". Each one sat exactly on top of a real defect.

    The worst of them read the API's own reply. Unpublishing a post returned
    `{"unpublished": true}`, the test asserted that the body contained the word
    "unpublished", and it passed for however long the handler had been broken --
    while the post stayed in the index, stayed listed, and stayed readable. The
    test and the defect agreed with each other, so the test defended it.

    Another said "token not found in cookie jar (may use httpOnly)" on every run
    of a working server, because it used `grep -oP` and this is macOS. The
    hedge in the message is what made that survivable: it had an explanation
    ready for its own failure, so nobody had to look.

    **Ask the system, not the component that just told you what it did.** Every
    step of that lifecycle is now checked against what a visitor sees, and the
    rewritten suite has no skips at all: 96 checks, each with one expected
    answer. Three of the six defects above were found by writing it.

43. **`pkill -x` is a machine-wide operation, and so is any name.** Three
    examples' `dev.sh` and the deployment's own `dev.sh` cleared stale state with
    `pkill -x m6-http; pkill -x m6-html; ...`. A process name is not a scope:
    anyone with another m6 running lost all of it, with no error, no log line,
    and nothing to connect the disappearance to the command that caused it.

    It destroyed a running seven-node local fleet twice in one afternoon, the
    second time while cleaning up after the first.

    Scoping by path (`pgrep -f "$SITE"`) fixes the examples, where each one owns
    its own directory. It does **not** fix the deployment, whose dev stack and
    whose local fleet both run with paths under the same repository -- there, a
    pid file is the only thing that actually knows which processes a previous run
    of this script started. The general rule: **a cleanup must be able to name
    what it owns.** If it can only describe what it wants to kill, it will kill
    somebody else's.

44. **A test that changes the file's contents cannot tell you whether a
    timestamp-only change is noticed.** `Request::touch` is m6-core's documented
    way for a renderer to invalidate the edge: write the content, then touch
    `site.toml`. On Linux it had never worked, and four watcher tests passed
    throughout.

    It called `filetime::set_file_times`, which is `utimensat(2)` with no open.
    That reports `IN_ATTRIB`. The inotify mask asked for
    `IN_CLOSE_WRITE | IN_CREATE | IN_MOVED_TO`, so the event arrived, was read,
    and was discarded. The mtime-polling fallback that would have caught it runs
    only when there is no watcher fd, and on Linux there always is one.

    Every existing watcher test wrote bytes, so every one of them produced
    `IN_CLOSE_WRITE` and passed against a mask that could not see the case the
    function actually used. The new test writes nothing on purpose, and reverting
    the mask in place on a Linux box confirmed it discriminates: five passed with
    the flag, four passed and one failed without it.

    Two things worth separating here.

    **The obvious one: it worked on macOS.** kqueue registers the watched files
    themselves and reports the attribute change, so the machine the code was
    written on had nothing to show, while the platform that serves production was
    broken. That is the same shape as lesson 33 and it will keep recurring.

    **The one that matters more: `m6-md --touch` worked the whole time.** Its own
    `touch_file` opens the file for write, so blog publishing was fine. One of two
    implementations of the same idea was correct, the working one was the one in
    daily use, and the broken one was the one m6 tells other people to use. **A
    second implementation of a documented mechanism is where the documented
    mechanism goes to rot**, because the copy that gets exercised is not the copy
    that gets recommended.

    It was found the hour after m6's checks started building the examples, which
    is lesson 41 arriving on time: the examples are the only code in the checks
    that uses m6 the way a reader would.

45. **The failure I spent five pushes on was already written down in this
    repository, in the header of `tag.sh`.**

    Pushing was broken and silent: git died with SIGPIPE, exit 141, no message,
    while `check.sh` printed "All checks passed" and the remote never moved. git
    opens its connection, runs the pre-push hook, then sends the pack. The hook ran
    the whole workspace suite single-threaded plus conformance, over ten minutes,
    so the connection sat idle long enough for the server to close it and git wrote
    the pack to a dead socket.

    `tag.sh` says, and has said for some time:

    > The pre-push hook only runs build + tests (~30s). This script is the right
    > place to gate tags on benchmark results because it runs locally before
    > creating the tag, **avoiding SSH-timeout issues that occur when benchmarks
    > run inside the network-push hook.**

    Somebody hit this, diagnosed it correctly, wrote it down, and put the slow work
    somewhere safe. Then the hook grew from ~30 seconds to over ten minutes and
    walked straight back into it. The note was not wrong and it was not hidden; it
    was in a file I had already listed twice that afternoon.

    Two separate things went wrong, and they are worth keeping apart.

    **The first is that a comment protecting an invariant does not protect it.**
    `tag.sh` explained why slow things must stay out of the hook. Nothing stopped
    the hook becoming slow. A rule that lives only in prose beside the workaround,
    rather than in the thing it constrains, decays silently — and this one decayed
    into a repository nobody could push to.

    **The second is mine: I reached for explanations further away than the code I
    had just been editing.** I blamed the network, then credentials, then the
    hook's stdin handling, and fixed the stdin handling — correctly, as it
    happens, and it changed nothing. The cheap discriminating test was
    `git push --no-verify --dry-run`, which cleared the network, the credentials
    and the refusal logic in one command and took four seconds. I ran it fifth.
    **When a tool breaks right after you have edited its hooks, the hook is the
    first hypothesis, and the test that excludes it is the first test.**

    And a third, smaller: four of the five attempts reported success because I
    wrapped the push in compound commands ending in `echo`, so the exit code I
    read belonged to the echo. Lesson 25's shape again, self-inflicted: a
    measurement that cannot fail is not a measurement.

46. **A config that is overridden is indistinguishable from a config that is
    correct.** `m6_core::config::load` merged a secrets file over the base config
    with "src wins on conflict", silently. A deployed renderer config therefore
    read `host = "localhost"`, `port = 1025` and a `from` address, all of them
    inert, while the service relayed through a real provider and sent as a
    different domain entirely.

    The cost was not an outage. It was a **wrong belief**: an audit of where the
    site sends mail from read the deployed config and reported what it said. The
    file was not stale and it was not wrong; it was shadowed, and there is nothing
    in the file to say so.

    The defence that had been argued for the overlap is the interesting part. The
    base config pointed at a dead relay on purpose, so that a missing secrets file
    would fail loudly rather than quietly succeed against the wrong host. **That
    reasoning requires the config to hold a deliberately wrong value, which is
    what made it misleading.** And it was never needed: the service already
    required every field and named the missing one. An absent required key fails
    loudly AND says what is absent. A present-but-wrong one only misleads.

    **When a value must come from somewhere else, leave it out. Do not leave a
    placeholder.** One key, one owner, and a clash is an error.

47. **`test -r` answers a different question from "can I read this".** A deploy
    script verified that the service user could read a TLS private key with
    `sudo -u m6 test -r <key>`, and it reported failure on a key that user reads
    perfectly well. `test -r` resolves through `access(2)`, which answers from the
    traditional permission bits and does not reflect a POSIX ACL. The bits were
    `rw-r----- root root`, so it said no; the ACL was `user:m6:r--` with a
    matching mask, so the read succeeded.

    The certificate was issued correctly and the ACLs were correct. The check was
    wrong, and it failed the run. **Verify by doing the thing, not by asking
    whether it would be permitted** — on an ACL, on a capability, on anything
    where authority is not in the mode bits, the two disagree.

48. **A self-test that checks one case is a self-test for one case.** m6-core has
    a test asserting that no file names one particular deployment, and a companion
    test that the scan can actually find something. The companion checked
    `banned[1]`: one entry of the list.

    So the list could grow an entry that was never exercised, and it could also be
    MISSING one — which it was. The maintainer's second domain was absent from the
    ban, and a personal email address consequently sat in a published
    `SECURITY.md`, on the front page of a public repository, with both tests green
    beside it.

    Widening the ban was the small fix. The real one was making the companion
    iterate every entry, because otherwise the same hole reopens for the next entry
    somebody adds. **A test that proves a guard works must prove it for the whole
    guard.**

49. **A pipeline's exit code belongs to the last command in it.** Lesson 45
    recorded this after four pushes reported success because they ended in `echo`.
    It recurred three times in one day: `./deploy.sh > log 2>&1; echo $?; tail log`
    reported the tail's status, and a `cargo test | grep | tail -20` run reported
    exit 0 while the log contained seven failures.

    The third instance was the worst, because the truncation was also silent: the
    file I read "0 failures" out of was the last twenty lines of *filtered* output,
    so the absence of failures in it meant nothing at all.

    Knowing the lesson is not the same as having the habit. **If the exit code
    matters, nothing goes after the command — redirect to a file, echo `$?` on its
    own line, and read the file.**

50. **A measuring tool can lie in the direction that looks like a server bug, and
    that is the dangerous direction.** m6 1.9.0 installed a TLS session ticketer.
    The fleet read 0% resumed on the browser channel for a day, so `m6-probe-h2`
    was extended to offer a ticket deliberately and report what came back. It
    reported `RESUMPTION: none` against all three production nodes on both
    channels.

    That was the probe. `tls_handshake` stopped reading the instant
    `is_handshaking()` went false, and TLS 1.3 sends `NewSessionTicket` AFTER
    Finished as application-phase data, so rustls never ingested a ticket and had
    nothing to offer on the next connection. Every handshake was full because the
    client never asked to resume.

    The server was correct all along: with the ticket collected, 7 of 8 h2
    handshakes resumed, and the origin's own counter incremented by exactly that
    many. Had the first result been reported, it would have started a hunt for a
    defect in the TLS code.

    What caught it was a **disagreement between two measurements** rather than
    suspicion of the tool: the probe said the h1 channel never resumed while the
    server's counter said 88-96%. Two measurements that disagree are a fact about
    the measurements. The same function already carried lesson 5's scar tissue for
    a missing flush one step earlier in the same sequence, which is worth noticing
    too: a tool can be fixed against one failure mode and still report a property
    of itself as a property of the peer.

51. **A rollout that REMOVES a wire feature must deploy the origin first.** The
    deploy takes edges before the origin, because a broken edge degrades one
    region and rolls back on its own while a broken origin takes every region at
    once. That is right for risk and exactly wrong for a release that drops
    something the origin still sends.

    m6 1.10.0 removed 103 Early Hints. Rolled edges-first, a 1.10.0 edge fetched
    from a 1.8.1 origin that still emitted them, and `forward.rs:1670` lists 204,
    304, 100 and 101 as bodyless and not 103, so the edge mis-framed the stream:
    **502 on about one miss in five**, 8 backend errors in 118 requests, nine of
    them reaching European visitors. Converging the origin next fixed it where it
    stood, because the reverse mismatch is harmless: an old edge understands a
    feature a new origin simply never sends.

    Two general parts. Removing the ability to SEND something is a product
    decision; being unable to RECEIVE it is a conformance defect, and a
    cache-first edge fetches from an origin it does not control (issue #100).
    And **staging cannot catch this class of defect at all**: it converges every
    instance in one run and ends uniform, so a mixed-version fleet is a state it
    never holds. "Staging validated the artefact" was true and said nothing about
    the rollout.
