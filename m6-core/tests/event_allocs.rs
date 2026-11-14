//! The three allocation checks for the loop parts, each on the main thread so
//! the count is the checks' own.
//!
//! `harness = false` means no libtest thread runs in this binary, so
//! [`alloc_count::now`] reads only what these checks allocated.

use std::time::{Duration, Instant};

use m6_core::event::{LoopTrace, Slots, Timeouts};
use m6_core::platform::Token;
use m6_core::testkit::alloc_count;

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

fn main() {
    let mut failed = false;
    failed |= check(
        "timeouts no_allocation_after_new",
        timeouts_no_allocation_after_new,
    );
    failed |= check(
        "slots growth_is_the_only_allocation",
        slots_growth_is_the_only_allocation,
    );
    failed |= check(
        "loop_trace no_allocation_after_new",
        loop_trace_no_allocation_after_new,
    );
    std::process::exit(if failed { 1 } else { 0 });
}

/// Run one check, print its line, and report whether it allocated.
fn check(name: &str, run: impl FnOnce() -> usize) -> bool {
    let n = run();
    println!("{name}: {n} allocation(s)");
    n != 0
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
