//! Counts heap allocations, to prove a run loop makes none.
//!
//! `alloc` and `realloc` each bump the process-wide count; `dealloc` does not.
//! Test instrumentation only: [`crate::alloc::M6Alloc`] calls these hooks when
//! the `alloc-count` feature is on, and compiles them out when it is off. The
//! m6-core library never installs a global allocator. The allocation guard and
//! the logging work use this same counter.
//!
//! # Two counters
//!
//! [`now`] is process-wide and counts `alloc` and `realloc` from every thread.
//! That is how a test binary proves a whole program made no allocation, and its
//! meaning has not changed.
//!
//! [`arm`], [`disarm`] and [`count`] are per thread. While a thread is armed,
//! its own `alloc`, `alloc_zeroed` and `realloc` calls and bytes go into that
//! thread's cells, and another thread's do not. That is what the allocation
//! guard needs: a service's run loop arms once when it starts and reads its own
//! count once at stop, so the number is the loop's, not the process's.
//!
//! The cells are `const`-initialised, so reading one never allocates and the
//! allocator cannot recurse into itself. Every access is a `try_with`: an
//! allocation during thread-local teardown has no counter left to bump and
//! must not panic.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Whether this thread is counting. False until its first [`arm`].
    static ARMED: Cell<bool> = const { Cell::new(false) };
    /// This thread's allocation calls since its last [`arm`].
    static CALLS: Cell<u64> = const { Cell::new(0) };
    /// This thread's allocated bytes since its last [`arm`].
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

/// Add one allocation of `bytes` to this thread's count, when it is armed.
///
/// Called from the allocator itself, so it never allocates, panics or takes a
/// lock. A destroyed or uninitialised thread-local answers `Err` and the
/// allocation is simply not this thread's.
fn record(bytes: usize) {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let _ = CALLS.try_with(|cell| cell.set(cell.get() + 1));
            let _ = BYTES.try_with(|cell| cell.set(cell.get() + bytes as u64));
        }
    });
}

/// Called by [`crate::alloc::M6Alloc`] on each `alloc`.
pub(crate) fn on_alloc(bytes: usize) {
    COUNT.fetch_add(1, Ordering::Relaxed);
    record_trace();
    record(bytes);
}

/// Called on each `alloc_zeroed`. Not added to COUNT: `now` has always meant
/// `alloc` and `realloc`, and changing it would change every earlier counting
/// test's number.
pub(crate) fn on_alloc_zeroed(bytes: usize) {
    record(bytes);
}

/// Called on each `realloc`, with the new size.
pub(crate) fn on_realloc(new_size: usize) {
    COUNT.fetch_add(1, Ordering::Relaxed);
    record_trace();
    record(new_size);
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

    /// Called from `M6Alloc`'s `alloc` and `realloc`. Never called when the
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

/// Start counting this thread's allocations, from zero.
///
/// The counters are cleared first, so a second arm on the same thread reports
/// only what happened after it. Allocations made by `arm` itself are not
/// counted.
pub fn arm() {
    let _ = CALLS.try_with(|calls| calls.set(0));
    let _ = BYTES.try_with(|bytes| bytes.set(0));
    let _ = ARMED.try_with(|armed| armed.set(true));
}

/// Stop counting this thread's allocations. [`count`] keeps the last reading.
pub fn disarm() {
    let _ = ARMED.try_with(|armed| armed.set(false));
}

/// This thread's `(calls, bytes)` since its last [`arm`].
///
/// A thread that never armed, or whose thread-locals are gone, reads `(0, 0)`.
/// A `realloc` counts its new size, the memory the caller now holds.
pub fn count() -> (u64, u64) {
    let calls = CALLS.try_with(|calls| calls.get()).unwrap_or(0);
    let bytes = BYTES.try_with(|bytes| bytes.get()).unwrap_or(0);
    (calls, bytes)
}
