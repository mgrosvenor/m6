//! After init, 100,000 lines at every level through the macros and through
//! `Line` allocate nothing.
//!
//! The counting allocator is process-wide, so this test is its own binary with
//! `harness = false`. Its `main` runs on one thread and exits non-zero when
//! anything allocated.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

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
    let devnull = std::fs::File::options()
        .write(true)
        .open("/dev/null")
        .expect("/dev/null");
    nix::unistd::dup2_stdout(&devnull).expect("stdout is /dev/null");
    m6_log::init(Level::Trace, Some(std::path::Path::new("/dev/null"))).expect("the logger");

    let before = COUNT.load(Ordering::Relaxed);
    for n in 0..LINES {
        m6_log::error!(n = n, "line");
        m6_log::warn!(n = n, "line");
        m6_log::info!(n = n, "line");
        m6_log::debug!(n = n, "line");
        m6_log::trace!(n = n, "line");
        with_analytics(|sink| {
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
    }
    let allocated = COUNT.load(Ordering::Relaxed) - before;
    m6_log::stop().expect("a clean stop");
    eprintln!(
        "no_allocation: {allocated} allocation(s) in {} lines",
        LINES * 6
    );
    if allocated != 0 {
        std::process::exit(1);
    }
}
