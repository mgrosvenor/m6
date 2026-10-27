//! The poll history and its two endpoints, through the real binary.
//!
//! Every test polls one node that cannot be reached, so the history holds a row
//! with `reachable == false`. That is the case worth pinning: a monitor whose
//! history lost the rows for a dead node would lose them exactly when someone
//! wants them.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use m6_core::testkit::{assert_app_lifecycle, binary, response::read_one, wait, Service};
use m6_core::time_range::rfc3339_utc_ms;

fn config(history: &str) -> String {
    format!(
        r#"
poll_interval_s = 1

[monitor]
timeout_ms = 200
{history}

[[monitor.nodes]]
name = "down"
url  = "https://127.0.0.1:1"
role = "origin"

[thread_pool]
size       = 1
queue_size = 4

[log]
level  = "info"
format = "text"
"#
    )
}

struct Setup {
    dir: tempfile::TempDir,
    config: PathBuf,
    sock: PathBuf,
}

fn setup(history: &str) -> Setup {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("monitor.conf");
    std::fs::write(&config, self::config(history)).expect("write config");
    // Short name: a unix socket path is capped near 104 bytes on macOS.
    let sock = dir.path().join("mon.sock");
    Setup { dir, config, sock }
}

impl Setup {
    fn command(&self) -> Command {
        let mut c = Command::new(binary("m6-monitor"));
        c.arg(self.dir.path())
            .arg(&self.config)
            .env("M6_SOCKET_OVERRIDE", &self.sock);
        c
    }

    fn start(&self) -> Service {
        let mut svc = Service::spawn("m6-monitor", &mut self.command());
        svc.wait_for_path(&self.sock, Duration::from_secs(10));
        svc
    }
}

struct Answer {
    status: u16,
    head: String,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    fn lines(&self) -> Vec<serde_json::Value> {
        self.body
            .lines()
            .map(|l| serde_json::from_str(l).expect("a JSON line"))
            .collect()
    }
}

fn get(sock: &Path, target: &str) -> Answer {
    let mut s = UnixStream::connect(sock).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET {target} HTTP/1.1\r\nHost: m6\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let raw = read_one(&mut s, "GET").expect("a response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Answer {
        status,
        head: head.to_string(),
        body: body.to_string(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A query for the hour either side of now.
fn range() -> String {
    format!(
        "from={}&to={}",
        rfc3339_utc_ms(now_ms() - 3_600_000),
        rfc3339_utc_ms(now_ms() + 3_600_000)
    )
}

/// Poll `/history` until it has a line, or fail with what it last said.
fn history_with_rows(sock: &Path) -> Answer {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let a = get(sock, &format!("/history?{}", range()));
        if a.status == 200 && !a.body.is_empty() {
            return a;
        }
        assert!(
            Instant::now() < deadline,
            "no history row appeared: {} {}",
            a.status,
            a.body
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn history_config(s: &Setup) -> String {
    format!(
        "history_file = {:?}",
        s.dir.path().join("data/history.sqlite3")
    )
}

#[test]
fn a_fresh_install_creates_the_file_and_serves_it_on_the_socket() {
    let dir_for_path = tempfile::tempdir().unwrap();
    let s = setup(&format!(
        "history_file = {:?}",
        dir_for_path.path().join("data/history.sqlite3")
    ));
    let _svc = s.start();
    let a = history_with_rows(&s.sock);
    assert_eq!(a.header("content-type"), Some("application/x-ndjson"));
    assert_eq!(a.header("cache-control"), Some("no-store"));
    let rows = a.lines();
    assert_eq!(rows[0]["node"], "down");
    assert_eq!(rows[0]["reachable"], false);
    assert_eq!(rows[0]["level"], "Fault");
    assert!(rows[0]["tunnel"].is_null());
    assert!(dir_for_path.path().join("data/history.sqlite3").exists());
}

#[test]
fn crawlers_answers_on_the_socket() {
    let s = setup("");
    let hist = tempfile::tempdir().unwrap();
    std::fs::write(
        &s.config,
        config(&format!(
            "history_file = {:?}",
            hist.path().join("h.sqlite3")
        )),
    )
    .unwrap();
    let _svc = s.start();
    history_with_rows(&s.sock);
    let a = get(&s.sock, &format!("/crawlers?{}", range()));
    assert_eq!(a.status, 200);
    assert_eq!(a.header("content-type"), Some("application/x-ndjson"));
    assert!(
        a.body.is_empty(),
        "an unreachable node has no crawler window: {}",
        a.body
    );
}

#[test]
fn a_bad_range_or_cursor_is_a_400() {
    let hist = tempfile::tempdir().unwrap();
    let s = setup(&format!(
        "history_file = {:?}",
        hist.path().join("h.sqlite3")
    ));
    let _svc = s.start();
    let r = range();
    for target in [
        "/history".to_string(),
        "/history?from=2026-01-01T00:00:00Z".to_string(),
        "/history?from=2026-01-02T00:00:00Z&to=2026-01-01T00:00:00Z".to_string(),
        "/history?from=2026-01-01T00:00:00Z&to=2026-03-01T00:00:00Z".to_string(),
        format!("/history?{r}&cursor=not*a*cursor"),
        format!("/history?{r}&cursor=notacursor"),
        format!("/crawlers?{r}&cursor=1-2&cursor=3-4"),
    ] {
        let a = get(&s.sock, &target);
        assert_eq!(a.status, 400, "{target}: {}", a.body);
        assert_eq!(a.header("cache-control"), Some("no-store"), "{target}");
    }
}

#[test]
fn history_off_answers_503() {
    let s = setup("");
    let _svc = s.start();
    for path in ["/history", "/crawlers"] {
        let a = get(&s.sock, &format!("{path}?{}", range()));
        assert_eq!(a.status, 503, "{path}");
        assert!(
            a.body
                .contains("history is off: set [monitor] history_file to keep one"),
            "{}",
            a.body
        );
    }
}

#[test]
fn an_unwritable_history_path_does_not_stop_monitoring() {
    let s = setup("");
    let blocker = s.dir.path().join("blocker");
    std::fs::write(&blocker, b"a file, not a directory").unwrap();
    std::fs::write(
        &s.config,
        config(&format!(
            "history_file = {:?}",
            blocker.join("history.sqlite3")
        )),
    )
    .unwrap();
    let mut svc = s.start();
    let a = get(&s.sock, &format!("/history?{}", range()));
    assert_eq!(a.status, 503, "{}", a.body);
    assert!(a.body.contains("history is unavailable"), "{}", a.body);
    // Polling carries on, and the digest says why there is no history.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let d = get(&s.sock, "/digest");
        if d.status == 200 {
            let v: serde_json::Value = serde_json::from_str(&d.body).unwrap();
            assert_eq!(v["history"]["state"], "not_open");
            let warned = v["findings"].as_array().unwrap().iter().any(|f| {
                f["node"] == "monitor"
                    && f["level"] == "Warn"
                    && f["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("could not be opened")
            });
            assert!(warned, "{}", d.body);
            break;
        }
        assert!(Instant::now() < deadline, "the digest never appeared");
        std::thread::sleep(Duration::from_millis(200));
    }
    svc.assert_alive("monitoring should go on without history");
}

#[test]
fn a_history_file_newer_than_the_binary_refuses_to_start() {
    let s = setup("");
    let file = s.dir.path().join("future.sqlite3");
    {
        let c = m6_core::sqlite::rusqlite::Connection::open(&file).unwrap();
        c.pragma_update(None, "user_version", 99).unwrap();
    }
    std::fs::write(&s.config, config(&format!("history_file = {file:?}"))).unwrap();
    let out = s
        .command()
        .stdin(Stdio::null())
        .output()
        .expect("run m6-monitor");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("future.sqlite3"), "{err}");
}

#[test]
fn a_bad_history_key_refuses_to_start() {
    let s = setup("history_file = \"h.sqlite3\"\nhistory_max_mb = 0");
    let out = s.command().stdin(Stdio::null()).output().expect("run");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("history_max_mb"));
}

#[test]
fn lifecycle_is_clean_with_history_on() {
    let s = setup("");
    std::fs::write(&s.config, config(&history_config(&s))).unwrap();
    assert_app_lifecycle("m6-monitor", &mut s.command(), &s.sock);
}

#[test]
fn an_unknown_node_is_a_400_and_a_known_one_filters() {
    let hist = tempfile::tempdir().unwrap();
    let s = setup(&format!(
        "history_file = {:?}",
        hist.path().join("h.sqlite3")
    ));
    let _svc = s.start();
    history_with_rows(&s.sock);
    let a = get(&s.sock, &format!("/history?{}&node=nobody", range()));
    assert_eq!(a.status, 400);
    assert!(
        !a.body.contains("nobody"),
        "the client's value must not be echoed"
    );
    let a = get(&s.sock, &format!("/history?{}&node=down", range()));
    assert_eq!(a.status, 200);
    assert!(a.lines().iter().all(|r| r["node"] == "down"));
    let a = get(&s.sock, &format!("/history?{}&node=", range()));
    assert_eq!(a.status, 400);
}

/// The process listens on its unix socket and on nothing else.
#[test]
fn no_tcp_listener_is_opened() {
    let hist = tempfile::tempdir().unwrap();
    let s = setup(&format!(
        "history_file = {:?}",
        hist.path().join("h.sqlite3")
    ));
    let mut child = s
        .command()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    assert!(wait::for_path(&s.sock, Duration::from_secs(10)));
    history_with_rows(&s.sock);
    let listening = tcp_listeners(child.id());
    let _ = child.kill();
    let _ = child.wait();
    assert!(listening.is_empty(), "TCP listeners: {listening:?}");
}

#[cfg(target_os = "linux")]
fn tcp_listeners(pid: u32) -> Vec<String> {
    let inodes: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("fd dir")
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter_map(|l| {
            let l = l.to_string_lossy().into_owned();
            Some(l.strip_prefix("socket:[")?.strip_suffix(']')?.to_string())
        })
        .collect();
    let mut found = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for line in std::fs::read_to_string(table)
            .unwrap_or_default()
            .lines()
            .skip(1)
        {
            let f: Vec<&str> = line.split_whitespace().collect();
            // Field 3 is the state (0A is LISTEN) and field 9 the socket inode.
            if f.len() > 9 && f[3] == "0A" && inodes.iter().any(|i| i == f[9]) {
                found.push(line.to_string());
            }
        }
    }
    found
}

#[cfg(not(target_os = "linux"))]
fn tcp_listeners(pid: u32) -> Vec<String> {
    let out = Command::new("lsof")
        .args(["-nP", "-a", "-p", &pid.to_string(), "-iTCP", "-sTCP:LISTEN"])
        .output()
        .expect("lsof");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect()
}
