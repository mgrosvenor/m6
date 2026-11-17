//! The three allocation checks for the loop parts, each on the main thread so
//! the count is the checks' own.
//!
//! `harness = false` means no libtest thread runs in this binary, so
//! [`alloc_count::now`] reads only what these checks allocated. A custom
//! harness must still speak libtest's protocol (`--list --format terse`,
//! `--exact <name>`), which `libtest-mimic` does; and the checks are forced
//! onto one thread so each runs in this `main` thread.

use std::time::{Duration, Instant};

use libtest_mimic::{Arguments, Failed, Trial};
use m6_core::event::{LoopTrace, Slots, Timeouts};
use m6_core::platform::Token;
use m6_core::testkit::alloc_count;

#[global_allocator]
static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;

fn main() {
    let mut args = Arguments::from_args();
    // One thread: `libtest-mimic` then runs every trial in this (the main)
    // thread, so the counting allocator reads only the trial's own
    // allocations and no other thread's.
    args.test_threads = Some(1);
    let tests = vec![
        Trial::test("timeouts::no_allocation_after_new", || {
            check(
                "timeouts::no_allocation_after_new",
                timeouts_no_allocation_after_new,
            )
        }),
        Trial::test("slots::growth_is_the_only_allocation", || {
            check(
                "slots::growth_is_the_only_allocation",
                slots_growth_is_the_only_allocation,
            )
        }),
        Trial::test("loop_trace::no_allocation_after_new", || {
            check(
                "loop_trace::no_allocation_after_new",
                loop_trace_no_allocation_after_new,
            )
        }),
    ];
    libtest_mimic::run(&args, tests).exit();
}

/// Run one check, print its line, and report whether it allocated.
fn check(name: &str, run: impl FnOnce() -> usize) -> Result<(), Failed> {
    let n = run();
    println!("{name}: {n} allocation(s)");
    if n == 0 {
        Ok(())
    } else {
        Err(format!("{name} allocated {n} time(s)").into())
    }
}

fn timeouts_no_allocation_after_new() -> usize {
    let mut t = Timeouts::new(1024, Duration::from_secs(10));
    let now = Instant::now();
    let start = alloc_count::now();
    for i in 0..1_000usize {
        let k = Token(i % 1024);
        t.touch(k, now);
        let _ = t.timeout(now, Duration::from_secs(1));
        if i % 3 == 0 {
            t.clear(k);
        }
        let _ = t.expired(now);
    }
    alloc_count::now() - start
}

fn slots_growth_is_the_only_allocation() -> usize {
    let mut s = Slots::new(64, 64, || 0u8);
    let start = alloc_count::now();
    for _ in 0..10_000 {
        let taken = s.take().expect("within the initial size");
        s.free(taken.key);
    }
    alloc_count::now() - start
}

fn loop_trace_no_allocation_after_new() -> usize {
    let mut trace = LoopTrace::new(&["a", "b"]);
    let start = alloc_count::now();
    for _ in 0..1_000 {
        trace.waited(Duration::from_millis(1));
        trace.worked(0, Duration::from_millis(2));
        trace.worked(1, Duration::from_millis(3));
    }
    let _ = trace.report();
    alloc_count::now() - start
}
