//! The `/traffic` window costs no heap allocation per logged request.
//!
//! `harness = false` means no libtest thread shares this binary, so the counter
//! reads only what the pushes allocated. The window is allocated before the
//! count starts, as it is at start in the server: everything it uses is
//! allocated once and the ring never grows.

use m6_core::testkit::alloc_count;
use m6_http_lib::health::{Kind, TrafficWindow};

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

/// Far more events than the ring holds, so the wrap path is measured too.
const PUSHES: usize = 100_000;

fn main() {
    let mut window = TrafficWindow::new();

    // Built before the count: the pushes borrow them, and none of them is
    // copied into an owned value.
    let path = format!("/{}", "p".repeat(600));
    let user_agent = format!("Mozilla/5.0 (compatible) {}", "a".repeat(400));
    let client_ip = "2001:db8::1";

    let start = alloc_count::now();
    for i in 0..PUSHES {
        let kind = match i % 3 {
            0 => Kind::Request,
            1 => Kind::Monitor,
            _ => Kind::RateLimited,
        };
        window.push(
            i as u64 * 1_000,
            kind,
            200 + (i % 5) as u16,
            &path,
            client_ip,
            Some(&user_agent),
        );
    }
    let allocations = alloc_count::now() - start;

    println!("a_push_allocates_nothing: {allocations} allocation(s) in {PUSHES} pushes");
    std::process::exit(if allocations == 0 { 0 } else { 1 });
}
