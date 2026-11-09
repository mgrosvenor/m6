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

/// Heap allocations so far (`alloc` and `realloc`, not `dealloc`).
pub fn now() -> usize {
    COUNT.load(Ordering::Relaxed)
}
