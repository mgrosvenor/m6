//! Counts heap allocations, to prove a run loop makes none.
//!
//! `alloc` and `realloc` each bump the count; `dealloc` does not. Test
//! instrumentation only: a test binary or example opts in by declaring its own
//! global allocator, and the m6-core library never installs one. Planner B's
//! ALLOC-0 and LOG-1 use this same counter.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The counting allocator, to install with `#[global_allocator]`.
pub struct Counting;

static COUNT: AtomicUsize = AtomicUsize::new(0);

#[allow(unsafe_code)] // replacing the allocator is unsafe by definition: test instrumentation only
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNT.fetch_add(1, Ordering::Relaxed);
        record_trace();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        COUNT.fetch_add(1, Ordering::Relaxed);
        record_trace();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Heap allocations so far (`alloc` and `realloc`, not `dealloc`).
pub fn now() -> usize {
    COUNT.load(Ordering::Relaxed)
}

/// Records a short backtrace per allocation while a test asks where the
/// allocations on one request come from.
///
/// Behind the off-by-default `alloc-trace` feature, so a release build and the
/// ordinary test build compile none of it. The collection is in a test-only
/// test target (`m6-core/tests/alloc_sites.rs`), which turns it on around the
/// measured window and groups each allocation by the first frame in `m6_core`
/// or `m6_http`.
#[cfg(feature = "alloc-trace")]
pub mod trace {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// Whether `record` should look at a backtrace. Off except inside a
    /// measured window.
    static RECORDING: AtomicBool = AtomicBool::new(false);

    /// Re-entrancy guard: capturing and formatting a backtrace allocates, and
    /// those allocations must not try to record a backtrace of their own.
    static INSIDE: AtomicBool = AtomicBool::new(false);

    /// Each recorded allocation's grouping key, one entry per allocation.
    static SITES: Mutex<Vec<(String, usize)>> = Mutex::new(Vec::new());

    /// Start a measured window, clearing anything recorded before it.
    pub fn start() {
        SITES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        RECORDING.store(true, Ordering::SeqCst);
    }

    /// End a measured window and return its groups, most frequent first.
    pub fn stop() -> Vec<(String, usize)> {
        RECORDING.store(false, Ordering::SeqCst);
        let mut sites = std::mem::take(
            &mut *SITES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        sites.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        sites
    }

    /// Called from `Counting`'s `alloc` and `realloc`. Never called when the
    /// feature is off, because `record_trace` is a no-op empty function then.
    pub(super) fn record() {
        if !RECORDING.load(Ordering::Relaxed) {
            return;
        }
        if INSIDE.swap(true, Ordering::SeqCst) {
            return;
        }
        let text = std::backtrace::Backtrace::force_capture().to_string();
        if let Some(site) = first_m6_frame(&text) {
            SITES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((site, 1));
        }
        INSIDE.store(false, Ordering::SeqCst);
    }

    /// The first frame in the captured backtrace that is inside `m6-core` or
    /// `m6-http`, skipping the counting allocator's own frames, as a
    /// `function (file:line)` grouping key.
    ///
    /// `Backtrace`'s `Display` prints one `   0: function` line per frame,
    /// followed by an `at file:line:column` line when a location is known.
    fn first_m6_frame(text: &str) -> Option<String> {
        let mut function: Option<&str> = None;
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("at ") {
                if let Some(function) = function {
                    if is_m6_call_site(function, rest) {
                        let location = rest.rsplit_once('/').map_or(rest, |(_, file)| file);
                        // Group by function and file, not line: a merge moves
                        // line numbers, and the diff is about which call site
                        // allocates, not where it sits.
                        let file = location.split(':').next().unwrap_or(location);
                        return Some(format!("{function} ({file})"));
                    }
                }
                function = None;
            } else if let Some((index, rest)) = line.split_once(": ") {
                if index.trim().parse::<usize>().is_ok() {
                    function = Some(rest.trim());
                }
            }
        }
        None
    }

    fn is_m6_call_site(function: &str, location: &str) -> bool {
        let inside = function.contains("m6_core")
            || function.contains("m6_http")
            || location.starts_with("./src/")
            || location.starts_with("src/")
            || location.contains("m6-core/src/")
            || location.contains("m6-http/src/");
        let is_counter =
            function.contains("alloc_count") || location.contains("testkit/alloc_count.rs");
        inside && !is_counter
    }
}

#[cfg(feature = "alloc-trace")]
fn record_trace() {
    trace::record();
}

#[cfg(not(feature = "alloc-trace"))]
#[inline(always)]
fn record_trace() {}
