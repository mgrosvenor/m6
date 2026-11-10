//! `m6-perf-ab` — the performance gate's verdict, run from the command line.
//!
//! The decisions live in `m6_http_lib::perf_ab`. This binary only collects the
//! arguments and environment and hands them over, so the tests can drive the
//! same code without touching the process environment.

use std::collections::BTreeMap;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env: BTreeMap<String, String> = std::env::vars().collect();
    std::process::exit(m6_http_lib::perf_ab::run(&args, &env));
}
