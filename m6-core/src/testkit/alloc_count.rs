//! Counts heap allocations, to prove a run loop makes none.
//!
//! `alloc` and `realloc` each bump the process-wide count; `dealloc` does not.
//! Test instrumentation only: a test binary or example opts in by declaring its
//! own global allocator, and the m6-core library never installs one. Planner B's
//! ALLOC-0 and LOG-1 use this same counter.
//!
//! # Two counters
//!
//! [`now`] is process-wide and counts `alloc` and `realloc` from every thread.
//! That is how a test binary proves a whole program made no allocation, and its
//! meaning has not changed.
//!
//! [`arm`], [`disarm`] and [`count`] are per thread. While a thread is armed,
//! its own `alloc`, `alloc_zeroed` and `realloc` calls and bytes go into that
//! thread's cells, and another thread's do not. That is what ALLOC-0 needs: a
//! service's run loop arms once when it starts and reads its own count once at
//! stop, so the number is the loop's, not the process's.
//!
//! The cells are `const`-initialised, so reading one never allocates and the
//! allocator cannot recurse into itself. Every access is a `try_with`: an
//! allocation during thread-local teardown has no counter left to bump and
//! must not panic.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The counting allocator, to install with `#[global_allocator]`.
pub struct Counting;

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

#[allow(unsafe_code)] // replacing the allocator is unsafe by definition: test instrumentation only
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNT.fetch_add(1, Ordering::Relaxed);
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // Not added to COUNT: `now` has always meant `alloc` and `realloc`,
        // and changing it would change every earlier counting test's number.
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        COUNT.fetch_add(1, Ordering::Relaxed);
        record(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Heap allocations so far (`alloc` and `realloc`, not `dealloc`).
pub fn now() -> usize {
    COUNT.load(Ordering::Relaxed)
}

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
