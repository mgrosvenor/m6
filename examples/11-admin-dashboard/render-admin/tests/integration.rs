//! Integration tests for render-admin.
//!
//! Tests each module's logic directly — no live server required.
//! All tests run as part of `cargo test -p render-admin`.

use std::fs;
use std::io::Write;
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn tmp() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

// ── perf ──────────────────────────────────────────────────────────────────────

mod perf_tests {
    use super::*;
    use render_admin::perf::perf_blob;

    fn write_log(dir: &TempDir, lines: &[&str]) -> std::path::PathBuf {
        let p = dir.path().join("m6.log");
        let mut f = fs::File::create(&p).unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        p
    }

    #[test]
    fn empty_log_returns_empty_history() {
        let dir = tmp();
        let path = write_log(&dir, &[]);
        let v = perf_blob(&path, 60);
        let history = v["history"].as_array().expect("history array");
        assert!(history.is_empty());
    }

    #[test]
    fn missing_log_returns_empty_history() {
        let dir = tmp();
        let path = dir.path().join("nonexistent.log");
        let v = perf_blob(&path, 60);
        let history = v["history"].as_array().expect("history array");
        assert!(history.is_empty());
    }

    #[test]
    fn non_stats_lines_are_ignored() {
        let dir = tmp();
        let path = write_log(
            &dir,
            &[
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/","latency_ns":42000},"target":"m6_http"}"#,
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"INFO","fields":{"message":"m6-render started","routes":3},"target":"m6_http"}"#,
                r#"not json at all"#,
            ],
        );
        let v = perf_blob(&path, 60);
        assert!(v["history"].as_array().unwrap().is_empty());
    }

    #[test]
    fn periodic_stats_are_parsed() {
        let dir = tmp();
        let path = write_log(
            &dir,
            &[
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","latency_ns":10000},"target":"m6_http"}"#,
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"INFO","fields":{"message":"periodic stats","rps_avg":100,"rps_peak":200,"cache_hits":80,"cache_misses":20,"cache_hit_rate":0.8,"backend_errors":0,"pool_members":4,"hit_p50_ns":500000,"miss_p50_ns":2000000},"target":"m6_http"}"#,
                r#"{"timestamp":"2024-01-01T00:00:10Z","level":"INFO","fields":{"message":"periodic stats","rps_avg":150,"rps_peak":300,"cache_hits":90,"cache_misses":10,"cache_hit_rate":0.9,"backend_errors":0,"pool_members":4,"hit_p50_ns":400000,"miss_p50_ns":1500000},"target":"m6_http"}"#,
            ],
        );
        let v = perf_blob(&path, 60);
        let history = v["history"].as_array().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["rps_avg"], 100);
        assert_eq!(history[1]["rps_avg"], 150);
        assert_eq!(history[0]["hit_p50_ns"], 500000);
        assert_eq!(history[1]["cache_hit_rate"].as_f64().unwrap(), 0.9);
    }

    #[test]
    fn all_fields_present_in_periodic_stats() {
        let dir = tmp();
        let path = write_log(
            &dir,
            &[
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"INFO","fields":{"message":"periodic stats","rps_avg":100,"rps_peak":200,"cache_hits":80,"cache_misses":20,"cache_hit_rate":0.8,"backend_errors":1,"pool_members":4,"hit_p0_ns":100,"hit_p50_ns":500000,"hit_p99_ns":1000000,"hit_max_ns":2000000,"miss_p0_ns":200,"miss_p50_ns":2000000,"miss_p99_ns":4000000,"miss_max_ns":5000000},"target":"m6_http"}"#,
            ],
        );
        let v = perf_blob(&path, 60);
        let entry = &v["history"].as_array().unwrap()[0];
        for key in &[
            "ts",
            "rps_avg",
            "rps_peak",
            "cache_hits",
            "cache_misses",
            "cache_hit_rate",
            "backend_errors",
            "pool_members",
            "hit_p50_ns",
            "miss_p50_ns",
        ] {
            assert!(!entry[key].is_null(), "missing field: {key}");
        }
        assert_eq!(entry["backend_errors"], 1);
        assert_eq!(entry["pool_members"], 4);
    }

    #[test]
    fn n_limits_history_to_last_n_entries() {
        let dir = tmp();
        // 5 periodic stats entries
        let lines: Vec<String> = (1..=5)
            .map(|i| format!(
                r#"{{"timestamp":"2024-01-01T00:00:00Z","level":"INFO","fields":{{"message":"periodic stats","rps_avg":{i},"rps_peak":0,"cache_hits":0,"cache_misses":0,"cache_hit_rate":0,"backend_errors":0,"pool_members":0}},"target":"m6_http"}}"#,
            ))
            .collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let path = write_log(&dir, &refs);

        let v = perf_blob(&path, 3);
        let history = v["history"].as_array().unwrap();
        assert_eq!(history.len(), 3);
        // Should be the last 3: rps_avg 3, 4, 5
        assert_eq!(history[0]["rps_avg"], 3);
        assert_eq!(history[2]["rps_avg"], 5);
    }
}

// ── routes ────────────────────────────────────────────────────────────────────

mod routes_tests {
    use super::*;
    use render_admin::perf::routes_blob;

    fn write_log(dir: &TempDir, lines: &[&str]) -> std::path::PathBuf {
        let p = dir.path().join("m6.log");
        let mut f = fs::File::create(&p).unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        p
    }

    #[test]
    fn empty_log_returns_empty_routes() {
        let dir = tmp();
        let path = write_log(&dir, &[]);
        let v = routes_blob(&path, 1000);
        assert_eq!(v["sample_requests"], 0);
        assert!(v["routes"].as_array().unwrap().is_empty());
    }

    #[test]
    fn missing_log_returns_empty_routes() {
        let dir = tmp();
        let path = dir.path().join("no.log");
        let v = routes_blob(&path, 1000);
        assert_eq!(v["sample_requests"], 0);
    }

    #[test]
    fn aggregates_per_path() {
        let dir = tmp();
        let path = write_log(
            &dir,
            &[
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/","status":200,"cache_hit":true,"latency_ns":100000},"target":"m6_http"}"#,
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/","status":200,"cache_hit":true,"latency_ns":200000},"target":"m6_http"}"#,
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/blog","status":200,"cache_hit":false,"latency_ns":300000},"target":"m6_http"}"#,
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/blog","status":200,"cache_hit":true,"latency_ns":400000},"target":"m6_http"}"#,
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"INFO","fields":{"message":"periodic stats","rps_avg":10},"target":"m6_http"}"#,
            ],
        );
        let v = routes_blob(&path, 1000);
        assert_eq!(v["sample_requests"], 4);

        let routes = v["routes"].as_array().unwrap();
        // Sorted by requests desc; both paths have 2 requests each.
        assert_eq!(routes.len(), 2);

        let root = routes.iter().find(|r| r["path"] == "/").unwrap();
        assert_eq!(root["requests"], 2);
        assert_eq!(root["cache_hits"], 2);
        assert_eq!(root["cache_misses"], 0);
        assert_eq!(root["avg_latency_ns"], 150000);

        let blog = routes.iter().find(|r| r["path"] == "/blog").unwrap();
        assert_eq!(blog["cache_hits"], 1);
        assert_eq!(blog["cache_misses"], 1);
        assert_eq!(blog["avg_latency_ns"], 350000);
    }

    #[test]
    fn sorted_by_request_count_descending() {
        let dir = tmp();
        let lines: Vec<String> = (0..5).map(|_| r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/popular","cache_hit":true,"latency_ns":50000},"target":"m6_http"}"#.to_string())
            .chain((0..2).map(|_| r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/rare","cache_hit":false,"latency_ns":200000},"target":"m6_http"}"#.to_string()))
            .collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let path = write_log(&dir, &refs);
        let v = routes_blob(&path, 1000);
        let routes = v["routes"].as_array().unwrap();
        assert_eq!(routes[0]["path"], "/popular");
        assert_eq!(routes[1]["path"], "/rare");
    }

    #[test]
    fn sample_n_limits_entries_examined() {
        let dir = tmp();
        // Write 10 entries for / and then 3 for /new.
        let mut lines: Vec<String> = (0..10)
            .map(|_| r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/","cache_hit":true,"latency_ns":10000},"target":"m6_http"}"#.to_string())
            .collect();
        lines.extend((0..3)
            .map(|_| r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/new","cache_hit":false,"latency_ns":20000},"target":"m6_http"}"#.to_string()));
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let path = write_log(&dir, &refs);

        // Limit sample to last 3 entries — should only see /new.
        let v = routes_blob(&path, 3);
        assert_eq!(v["sample_requests"], 3);
        let routes = v["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0]["path"], "/new");
    }

    #[test]
    fn hit_rate_calculated_correctly() {
        let dir = tmp();
        let path = write_log(
            &dir,
            &[
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/x","cache_hit":true,"latency_ns":1000},"target":"m6_http"}"#,
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/x","cache_hit":true,"latency_ns":1000},"target":"m6_http"}"#,
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/x","cache_hit":false,"latency_ns":1000},"target":"m6_http"}"#,
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/x","cache_hit":false,"latency_ns":1000},"target":"m6_http"}"#,
            ],
        );
        let v = routes_blob(&path, 1000);
        let routes = v["routes"].as_array().unwrap();
        let hit_rate = routes[0]["hit_rate"].as_f64().unwrap();
        assert!(
            (hit_rate - 0.5).abs() < 0.001,
            "expected 0.5, got {hit_rate}"
        );
    }

    #[test]
    fn sample_window_secs_computed_from_timestamps() {
        let dir = tmp();
        let path = write_log(
            &dir,
            &[
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/","cache_hit":false,"latency_ns":1000},"target":"m6_http"}"#,
                r#"{"timestamp":"2024-01-01T00:01:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/","cache_hit":false,"latency_ns":1000},"target":"m6_http"}"#,
            ],
        );
        let v = routes_blob(&path, 1000);
        let window = v["sample_window_secs"]
            .as_u64()
            .expect("sample_window_secs should be a number");
        assert_eq!(window, 60, "expected 60s window, got {window}");
    }

    #[test]
    fn sample_window_secs_null_without_timestamps() {
        let dir = tmp();
        // Entries without timestamp fields — window must be null.
        let path = write_log(
            &dir,
            &[
                r#"{"level":"DEBUG","fields":{"message":"request complete","path":"/","cache_hit":false,"latency_ns":1000},"target":"m6_http"}"#,
                r#"{"level":"DEBUG","fields":{"message":"request complete","path":"/","cache_hit":false,"latency_ns":1000},"target":"m6_http"}"#,
            ],
        );
        let v = routes_blob(&path, 1000);
        assert!(
            v["sample_window_secs"].is_null(),
            "expected null when no timestamps present"
        );
    }

    #[test]
    fn zero_latency_entries_do_not_panic() {
        // All zero latency_ns values — avg should be 0.
        let dir = tmp();
        let path = write_log(
            &dir,
            &[
                r#"{"timestamp":"2024-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"request complete","path":"/","cache_hit":false,"latency_ns":0},"target":"m6_http"}"#,
            ],
        );
        let v = routes_blob(&path, 1000);
        let routes = v["routes"].as_array().unwrap();
        assert_eq!(routes[0]["avg_latency_ns"], 0);
    }
}

// ── system ────────────────────────────────────────────────────────────────────

mod system_tests {
    use render_admin::system::system_blob;

    #[test]
    fn system_blob_has_required_keys() {
        let v = system_blob();
        assert!(v["uptime_secs"].is_number(), "missing uptime_secs");
        assert!(v["cpu_usage_pct"].is_number(), "missing cpu_usage_pct");
        assert!(v["memory"].is_object(), "missing memory");
        assert!(v["disks"].is_array(), "missing disks");
    }

    #[test]
    fn memory_has_required_fields() {
        let v = system_blob();
        let mem = &v["memory"];
        assert!(mem["used_bytes"].is_number(), "missing memory.used_bytes");
        assert!(mem["total_bytes"].is_number(), "missing memory.total_bytes");
        assert!(mem["used_pct"].is_number(), "missing memory.used_pct");
    }

    #[test]
    fn uptime_is_positive() {
        let v = system_blob();
        let uptime = v["uptime_secs"].as_u64().expect("uptime_secs u64");
        assert!(uptime > 0, "uptime should be positive, got {uptime}");
    }

    #[test]
    fn memory_totals_are_sensible() {
        let v = system_blob();
        let total = v["memory"]["total_bytes"].as_u64().unwrap_or(0);
        assert!(total > 0, "total memory should be > 0");
        let used = v["memory"]["used_bytes"].as_u64().unwrap_or(0);
        assert!(
            used <= total,
            "used_bytes ({used}) should not exceed total_bytes ({total})"
        );
    }

    #[test]
    fn disk_entries_have_required_fields() {
        let v = system_blob();
        for disk in v["disks"].as_array().unwrap() {
            assert!(disk["mount"].is_string(), "missing disk.mount");
            assert!(disk["used_bytes"].is_number(), "missing disk.used_bytes");
            assert!(disk["total_bytes"].is_number(), "missing disk.total_bytes");
            assert!(disk["used_pct"].is_number(), "missing disk.used_pct");
        }
    }
}

// ── backends view ─────────────────────────────────────────────────────────────

mod backends_tests {
    use super::*;
    use render_admin::perf::backends_summary_blob;

    // The bind is only in the system config, as since m6 #243; site.toml has no [server].
    #[test]
    fn backends_reports_the_bind_from_the_system_config() {
        let dir = tmp();
        let site = dir.path().join("site.toml");
        fs::write(
            &site,
            "[site]\nname = \"s\"\n\n[[backend]]\nname = \"api\"\n",
        )
        .unwrap();
        let system = dir.path().join("system.toml");
        fs::write(&system, "[server]\nbind = \"192.0.2.10:8444\"\n").unwrap();
        let log = dir.path().join("empty.log");
        fs::write(&log, "").unwrap();

        let v = backends_summary_blob(&log, &site, &system, 100);
        assert_eq!(v["bind"], "192.0.2.10:8444");
        assert_eq!(v["backends"][0]["name"], "api");
    }

    #[test]
    fn backends_bind_is_null_without_a_system_config() {
        let dir = tmp();
        let site = dir.path().join("site.toml");
        fs::write(&site, "[site]\nname = \"s\"\n").unwrap();
        let log = dir.path().join("empty.log");
        fs::write(&log, "").unwrap();

        let v = backends_summary_blob(&log, &site, &dir.path().join("none.toml"), 100);
        assert!(v["bind"].is_null());
    }
}

// ── bench ─────────────────────────────────────────────────────────────────────

mod bench_tests {
    use super::*;
    use render_admin::bench::{discover_targets, new_job_store, poll_job, start_bench};

    fn write_system_toml(dir: &TempDir, bind: &str) -> std::path::PathBuf {
        let p = dir.path().join("system.toml");
        fs::write(&p, format!("[server]\nbind = \"{bind}\"\n")).unwrap();
        p
    }

    #[test]
    fn targets_discovered_from_system_config_bind() {
        let dir = tmp();
        let path = write_system_toml(&dir, "192.0.2.10:8444");
        let targets = discover_targets(&path);
        assert_eq!(targets.len(), 4); // h1, h2, h3, all
        for t in &targets {
            assert_eq!(t["addr"].as_str().unwrap(), "127.0.0.1:8444");
        }
        let protos: Vec<&str> = targets
            .iter()
            .map(|t| t["proto"].as_str().unwrap())
            .collect();
        assert!(protos.contains(&"h1"));
        assert!(protos.contains(&"h2"));
        assert!(protos.contains(&"h3"));
        assert!(protos.contains(&"all"));
    }

    #[test]
    fn targets_missing_system_config_returns_empty() {
        let dir = tmp();
        let path = dir.path().join("no-system.toml");
        assert!(discover_targets(&path).is_empty());
    }

    // A system config with no [server].bind must not invent a port to bench.
    #[test]
    fn targets_empty_when_bind_missing() {
        let dir = tmp();
        let p = dir.path().join("system.toml");
        fs::write(&p, "[auth]\nbackend = \"auth\"\n").unwrap();
        assert!(discover_targets(&p).is_empty());
    }

    #[test]
    fn poll_nonexistent_job_returns_none() {
        let store = new_job_store();
        assert!(poll_job(&store, "no-such-id").is_none());
    }

    // A running bench is reaped from its child's exit descriptor (`ChildExit`:
    // a pidfd on Linux, a kqueue on macOS). The tests below drive a real child
    // the way the service loop does: the output pipe and the exit descriptor on
    // their own tokens, plus one socket as another request the loop must keep
    // serving.

    #[test]
    fn a_bench_child_that_closes_stdout_does_not_stop_the_loop() {
        child::a_silent_child_does_not_stop_the_loop();
    }

    #[test]
    fn a_bench_child_that_exits_nonzero_reports_that_status() {
        child::a_failing_child_reports_its_status();
    }

    #[test]
    fn start_bench_with_echo_produces_done_status() {
        // Use `echo` as a stand-in for m6-bench (exits 0, produces output).
        child::echo_child_is_done();
    }

    #[test]
    fn start_bench_with_nonexistent_binary_produces_failed_status() {
        let mut store = new_job_store();
        let (job_id, run) = start_bench(
            std::path::Path::new("/no-such-binary"),
            &mut store,
            "127.0.0.1:9999",
            "h1",
            1,
            1,
        );
        assert!(run.is_none(), "a binary that cannot start has no pipe");

        let blob = poll_job(&store, &job_id).expect("job exists");
        assert_eq!(blob["status"].as_str(), Some("failed"));
        assert!(blob["output"]
            .as_str()
            .unwrap_or("")
            .contains("failed to start m6-bench"));
    }

    mod child {
        use super::*;
        use m6_core::platform::{now, Events, Interest, Poll, Token};
        use render_admin::bench::{bench_event, start_bench, BenchEvent, JobStore, Running};
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        const PIPE: Token = Token(0);
        const EXIT: Token = Token(1);
        const REQUEST: Token = Token(2);

        // The child ignores the bench arguments and runs this shell body.
        fn bench_script(dir: &TempDir, body: &str) -> std::path::PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.path().join("bench.sh");
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write the bench script");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("make the bench script executable");
            path
        }

        // Drive a running bench the way the service loop does, with a socket
        // registered beside it as the other request. The per-event work and
        // the job's finish are `bench_event`, the function the service calls,
        // so deleting its done check leaves the bench unfinished and this
        // driver fails at its deadline. Returns whether the child had not yet
        // been reported exited when that request was answered.
        fn drive(run: Running, store: &mut JobStore) -> bool {
            let (mut server, mut client) = UnixStream::pair().expect("a socket pair");
            server
                .set_nonblocking(true)
                .expect("the server end is non-blocking");
            client.write_all(b"?").expect("send the request");
            let mut poll = Poll::new().expect("a poll");
            let mut bench = Some(run);
            let run = bench.as_ref().expect("the bench starts");
            poll.registry()
                .register_fd(run.pipe_fd(), PIPE, Interest::READABLE)
                .expect("register the bench pipe");
            poll.registry()
                .register_fd(run.exit_fd(), EXIT, Interest::READABLE)
                .expect("register the exit descriptor");
            poll.registry()
                .register_fd(server.as_raw_fd(), REQUEST, Interest::READABLE)
                .expect("register the request");
            let mut events = Events::with_capacity(8);
            let mut buf = [0u8; 1024];
            let deadline = now() + Duration::from_secs(10);
            let mut served = false;
            let mut exited = false;
            let mut served_while_running = false;
            loop {
                let left = deadline
                    .checked_duration_since(now())
                    .expect("the bench and its request finish within 10 s");
                poll.poll(&mut events, Some(left)).expect("poll");
                for ev in &events {
                    match ev.token() {
                        PIPE => {
                            bench_event(&mut bench, store, &poll, BenchEvent::Pipe(&mut buf))
                                .expect("read the bench output");
                        }
                        EXIT => {
                            exited = true;
                            bench_event(&mut bench, store, &poll, BenchEvent::Exit)
                                .expect("reap the bench child");
                        }
                        REQUEST => {
                            let n = server.read(&mut buf).expect("read the request");
                            assert!(n > 0, "the request carries one byte");
                            server.write_all(b"ok").expect("answer the request");
                            served_while_running = !exited;
                            served = true;
                        }
                        token => panic!("unexpected token {token:?}"),
                    }
                }
                if served && bench.is_none() {
                    poll.registry()
                        .deregister_fd(server.as_raw_fd())
                        .expect("deregister the request");
                    break;
                }
            }
            served_while_running
        }

        fn start(script: &std::path::Path) -> (String, Running, JobStore) {
            let mut store = new_job_store();
            let (job_id, run) = start_bench(script, &mut store, "127.0.0.1:9999", "h1", 1, 1);
            (job_id, run.expect("the bench script starts"), store)
        }

        pub(super) fn a_silent_child_does_not_stop_the_loop() {
            let dir = tmp();
            // The child closes its stdout at once and then sleeps: end of file
            // arrives while the child is alive, so the loop must keep serving.
            let script = bench_script(&dir, "exec 1>&-\nsleep 2\nexit 0");
            let (job_id, run, mut store) = start(&script);
            let served_while_running = drive(run, &mut store);
            assert!(
                served_while_running,
                "the loop answered another request while the bench child was still running"
            );

            let blob = poll_job(&store, &job_id).expect("job exists");
            assert_eq!(blob["status"].as_str(), Some("done"), "job: {blob}");
            assert_eq!(blob["output"].as_str(), Some(""));
        }

        pub(super) fn a_failing_child_reports_its_status() {
            let dir = tmp();
            let script = bench_script(&dir, "echo boom\nexit 7");
            let (job_id, run, mut store) = start(&script);
            drive(run, &mut store);

            let blob = poll_job(&store, &job_id).expect("job exists");
            assert_eq!(blob["status"].as_str(), Some("failed"), "job: {blob}");
            assert_eq!(blob["output"].as_str(), Some("boom\n"));
        }

        pub(super) fn echo_child_is_done() {
            let mut store = new_job_store();
            let (job_id, run) = start_bench(
                std::path::Path::new("echo"),
                &mut store,
                "127.0.0.1:9999",
                "h1",
                1,
                1,
            );
            let run = run.expect("echo starts");
            drive(run, &mut store);

            let blob = poll_job(&store, &job_id).expect("job exists");
            assert_eq!(blob["status"].as_str(), Some("done"));
        }
    }
}

// ── logs ──────────────────────────────────────────────────────────────────────

mod logs_tests {
    use super::*;
    use render_admin::logs::logs_blob;

    fn write_log(dir: &TempDir, n_lines: usize) -> std::path::PathBuf {
        let p = dir.path().join("test.log");
        let mut f = fs::File::create(&p).unwrap();
        for i in 0..n_lines {
            writeln!(f, "log line {i}").unwrap();
        }
        p
    }

    #[test]
    fn missing_log_returns_error_key() {
        let dir = tmp();
        let v = logs_blob(&dir.path().join("no.log"), 100, None);
        assert!(v["error"].is_string());
    }

    #[test]
    fn empty_log_returns_zero_lines() {
        let dir = tmp();
        let path = write_log(&dir, 0);
        let v = logs_blob(&path, 100, None);
        assert_eq!(v["total_lines"], 0);
        assert!(v["lines"].as_array().unwrap().is_empty());
    }

    #[test]
    fn returns_all_lines_when_fewer_than_n() {
        let dir = tmp();
        let path = write_log(&dir, 5);
        let v = logs_blob(&path, 100, None);
        assert_eq!(v["total_lines"], 5);
        assert_eq!(v["lines"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn returns_last_n_lines() {
        let dir = tmp();
        let path = write_log(&dir, 10);
        let v = logs_blob(&path, 3, None);
        assert_eq!(v["total_lines"], 10);
        let lines = v["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].as_str().unwrap(), "log line 7");
        assert_eq!(lines[2].as_str().unwrap(), "log line 9");
    }

    #[test]
    fn n_larger_than_file_returns_all() {
        let dir = tmp();
        let path = write_log(&dir, 3);
        let v = logs_blob(&path, 1000, None);
        assert_eq!(v["lines"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn line_content_is_correct() {
        let dir = tmp();
        // Write specific named lines.
        let p = dir.path().join("specific.log");
        fs::write(&p, "alpha\nbeta\ngamma\n").unwrap();
        let v = logs_blob(&p, 100, None);
        let lines = v["lines"].as_array().unwrap();
        assert_eq!(lines[0].as_str().unwrap(), "alpha");
        assert_eq!(lines[1].as_str().unwrap(), "beta");
        assert_eq!(lines[2].as_str().unwrap(), "gamma");
    }
}

// ── ops ───────────────────────────────────────────────────────────────────────

mod ops_tests {
    use super::*;
    use render_admin::ops::{config_read, config_write, restart_service, ServiceConfig};
    use serde_json::json;

    fn write_toml(dir: &TempDir, content: &str) -> std::path::PathBuf {
        let p = dir.path().join("site.toml");
        fs::write(&p, content).unwrap();
        p
    }

    // ── config_read ──────────────────────────────────────────────────────────

    #[test]
    fn config_read_returns_parsed_json() {
        let dir = tmp();
        let path = write_toml(&dir, "[server]\nbind = \"127.0.0.1:8443\"\n");
        let v = config_read(&path);
        assert!(v["error"].is_null(), "unexpected error: {}", v["error"]);
        assert!(v["path"].as_str().unwrap().ends_with("site.toml"));
        // Config is returned as a parsed JSON object, not raw text.
        assert!(v["config"].is_object(), "config should be a JSON object");
        assert_eq!(
            v["config"]["server"]["bind"].as_str().unwrap(),
            "127.0.0.1:8443"
        );
    }

    #[test]
    fn config_read_arrays_preserved() {
        let dir = tmp();
        let toml = "[[route]]\npath = \"/\"\nbackend = \"html\"\n\n[[route]]\npath = \"/api\"\nbackend = \"render\"\n";
        let path = write_toml(&dir, toml);
        let v = config_read(&path);
        let routes = v["config"]["route"].as_array().expect("route array");
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0]["path"].as_str().unwrap(), "/");
        assert_eq!(routes[1]["path"].as_str().unwrap(), "/api");
    }

    #[test]
    fn config_read_missing_file_returns_error() {
        let dir = tmp();
        let v = config_read(&dir.path().join("no.toml"));
        assert!(v["error"].is_string());
    }

    // ── config_write ─────────────────────────────────────────────────────────

    #[test]
    fn config_write_json_blob_round_trips() {
        let dir = tmp();
        let original = "[server]\nbind = \"127.0.0.1:8443\"\n";
        let path = write_toml(&dir, original);

        // Read back as JSON blob.
        let read_v = config_read(&path);
        let config_json = &read_v["config"];
        assert!(config_json.is_object());

        // Modify a field.
        let mut updated = config_json.clone();
        updated["server"]["bind"] = json!("0.0.0.0:9000");

        // Write back.
        config_write(&path, &updated).expect("write should succeed");

        // Read again and verify.
        let read_v2 = config_read(&path);
        assert_eq!(
            read_v2["config"]["server"]["bind"].as_str().unwrap(),
            "0.0.0.0:9000"
        );
    }

    #[test]
    fn config_write_null_value_is_rejected() {
        let dir = tmp();
        let path = write_toml(&dir, "[server]\nbind = \"ok\"\n");
        let bad = json!({ "server": { "bind": null } });
        let result = config_write(&path, &bad);
        assert!(result.is_err(), "should reject null (not valid TOML)");
        // Original file must be unchanged.
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("bind"));
    }

    #[test]
    fn config_write_empty_object_succeeds() {
        let dir = tmp();
        let path = write_toml(&dir, "");
        config_write(&path, &json!({})).expect("empty object should be valid TOML");
        let read_back = config_read(&path);
        assert!(read_back["config"].is_object());
    }

    #[test]
    fn config_write_preserves_arrays() {
        let dir = tmp();
        let path = write_toml(&dir, "");
        let cfg = json!({
            "server": { "bind": "127.0.0.1:8443" },
            "route": [
                { "path": "/",    "backend": "html" },
                { "path": "/api", "backend": "render" }
            ]
        });
        config_write(&path, &cfg).expect("write should succeed");

        let read_back = config_read(&path);
        let routes = read_back["config"]["route"]
            .as_array()
            .expect("route array");
        assert_eq!(routes.len(), 2);
    }

    // ── restart_service ──────────────────────────────────────────────────────

    #[test]
    fn restart_missing_pid_file_returns_error() {
        let svc = ServiceConfig {
            name: "test".into(),
            pid_file: "/tmp/no-such-render-admin-test.pid".into(),
        };
        let v = restart_service(&svc);
        assert!(v["error"].is_string(), "expected error key, got: {v}");
    }

    #[test]
    fn restart_invalid_pid_content_returns_error() {
        let dir = tmp();
        let pid_file = dir.path().join("bad.pid");
        fs::write(&pid_file, "not-a-number\n").unwrap();
        let svc = ServiceConfig {
            name: "test".into(),
            pid_file: pid_file.to_string_lossy().into(),
        };
        let v = restart_service(&svc);
        assert!(v["error"].is_string());
    }

    #[test]
    fn restart_returns_ok_true_on_success() {
        // Spawn a sleep process we can SIGTERM safely.
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        let dir = tmp();
        let pid_file = dir.path().join("test.pid");
        fs::write(&pid_file, format!("{pid}\n")).unwrap();
        let svc = ServiceConfig {
            name: "sleep".into(),
            pid_file: pid_file.to_string_lossy().into(),
        };
        let v = restart_service(&svc);
        // Reap the child to avoid zombie.
        child.wait().ok();

        assert_eq!(v["ok"], true, "expected ok:true, got: {v}");
        assert_eq!(v["pid"], pid);
    }
}
