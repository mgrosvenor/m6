//! Writing the generation ID costs no allocation on either HTTP/1.1 writer.
//!
//! `harness = false` means no libtest thread runs in this binary, so the
//! counting allocator reads only what these checks allocated. The first check
//! writes a cached hit with `genid: 7` one thousand times into one reused
//! buffer; the second compares one miss-path response with an ID to the same
//! response without one. The harness speaks libtest's protocol through
//! `libtest-mimic`, forced onto one thread so each check runs in this `main`
//! thread.
//!
//! Breaks: format the ID into a `String` and push it as a header, and the
//! first check reports 1000 allocations. Add a second buffer for the ID in the
//! miss writer, and the second check's counts differ.

use std::hint::black_box;
use std::sync::Arc;

use libtest_mimic::{Arguments, Failed, Trial};
use m6_core::testkit::alloc_count;
use m6_http_lib::cache::CachedResponse;
use m6_http_lib::http11::{build_response, write_cached_h1, HitHeaders};

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

fn main() {
    let mut args = Arguments::from_args();
    // One thread: `libtest-mimic` then runs every trial in this (the main)
    // thread, so the counter reads only the trial's own allocations.
    args.test_threads = Some(1);
    let tests = vec![
        Trial::test("a_cached_hit_with_a_generation_allocates_nothing", || {
            outcome(
                "a_cached_hit_with_a_generation_allocates_nothing",
                a_cached_hit_with_a_generation_allocates_nothing(),
            )
        }),
        Trial::test("the_miss_writer_pays_nothing_for_the_generation", || {
            outcome(
                "the_miss_writer_pays_nothing_for_the_generation",
                the_miss_writer_pays_nothing_for_the_generation(),
            )
        }),
    ];
    libtest_mimic::run(&args, tests).exit();
}

fn outcome(name: &str, failed: bool) -> Result<(), Failed> {
    if failed {
        Err(format!("{name}: failed").into())
    } else {
        Ok(())
    }
}

fn a_cached_hit_with_a_generation_allocates_nothing() -> bool {
    let entry = CachedResponse {
        genid: 7,
        status: 200,
        headers: m6_http_lib::cache::HeaderBlock::from_pairs(vec![(
            "content-type".to_string(),
            "text/html; charset=utf-8".to_string(),
        )]),
        body: bytes::Bytes::from_static(b"hello"),
    };
    let hit = HitHeaders {
        alt_svc: Arc::<[u8]>::from(&b"alt-svc: h3=\":443\"; ma=86400"[..]),
        describedby: None,
    };
    let mut out = Vec::with_capacity(4096);
    let start = alloc_count::now();
    for _ in 0..1_000 {
        write_cached_h1(
            &mut out,
            &entry,
            std::time::Duration::ZERO,
            &hit,
            None,
            "GET",
            true,
        );
        out.clear();
    }
    let n = alloc_count::now() - start;
    println!("a_cached_hit_with_a_generation_allocates_nothing: {n} allocation(s) in 1000 hits");
    n != 0
}

fn the_miss_writer_pays_nothing_for_the_generation() -> bool {
    let headers = [(
        "Content-Type".to_string(),
        "text/html; charset=utf-8".to_string(),
    )];
    let body = [b'x'; 1024];
    let start = alloc_count::now();
    black_box(build_response(200, &headers, &body, "GET", true, None));
    let without = alloc_count::now() - start;
    let start = alloc_count::now();
    black_box(build_response(200, &headers, &body, "GET", true, Some(7)));
    let with = alloc_count::now() - start;
    println!("the_miss_writer_pays_nothing_for_the_generation: without={without} with={with}");
    with != without
}
