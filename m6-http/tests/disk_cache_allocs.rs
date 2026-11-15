//! Queuing a stored entry for the disk writer allocates nothing.
//!
//! `harness = false` means no libtest thread runs in this binary, so the
//! counting allocator reads only what these checks allocated. `main` forces
//! `libtest-mimic` onto one thread, and the library is exercised through the
//! same `DiskWriter::queue` the request path calls. Four thousand keys are made
//! and stored in a `Cache` before counting, so the flag test and set does real
//! map work rather than finding nothing. One thousand queued keys after that
//! cost zero allocations: the key is moved into a ring reserved at start, and
//! the duplicate check is the entry's `queued` flag, not a second set. A full
//! ring refuses the push and counts it, also with no allocation.
//!
//! Breaks: clone the key into the ring instead of moving it, and the count is
//! one thousand. Refuse duplicates with a `HashSet` instead of the entry's
//! flag, and the first push allocates a table. Grow the ring instead of
//! refusing a full one, and the full-ring check allocates.

use std::sync::Arc;

use libtest_mimic::{Arguments, Failed, Trial};
use m6_core::testkit::alloc_count;
use m6_http_lib::cache::{Cache, CacheKey, CachedResponse};
use m6_http_lib::disk_cache::{DiskWriter, QUEUE_SLOTS};

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

fn main() {
    let mut args = Arguments::from_args();
    // One thread: `libtest-mimic` then runs every trial in this (the main)
    // thread, so the counter reads only the trial's own allocations.
    args.test_threads = Some(1);
    let tests = vec![
        Trial::test("queueing_allocates_nothing", || {
            outcome("queueing_allocates_nothing", queueing_allocates_nothing())
        }),
        Trial::test("a_full_ring_counts_the_skip", || {
            outcome("a_full_ring_counts_the_skip", a_full_ring_counts_the_skip())
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
        headers: Arc::new(Vec::new()),
        body: bytes::Bytes::from_static(b"cached body"),
    }
}

/// A cache filled to the plan's 4,096 entries, and the keys this check queues.
fn filled_cache() -> (Cache, Vec<CacheKey>) {
    let cache = Cache::new();
    let mut queued = Vec::with_capacity(1_000);
    for i in 0..4_096 {
        let key = CacheKey::new(&format!("/p{i}"), None, "");
        if i < 1_000 {
            queued.push(key.clone());
        }
        cache.insert(key, response());
    }
    (cache, queued)
}

fn queueing_allocates_nothing() -> bool {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut cache, queued) = filled_cache();
    let mut writer = DiskWriter::new(dir.path().to_path_buf(), 128 * 1024 * 1024).expect("writer");
    writer.use_id(7);
    assert_eq!(cache.len(), 4_096, "the check starts with a full cache");

    let start = alloc_count::now();
    for key in queued {
        writer.queue(key, &mut cache);
    }
    let n = alloc_count::now() - start;
    println!("queueing_allocates_nothing: {n} allocation(s) in 1000 queued entries");
    assert_eq!(writer.queued(), 1_000, "every key was queued");
    n != 0
}

fn a_full_ring_counts_the_skip() -> bool {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cache = Cache::new();
    let mut writer = DiskWriter::new(dir.path().to_path_buf(), 128 * 1024 * 1024).expect("writer");
    writer.use_id(7);
    for i in 0..QUEUE_SLOTS {
        writer.queue(CacheKey::new(&format!("/full{i}"), None, ""), &mut cache);
    }
    assert_eq!(writer.queued(), QUEUE_SLOTS, "the ring is full");
    let extra = CacheKey::new("/one-more", None, "");

    let start = alloc_count::now();
    writer.queue(extra, &mut cache);
    let n = alloc_count::now() - start;
    println!("a_full_ring_counts_the_skip: {n} allocation(s) for one refused push");
    assert_eq!(
        writer.skipped_queue_full(),
        1,
        "the refused push is counted"
    );
    assert_eq!(writer.queued(), QUEUE_SLOTS, "the ring did not grow");
    n != 0
}
