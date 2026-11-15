//! The cache node's store rule, its hit writer and its generation swap cost no
//! per-request allocation.
//!
//! `harness = false` means no libtest thread runs in this binary, so the
//! counting allocator reads only what these checks allocated. The first check
//! calls `genid::Follower::may_store` two thousand times: one thousand replies
//! carry the ID the follower holds, one thousand carry another ID. The header
//! is a borrowed byte slice, and the rule parses it in place, so the count is
//! zero. The second writes a cached hit with an ID one thousand times into one
//! reused buffer, which is the path a cache node answers from while its
//! upstream is down. The third swaps a 4096-entry generation and measures a
//! constant cost: the replacement is built empty and published, so the swap
//! does not copy the entries. The harness speaks libtest's protocol through
//! `libtest-mimic`, forced onto one thread so each check runs in this `main`
//! thread.
//!
//! Breaks: copy the header into a `String` before parsing it, and the first
//! check reports two thousand allocations. Format the stored ID into a
//! `String` and push it as a header, and the second reports one per hit.
//! Build the replacement generation by re-inserting every entry, and the
//! third reports one allocation or more per entry.

use std::hint::black_box;
use std::sync::Arc;

use libtest_mimic::{Arguments, Failed, Trial};
use m6_core::testkit::alloc_count;
use m6_http_lib::cache::{Cache, CacheKey, CachedResponse};
use m6_http_lib::genid::{Follower, Outcome};
use m6_http_lib::http11::{write_cached_h1, HitHeaders};

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

fn main() {
    let mut args = Arguments::from_args();
    // One thread: `libtest-mimic` then runs every trial in this (the main)
    // thread, so the counter reads only the trial's own allocations.
    args.test_threads = Some(1);
    let tests = vec![
        Trial::test("a_cache_node_miss_allocates_nothing_new", || {
            outcome(
                "a_cache_node_miss_allocates_nothing_new",
                a_cache_node_miss_allocates_nothing_new(),
            )
        }),
        Trial::test("a_cache_node_hit_allocates_nothing", || {
            outcome(
                "a_cache_node_hit_allocates_nothing",
                a_cache_node_hit_allocates_nothing(),
            )
        }),
        Trial::test(
            "a_generation_swap_allocates_a_constant_number_of_times",
            || {
                outcome(
                    "a_generation_swap_allocates_a_constant_number_of_times",
                    a_generation_swap_allocates_a_constant_number_of_times(),
                )
            },
        ),
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

fn a_cache_node_miss_allocates_nothing_new() -> bool {
    let now = m6_core::platform::now();
    let mut follower = Follower::new(std::time::Duration::from_secs(10), now);
    assert_eq!(
        follower.answered(Ok((7, std::time::Duration::ZERO)), now),
        Outcome::First(7),
        "the follower holds ID 7 before the check starts"
    );
    let start = alloc_count::now();
    for _ in 0..1_000 {
        black_box(follower.may_store(Some(b"7")));
    }
    for _ in 0..1_000 {
        black_box(follower.may_store(Some(b"8")));
    }
    let n = alloc_count::now() - start;
    println!("a_cache_node_miss_allocates_nothing_new: {n} allocation(s) in 2000 store checks");
    n != 0
}

fn a_cache_node_hit_allocates_nothing() -> bool {
    let entry = CachedResponse {
        genid: 7,
        status: 200,
        headers: Arc::new(vec![(
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
    println!("a_cache_node_hit_allocates_nothing: {n} allocation(s) in 1000 hits");
    n != 0
}

/// A generation swap is a publish, not a copy: 4096 entries cost the same two
/// allocations (the replacement's map handle and its byte counter) as an empty
/// one. The count starts after the entries are stored.
///
/// Break: build the replacement by re-inserting every entry, and the count
/// grows with the 4096 entries.
fn a_generation_swap_allocates_a_constant_number_of_times() -> bool {
    let mut cache = Cache::new();
    let body = bytes::Bytes::from_static(b"old generation");
    for i in 0..4096 {
        cache.insert(
            CacheKey::new(&format!("/p{i}"), None, "gzip"),
            CachedResponse {
                genid: 7,
                status: 200,
                headers: Arc::new(vec![]),
                body: body.clone(),
            },
        );
    }
    assert_eq!(cache.len(), 4096, "the check starts with a full generation");

    let start = alloc_count::now();
    let old = cache.swap_generation();
    let n = alloc_count::now() - start;
    drop(old);

    println!(
        "a_generation_swap_allocates_a_constant_number_of_times: {n} allocation(s) for a 4096-entry swap"
    );
    n > 2
}
