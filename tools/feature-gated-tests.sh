#!/usr/bin/env bash
# feature-gated-tests.sh — every test target that `cargo test --workspace` does
# not build, named with the feature that makes it visible.
#
# ── Why this file exists ─────────────────────────────────────────────────────
#
# `cargo test --workspace` builds only the targets whose `required-features` the
# workspace turns on. Any other target is omitted, not run: it is invisible in
# the suite and cannot fail it. A target can therefore rot for weeks while every
# local run is green.
#
# THAT IS WHAT HAPPENED. `h1_tls_miss_insert` in `m6-http/tests/allocations.rs`
# sits behind `required-features = ["count-allocations"]`, which is off by
# default. `cargo test --workspace` therefore neither built nor ran it, and it
# reported 2,641 passing tests, 0 failing, while CI ran the same target with the
# feature on and failed on all three of its machines. The local gate and CI were
# running different suites, and the local one was the smaller.
#
# So the set lives here once. CI, `check.sh` and `tools/build-host-tests.sh` all
# call this script, and adding a `required-features` test target without adding
# it here is the same defect again.
#
# ── The rule ─────────────────────────────────────────────────────────────────
#
# Every `[[test]]` in the workspace whose `required-features` is not satisfied by
# another member's dev-dependency belongs here, named exactly as `ci.yml` runs
# it. The m6-core `testkit` targets are already visible through m6-html's and
# m6-http's dev-dependencies and are named anyway, so removing one of those
# requests cannot hide them silently.
#
# A feature that changes the allocator (the counting and attribution features)
# must be named per package: `--all-features` on the workspace would turn on
# `alloc-attribution` as well as `count-allocations` for every target, and the
# dhat profiler is not what the ordinary suite should run under.
#
# Usage: tools/feature-gated-tests.sh
set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> feature-gated tests (the targets cargo test --workspace omits)"

# `testkit` also turns on `alloc-count`, so the counting tests build.
cargo test -p m6-core --features testkit \
  --test alloc_count_armed \
  --test event_allocs \
  --test limit_page_allocs \
  --test server_conn \
  --test site_allocs \
  --test upstream_no_alloc

# `email` makes these two visible, `testkit` is the harness they use.
cargo test -p m6-core --features email,testkit \
  --test email_allocs \
  --test email_in_the_loop

# `count-allocations` builds the counting allocator into the binary the test
# reads. The budgets in that test are ceilings: a failure means a counted path
# started allocating more, and the path is fixed rather than the budget raised.
cargo test -p m6-http --features count-allocations --test allocations

# `count-allocations` builds the counter into m6-monitor's binary.
cargo test -p m6-monitor --features count-allocations --test client_allocs
