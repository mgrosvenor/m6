//! The allocator every m6 binary and every allocation-counting test installs.
//!
//! [`M6Alloc`] is mimalloc. Under the `alloc-count` feature it also counts each
//! call the way [`crate::testkit::alloc_count`] reads them. Without the feature
//! the counting is not compiled, so a service pays nothing for it. A test counts
//! against the allocator production runs, not against another one.
//!
//! A binary declares it, and a library never does, because a program has one
//! global allocator:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;
//! ```
//!
//! # Kernel memory calls
//!
//! The cost this removes is the call into the kernel (`brk`, `mmap`, `munmap`,
//! `madvise`), so a warm service makes none:
//!
//! - [`never_return`] stops mimalloc handing freed memory back, so the heap
//!   grows and never shrinks. Call it first in `main`.
//! - [`reserve`] takes `[limits.heap_reserve]` bytes from the OS once, at start,
//!   committed and on huge pages where the kernel has them, so allocation
//!   inside it is a pointer move and no `mprotect`.
//! - Past the reservation mimalloc still asks the OS for more. That is growth,
//!   not a failure; [`note_growth`] logs the first time.
//!
//! The options are set in code through mimalloc's own option API, not through
//! `MIMALLOC_*` environment variables an operator could forget.

use std::alloc::{GlobalAlloc, Layout};
use std::os::raw::c_long;
use std::sync::atomic::{AtomicUsize, Ordering};

use libmimalloc_sys as mi;
use mimalloc::MiMalloc;

/// mimalloc's `mi_option_purge_delay`, the same number in its v2 and v3
/// headers, and not among the constants the Rust binding exports. A test reads
/// the option back, so a header that moves it fails the build.
const PURGE_DELAY: mi::mi_option_t = 15;

/// mimalloc, counting under the `alloc-count` feature.
pub struct M6Alloc;

#[allow(unsafe_code)] // a global allocator is an unsafe trait by definition
unsafe impl GlobalAlloc for M6Alloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        #[cfg(feature = "alloc-count")]
        crate::testkit::alloc_count::on_alloc(layout.size());
        unsafe { MiMalloc.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { MiMalloc.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        #[cfg(feature = "alloc-count")]
        crate::testkit::alloc_count::on_alloc_zeroed(layout.size());
        unsafe { MiMalloc.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        #[cfg(feature = "alloc-count")]
        crate::testkit::alloc_count::on_realloc(new_size);
        unsafe { MiMalloc.realloc(ptr, layout, new_size) }
    }
}

/// Never return freed memory to the OS: the heap grows and does not shrink.
///
/// Call it first in `main`, before the first allocation that matters. mimalloc
/// reads the option each time it would purge, so a call before the service's
/// own allocations is early enough.
#[allow(unsafe_code)] // FFI into mimalloc's option table
pub fn never_return() {
    // SAFETY: sets one integer option; mimalloc documents setting options at start.
    unsafe { mi::mi_option_set(PURGE_DELAY, -1 as c_long) };
}

/// Bytes reserved by [`reserve`], zero before it runs or when it failed.
static RESERVED: AtomicUsize = AtomicUsize::new(0);

/// Take `size` bytes from the OS once, committed, for mimalloc to allocate
/// from, on huge pages where the kernel offers them and on normal pages where it
/// does not. Committed so the first touch is a page fault and never an
/// `mprotect`; the kernel backs the pages only as they are used.
///
/// Returns whether the reservation succeeded. A refusal by the OS is reported on
/// stderr and is not fatal: mimalloc then goes to the OS as it needs, which is
/// what happens without a reservation.
#[allow(unsafe_code)] // FFI into mimalloc's arena API
pub fn reserve(size: usize) -> bool {
    // SAFETY: an arena id is an integer or a pointer; zero is a valid
    // placeholder for mimalloc to overwrite.
    let mut arena: mi::mi_arena_id_t = unsafe { std::mem::zeroed() };
    // SAFETY: `arena` outlives the call and mimalloc writes one id into it.
    // `allow_large` asks for huge pages; mimalloc falls back to normal pages
    // when the kernel has none to give.
    let status = unsafe { mi::mi_reserve_os_memory_ex(size, true, true, false, &mut arena) };
    if status == 0 {
        RESERVED.store(size, Ordering::Relaxed);
        true
    } else {
        // The log may not be open yet, so this goes to stderr like a limit's
        // rounding warning does.
        eprintln!(
            "could not reserve {} MiB for the heap, [limits.heap_reserve] start (error {status}); the heap will grow from the OS as it is used",
            size / (1024 * 1024)
        );
        false
    }
}

/// Log once if the heap has grown past [`reserve`]'s reservation. Cheap, and
/// meant for a service's existing accounting tick, not for a timer of its own.
#[allow(unsafe_code)] // FFI into mimalloc's process statistics
pub fn note_growth() {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let reserved = RESERVED.load(Ordering::Relaxed);
    if reserved == 0 || LOGGED.load(Ordering::Relaxed) {
        return;
    }
    let mut commit = 0usize;
    let mut zero = 0usize;
    // SAFETY: every pointer is to a live local, or null where mimalloc skips it.
    unsafe {
        mi::mi_process_info(
            &mut zero,
            &mut zero,
            &mut zero,
            &mut zero,
            &mut zero,
            &mut commit,
            &mut zero,
            &mut zero,
        )
    };
    if commit > reserved && !LOGGED.swap(true, Ordering::Relaxed) {
        crate::log::info!(
            "heap grew past [limits.heap_reserve] ({} reserved, {} committed); raise start to reserve more",
            reserved,
            commit
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `never_return` makes the purge delay -1, which is "never purge". The
    /// default is a delay of zero or more milliseconds, and it differs by
    /// platform. The strace procedure in `tools/memcalls.sh` is the end-to-end
    /// proof that option 15 is the right one.
    #[test]
    fn never_return_sets_the_purge_delay() {
        #[allow(unsafe_code)]
        let read = || unsafe { mi::mi_option_get(PURGE_DELAY) };
        assert!(read() >= 0, "option 15 starts as a delay, not as never");
        never_return();
        assert_eq!(read(), -1);
    }
}
