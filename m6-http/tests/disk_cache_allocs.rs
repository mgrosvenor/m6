//! Storing an entry for the disk writer allocates nothing extra.
//!
//! `harness = false` means no libtest thread runs in this binary, so the
//! counting allocator reads only what these checks allocated. `main` forces
//! `libtest-mimic` onto one thread, and the library is exercised through the
//! same `Cache::insert_queued` the request path calls. A thousand keys are made,
//! stored, and their replacement responses prepared before counting. The
//! measured loop stores the same thousand keys again with the writer's flag
//! set: the key already exists in the map, so the insert replaces an entry and
//! grows nothing. The flag is set in that same insert, which is why the request
//! path copies no key and pushes nothing.
//!
//! Breaks: return `Cache` to cloning the key for a writer ring, and the count
//! is one thousand. Store the key's bytes a second time in a queue, and it is
//! one thousand again.

use libtest_mimic::{Arguments, Failed, Trial};
use m6_core::testkit::alloc_count;
use m6_http_lib::cache::{Cache, CacheKey, CachedResponse, HeaderBlock};

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

fn main() {
    let mut args = Arguments::from_args();
    // One thread: `libtest-mimic` then runs every trial in this (the main)
    // thread, so the counter reads only the trial's own allocations.
    args.test_threads = Some(1);
    let tests = vec![
        Trial::test("storing_for_the_writer_allocates_nothing", || {
            outcome(
                "storing_for_the_writer_allocates_nothing",
                storing_for_the_writer_allocates_nothing(),
            )
        }),
        Trial::test("a_store_without_a_writer_marks_nothing", || {
            outcome(
                "a_store_without_a_writer_marks_nothing",
                a_store_without_a_writer_marks_nothing(),
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

fn response() -> CachedResponse {
    CachedResponse {
        genid: 7,
        status: 200,
        headers: HeaderBlock::empty(),
        body: bytes::Bytes::from_static(b"cached body"),
    }
}

fn storing_for_the_writer_allocates_nothing() -> bool {
    let cache = Cache::new();
    let keys: Vec<CacheKey> = (0..1_000)
        .map(|i| CacheKey::new(&format!("/p{i}"), None, ""))
        .collect();
    // Store each key once, before counting. The measured loop then replaces
    // the same entries, so no hash table growth lands inside the count.
    for key in &keys {
        cache.insert(key.clone(), response());
    }
    let mut responses: Vec<CachedResponse> = (0..1_000).map(|_| response()).collect();
    assert_eq!(cache.len(), 1_000, "the check starts with a full set");

    let start = alloc_count::now();
    for key in keys {
        cache.insert_queued(key, responses.pop().expect("one response per key"));
    }
    let n = alloc_count::now() - start;
    println!(
        "storing_for_the_writer_allocates_nothing: {n} allocation(s) in 1000 stores marked for the writer"
    );
    assert_eq!(cache.queued(), 1_000, "every entry is marked");
    n != 0
}

/// A store on a node with no disk writer leaves the flag clear, so the flag
/// costs nothing on the ordinary path and the counter is not maintained for
/// nothing.
fn a_store_without_a_writer_marks_nothing() -> bool {
    let cache = Cache::new();
    let key = CacheKey::new("/plain", None, "");
    cache.insert(key, response());
    println!(
        "a_store_without_a_writer_marks_nothing: {} entries queued",
        cache.queued()
    );
    assert_eq!(cache.queued(), 0, "a plain store does not mark");
    false
}
