//! The counting allocator's own guard.
//!
//! Four checks, all on the main thread so the per-thread count is this
//! binary's own: an armed `Vec::with_capacity(10)` counts one call and ten
//! bytes, a disarmed one counts nothing, another thread's allocation is not
//! this thread's, and [`alloc_count::now`] still counts every thread's.
//!
//! `harness = false` and its own `main`, so the counting allocator is installed
//! before any thread starts. The checks speak libtest's protocol through
//! `libtest-mimic`, forced onto one thread so each trial runs in this `main`
//! thread. The idle-wait loops use an atomic flag and `spin_loop` rather than a
//! channel or a sleep: a channel send allocates on the armed thread, which
//! would be counted and would hide the very thing the check measures.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: one thread's allocation is the thing being measured"
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use libtest_mimic::{Arguments, Failed, Trial};
use m6_core::testkit::alloc_count;

#[global_allocator]
static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;

fn main() {
    let mut args = Arguments::from_args();
    // One thread: `libtest-mimic` then runs each trial in this (the main)
    // thread, so the per-thread count is the trial's own.
    args.test_threads = Some(1);
    let tests = vec![
        Trial::test(
            "armed_counts_one_allocation_and_its_bytes",
            armed_counts_one_allocation_and_its_bytes,
        ),
        Trial::test("disarmed_counts_nothing", disarmed_counts_nothing),
        Trial::test(
            "another_threads_allocation_is_not_this_threads",
            another_threads_allocation_is_not_this_threads,
        ),
        Trial::test(
            "now_still_counts_every_thread",
            now_still_counts_every_thread,
        ),
    ];
    libtest_mimic::run(&args, tests).exit();
}

fn armed_counts_one_allocation_and_its_bytes() -> Result<(), Failed> {
    alloc_count::arm();
    let buffer: Vec<u8> = Vec::with_capacity(10);
    let (calls, bytes) = alloc_count::count();
    alloc_count::disarm();
    drop(buffer);

    if calls == 1 && bytes == 10 {
        Ok(())
    } else {
        Err(format!(
            "an armed Vec::with_capacity(10) counted {calls} call(s) and {bytes} byte(s), \
             wanted 1 call and 10 bytes"
        )
        .into())
    }
}

fn disarmed_counts_nothing() -> Result<(), Failed> {
    alloc_count::arm();
    alloc_count::disarm();
    let buffer: Vec<u8> = Vec::with_capacity(10);
    let (calls, bytes) = alloc_count::count();
    drop(buffer);

    if calls == 0 && bytes == 0 {
        Ok(())
    } else {
        Err(format!(
            "a disarmed Vec::with_capacity(10) counted {calls} call(s) and {bytes} byte(s), \
             wanted 0 and 0"
        )
        .into())
    }
}

fn another_threads_allocation_is_not_this_threads() -> Result<(), Failed> {
    let start = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let thread = {
        let start = Arc::clone(&start);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            while !start.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            let buffer: Vec<u8> = Vec::with_capacity(10);
            done.store(true, Ordering::Release);
            drop(buffer);
        })
    };

    // The spawn above allocated, and its bookkeeping is not in the count
    // because the arm below clears the cells.
    alloc_count::arm();
    start.store(true, Ordering::Release);
    while !done.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
    let (calls, bytes) = alloc_count::count();
    alloc_count::disarm();
    thread.join().expect("the allocating thread");

    if calls == 0 && bytes == 0 {
        Ok(())
    } else {
        Err(format!(
            "another thread's Vec::with_capacity(10) was counted on this thread: \
             {calls} call(s) and {bytes} byte(s), wanted 0 and 0"
        )
        .into())
    }
}

fn now_still_counts_every_thread() -> Result<(), Failed> {
    let start = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let thread = {
        let start = Arc::clone(&start);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            while !start.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            let buffer: Vec<u8> = Vec::with_capacity(10);
            done.store(true, Ordering::Release);
            drop(buffer);
        })
    };

    // Taken after the spawn, so the only allocations between the two reads are
    // the other thread's own.
    let before = alloc_count::now();
    start.store(true, Ordering::Release);
    while !done.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
    let after = alloc_count::now();
    thread.join().expect("the allocating thread");

    if after > before {
        Ok(())
    } else {
        Err(format!(
            "now() read {before} before another thread's allocation and {after} after it, \
             so it did not see that thread"
        )
        .into())
    }
}
