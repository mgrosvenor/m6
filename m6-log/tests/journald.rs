//! The native journal write: fields, one datagram per line, the refusal for a
//! line the socket cannot carry, and the fall back to stdout.
//!
//! One test function, because the Sink and the limits are per process: each
//! phase needs the process logger in a known state, so the phases run in
//! order on one thread.
//!
//! The last phase needs a real journald and `journalctl`, which is a Linux
//! host with systemd. It prints `NOT MEASURED` and the reason on a host
//! without them, so a macOS run says what it could not check.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: the wait polls its own datagram socket under one deadline"
)]

use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use m6_log::{Level, Line, LogLimits, Type};

/// The socket the test's own journal receiver is bound at.
const TEST_SOCKET: &str = "M6_TEST_JOURNAL_SOCKET";

#[test]
fn the_native_journal_write() {
    fields_and_one_datagram_per_line();
    an_oversized_datagram_is_refused_and_counted();
    a_failed_send_falls_back_to_stdout();
    a_real_journald_carries_the_fields();
}

/// Bind a receiver at `dir`, name it for the logger, and answer both.
fn receiver(dir: &Path) -> (UnixDatagram, PathBuf) {
    let path = dir.join("journal.sock");
    let socket = UnixDatagram::bind(&path).expect("bind the test journal");
    socket
        .set_nonblocking(true)
        .expect("a non-blocking receiver");
    std::env::set_var(TEST_SOCKET, &path);
    (socket, path)
}

/// One datagram, or `None` when none arrives within the bound.
fn recv(socket: &UnixDatagram) -> Option<String> {
    let deadline = Instant::now() + m6_core_bound();
    let mut buf = vec![0u8; m6_log::DATAGRAM_MAX];
    loop {
        match socket.recv(&mut buf) {
            Ok(n) => return Some(String::from_utf8_lossy(&buf[..n]).into_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("the test journal socket failed: {error}"),
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// How long a wait on the test's own socket may take.
fn m6_core_bound() -> Duration {
    Duration::from_secs(2)
}

/// The `KEY=value` field named `key` of one datagram.
fn field<'a>(datagram: &'a str, key: &str) -> Option<&'a str> {
    let start = format!("{key}=");
    let at = datagram
        .split('\n')
        .position(|part| part.starts_with(&start))?;
    datagram
        .split('\n')
        .nth(at)
        .map(|part| &part[start.len()..])
}

fn fields_and_one_datagram_per_line() {
    let dir = tempfile::tempdir().expect("a directory");
    let (socket, _path) = receiver(dir.path());
    m6_log::init(Level::Info).expect("the logger");
    assert!(m6_log::writes_journal(), "the test socket is the journal");

    m6_log::with_operational(|sink| {
        Line::begin(sink, Level::Warn, "warning").end();
    });
    let document = serde_json::json!({"requests_total": 7});
    m6_log::with_operational(|sink| {
        Line::begin(sink, Level::Info, "stats")
            .kind(Type::Perf)
            .json("perf", &document)
            .end();
    });
    m6_log::flush();

    let first = recv(&socket).expect("the warning's datagram");
    assert_eq!(field(&first, "PRIORITY"), Some("4"), "{first}");
    assert_eq!(field(&first, "M6_TYPE"), Some("event"), "{first}");
    let identifier = field(&first, "SYSLOG_IDENTIFIER").expect("SYSLOG_IDENTIFIER");
    assert!(
        identifier.starts_with("journald"),
        "the program's own name, with the hash cargo gives a test binary: {identifier}"
    );
    let message = field(&first, "MESSAGE").expect("MESSAGE");
    let value: serde_json::Value = serde_json::from_str(message).expect("MESSAGE is one JSON line");
    assert_eq!(value["level"], "WARN");
    assert_eq!(value["type"], "event");
    assert_eq!(value["fields"]["message"], "warning");

    let second = recv(&socket).expect("the metrics datagram");
    assert_eq!(field(&second, "PRIORITY"), Some("6"), "{second}");
    assert_eq!(field(&second, "M6_TYPE"), Some("perf"), "{second}");
    let message = field(&second, "MESSAGE").expect("MESSAGE");
    let value: serde_json::Value = serde_json::from_str(message).expect("MESSAGE is JSON");
    assert_eq!(value["type"], "perf");
    assert_eq!(value["fields"]["perf"]["requests_total"], 7);

    assert!(
        recv(&socket).is_none(),
        "one line is one datagram, and there is no third"
    );
    m6_log::stop().expect("a clean stop");
}

fn an_oversized_datagram_is_refused_and_counted() {
    let dir = tempfile::tempdir().expect("a directory");
    let (socket, _path) = receiver(dir.path());
    // The field cap is raised so the line reaches the datagram limit rather
    // than being refused for its field, which is a different refusal.
    m6_log::set_limits(LogLimits {
        buffer_start: 16 * 1024,
        buffer_max: 1024 * 1024,
        field_max: 128 * 1024,
        file: "the test".to_string(),
    });
    m6_log::init(Level::Info).expect("the logger");

    let wide = "x".repeat(m6_log::DATAGRAM_MAX + 1024);
    m6_log::with_operational(|sink| {
        Line::begin(sink, Level::Info, "wide")
            .str("value", &wide)
            .end();
    });
    m6_log::flush();

    // The wide line is not sent. The dropped-count line that follows it is a
    // line like any other, so it arrives as its own datagram.
    let report = recv(&socket).expect("the dropped-count line");
    let message = field(&report, "MESSAGE").expect("MESSAGE");
    assert!(
        !message.contains("\"message\":\"wide\""),
        "the refused line reached the journal: {message}"
    );
    assert!(
        message.contains("log lines dropped"),
        "the line that arrived is the dropped-count report: {message}"
    );
    assert!(recv(&socket).is_none(), "there is no third datagram");
    assert_eq!(
        m6_log::operational_lines(),
        2,
        "the refused line and the dropped-count report were both built"
    );
    assert_eq!(
        m6_log::operational_dropped(),
        1,
        "the refused line is counted as dropped"
    );
    m6_log::stop().expect("a clean stop");
}

fn a_failed_send_falls_back_to_stdout() {
    let dir = tempfile::tempdir().expect("a directory");
    let (socket, path) = receiver(dir.path());
    let out_path = dir.path().join("stdout.log");
    let out = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&out_path)
        .expect("the stdout file");
    let saved = m6_platform::fd::dup(&std::io::stdout()).expect("a copy of stdout");
    m6_platform::fd::dup2_stdout(&out).expect("stdout is the file");
    m6_log::init(Level::Info).expect("the logger");

    // The receiver goes away with its path, so the next send fails.
    drop(socket);
    std::fs::remove_file(&path).expect("remove the journal");

    m6_log::with_operational(|sink| {
        Line::begin(sink, Level::Info, "after the send failed").end();
    });
    m6_log::flush();
    m6_log::stop().expect("a clean stop");
    m6_platform::fd::dup2_stdout(&saved).expect("stdout is back");

    let written = std::fs::read_to_string(&out_path).expect("the stdout file");
    assert!(
        written.contains("\"message\":\"after the send failed\""),
        "the line a failed journal send could not take went to stdout: {written}"
    );
}

/// A real journald, which only a Linux host with systemd has.
fn a_real_journald_carries_the_fields() {
    std::env::remove_var(TEST_SOCKET);
    let socket = Path::new("/run/systemd/journal/socket");
    if !socket.exists() {
        println!(
            "NOT MEASURED: no journald socket at {} on this host; the native fields are checked by the phases above and by CI on Linux",
            socket.display()
        );
        return;
    }
    let journalctl = std::process::Command::new("journalctl")
        .arg("--version")
        .output();
    if journalctl.is_err() {
        println!("NOT MEASURED: journalctl is not installed on this host");
        return;
    }
    // The phase proves the socket path end to end on a host where it exists:
    // it points the logger at the real socket, writes one line and reads it
    // back out of the journal. The variable systemd sets is read only by the
    // platform layer, whose own test covers it.
    std::env::set_var(TEST_SOCKET, socket);
    m6_log::init(Level::Info).expect("the logger");
    let marker = format!("m6 journald test {}", std::process::id());
    m6_log::info!(marker = %marker, "journald probe");
    m6_log::flush();
    m6_log::stop().expect("a clean stop");
    std::env::remove_var(TEST_SOCKET);

    let out = std::process::Command::new("journalctl")
        .args(["--output=json", "--since", "-2 min", "M6_TYPE=event"])
        .output()
        .expect("journalctl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let found = text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|entry| {
            entry["MESSAGE"]
                .as_str()
                .is_some_and(|message| message.contains(&marker))
        });
    let Some(entry) = found else {
        panic!(
            "journalctl did not carry the probe with M6_TYPE=event; its stderr was {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    assert_eq!(entry["M6_TYPE"], "event", "{entry}");
    assert_eq!(entry["PRIORITY"], "6", "{entry}");
    assert!(
        entry["SYSLOG_IDENTIFIER"]
            .as_str()
            .is_some_and(|name| name.starts_with("journald")),
        "the program's name: {entry}"
    );
    let message: serde_json::Value =
        serde_json::from_str(entry["MESSAGE"].as_str().expect("MESSAGE is a string"))
            .expect("MESSAGE is the JSON line");
    assert_eq!(message["fields"]["marker"], marker);
}
