//! The rendered-page working area's allocation rules.
//!
//! One `Bounded<u8>` is made at the `render_page` start when the service
//! starts and reused for every page. A page under the start allocates nothing
//! for it, a page over the start grows it once, and the pages after that, of
//! any size up to the grown one, allocate nothing for it. Each check runs on
//! the main thread so the count is the check's own.

use std::io::Write;
use std::path::Path;

use libtest_mimic::{Arguments, Failed, Trial};
use m6_core::limits::{Bounded, Limits, RENDER_PAGE};
use m6_core::platform::process::page_size;
use m6_core::testkit::alloc_count;

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

const KIB: usize = 1024;

fn main() {
    let mut args = Arguments::from_args();
    args.test_threads = Some(1);
    let tests = vec![
        Trial::test("a_page_under_start_allocates_nothing", || {
            check(under_start)
        }),
        Trial::test("an_oversized_page_grows_the_area_once", || {
            check(oversized_grows_once)
        }),
        Trial::test("the_next_pages_of_any_size_allocate_nothing", || {
            check(next_pages)
        }),
    ];
    libtest_mimic::run(&args, tests).exit();
}

fn check(run: impl FnOnce() -> Result<(), String>) -> Result<(), Failed> {
    run().map_err(Failed::from)
}

/// The default area: 512 KiB at start, 4 MiB at most.
fn area() -> Bounded<u8> {
    let limits = Limits::from_toml(None, Path::new("site.toml"), page_size()).unwrap();
    Bounded::new(limits.get(&RENDER_PAGE))
}

/// A page written the way a renderer writes: many small pieces.
fn write_page(area: &mut Bounded<u8>, bytes: usize) {
    area.clear();
    let piece = [b'x'; 997];
    let mut left = bytes;
    while left > 0 {
        let n = left.min(piece.len());
        area.write_all(&piece[..n]).expect("within max");
        left -= n;
    }
}

fn counted(what: &str, want: usize, run: impl FnOnce()) -> Result<(), String> {
    let before = alloc_count::now();
    run();
    let got = alloc_count::now() - before;
    println!("{what}: {got} allocation(s)");
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: {got} allocation(s), wanted {want}"))
    }
}

fn under_start() -> Result<(), String> {
    let mut area = area();
    counted("100 pages of 200 KiB", 0, || {
        for _ in 0..100 {
            write_page(&mut area, 200 * KIB);
        }
    })?;
    if area.capacity() != 512 * KIB {
        return Err(format!("capacity moved to {}", area.capacity()));
    }
    Ok(())
}

fn oversized_grows_once() -> Result<(), String> {
    let mut area = area();
    counted("one 600 KiB page", 1, || write_page(&mut area, 600 * KIB))?;
    if area.capacity() != 1024 * KIB {
        return Err(format!("capacity is {}, wanted 1 MiB", area.capacity()));
    }
    Ok(())
}

fn next_pages() -> Result<(), String> {
    let mut area = area();
    write_page(&mut area, 600 * KIB);
    counted("a 600 KiB page, a small one, a 1 MiB one", 0, || {
        write_page(&mut area, 600 * KIB);
        write_page(&mut area, 10 * KIB);
        write_page(&mut area, 1024 * KIB);
    })?;
    if area.capacity() != 1024 * KIB {
        return Err(format!("capacity is {}, wanted 1 MiB", area.capacity()));
    }
    Ok(())
}
