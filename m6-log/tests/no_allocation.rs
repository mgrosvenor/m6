//! After init, 100,000 lines at every level through the macros and through
//! `Line` allocate nothing.
//!
//! The counting allocator is process-wide, so this test is its own binary with
//! `harness = false`. The harness speaks libtest's protocol through
//! `libtest-mimic`, forced onto one thread: the check runs in `main` and fails
//! when anything allocated.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use libtest_mimic::{Arguments, Failed, Trial};
use m6_log::{with_analytics, Level, Line};

struct Counting;

static COUNT: AtomicUsize = AtomicUsize::new(0);

// Test instrumentation: the only unsafe in this crate's tests, and the reason
// the primer allows it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const LINES: u64 = 100_000;

fn main() {
    let mut args = Arguments::from_args();
    // One thread: `libtest-mimic` then runs the trial in this (the main)
    // thread, so the counting allocator reads only the trial's own
    // allocations and no other thread's.
    args.test_threads = Some(1);
    let tests = vec![Trial::test(
        "no_allocation_after_init",
        no_allocation_after_init,
    )];
    libtest_mimic::run(&args, tests).exit();
}

fn no_allocation_after_init() -> Result<(), Failed> {
    let devnull = std::fs::File::options()
        .write(true)
        .open("/dev/null")
        .expect("/dev/null");
    m6_platform::fd::dup2_stdout(&devnull).expect("stdout is /dev/null");
    m6_log::init(Level::Trace, Some(std::path::Path::new("/dev/null"))).expect("the logger");

    let before = COUNT.load(Ordering::Relaxed);
    for n in 0..LINES {
        m6_log::error!(n = n, "line");
        m6_log::warn!(n = n, "line");
        m6_log::info!(n = n, "line");
        m6_log::debug!(n = n, "line");
        m6_log::trace!(n = n, "line");
        let accepted = with_analytics(|sink| {
            Line::begin(sink, Level::Info, "request")
                .str("node", "dev")
                .str("path", "/capabilities")
                .u64("status", 200)
                .str("cache_state", "HIT")
                .str("client_ip", "192.0.2.1")
                .str("session_id", "0f1e2d3c4b5a69788796a5b4c3d2e1f0")
                .bool("session_new", true)
                .str("user_agent", "curl/8.7.1")
                .u64("latency_ns", 3360)
                .end();
        });
        assert!(accepted, "the analytics Sink was not open");
    }
    let allocated = COUNT.load(Ordering::Relaxed) - before;
    let operational_lines = m6_log::operational_lines();
    let operational_written = m6_log::operational_written();
    let operational_dropped = m6_log::operational_dropped();
    let analytics_lines = m6_log::analytics_lines();
    let analytics_dropped = m6_log::analytics_dropped();
    let analytics_written = m6_log::analytics_written();
    m6_log::stop().expect("a clean stop");
    eprintln!(
        "no_allocation: {allocated} allocation(s) in {} lines",
        LINES * 6
    );
    assert_eq!(
        operational_lines,
        LINES * 5,
        "the macro lines did not reach the operational Sink"
    );
    assert_eq!(
        analytics_lines, LINES,
        "the analytics lines were not all written"
    );
    assert_eq!(operational_dropped, 0, "operational lines were dropped");
    assert_eq!(analytics_dropped, 0, "analytics lines were dropped");
    assert!(
        operational_written > 0,
        "the operational Sink wrote nothing"
    );
    assert!(analytics_written > 0, "the analytics Sink wrote nothing");
    if allocated == 0 {
        Ok(())
    } else {
        Err(format!("{allocated} allocation(s) in {} lines", LINES * 6).into())
    }
}
