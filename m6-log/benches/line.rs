//! ns per line for the two shapes m6 writes most.
//!
//! The operational lines go to standard output, which the caller redirects to
//! a file or a terminal. The analytics line goes to `/dev/null`.

use std::path::Path;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use m6_log::{Level, Line, Sink};

const SESSION: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

fn benches(c: &mut Criterion) {
    m6_log::init(Level::Info).expect("the logger");

    let mut sink = Sink::file(Path::new("/dev/null")).expect("the line sink");
    c.bench_function("analytics_line/m6-log", |b| {
        b.iter(|| {
            Line::begin(&mut sink, Level::Info, "request")
                .str("node", black_box("dev"))
                .str("path", black_box("/capabilities"))
                .u64("status", black_box(200))
                .str("cache_state", black_box("HIT"))
                .str("client_ip", black_box("192.0.2.1"))
                .str("session_id", black_box(SESSION))
                .bool("session_new", black_box(true))
                .str("user_agent", black_box("curl/8.7.1"))
                .u64("latency_ns", black_box(3360))
                .end();
        });
    });

    c.bench_function("info_three_fields/m6-log", |b| {
        b.iter(|| {
            m6_log::info!(
                node = black_box("dev"),
                status = black_box(200u64),
                cache_state = black_box("HIT"),
                "request"
            )
        });
    });
}

criterion_group!(line, benches);
criterion_main!(line);
