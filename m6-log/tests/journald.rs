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
fn recv(socket: &UnixDatagram) -> Option<Vec<u8>> {
    let deadline = Instant::now() + m6_core_bound();
    let mut buf = vec![0u8; m6_log::DATAGRAM_MAX];
    loop {
        match socket.recv(&mut buf) {
            Ok(n) => return Some(buf[..n].to_vec()),
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

/// One field of a journal datagram.
#[derive(Debug)]
struct Field {
    name: String,
    value: Vec<u8>,
}

/// Parse a datagram with journald's own rules.
///
/// A field starts with its name up to the next newline. A name followed by
/// `=` is the text form, and its value runs to that newline. A name with no
/// `=` is the binary form: eight bytes of little-endian length follow, then
/// exactly that many value bytes, then a newline. Every field, the last one
/// included, must end with a newline, because journald discards a field that
/// does not. Refusing here is the assertion that catches a datagram journald
/// would read differently from this test.
fn parse(datagram: &[u8]) -> Result<Vec<Field>, String> {
    if datagram.is_empty() {
        return Err("the datagram is empty".to_string());
    }
    let mut fields = Vec::new();
    let mut at = 0;
    while at < datagram.len() {
        let Some(offset) = datagram[at..].iter().position(|&byte| byte == b'\n') else {
            return Err(format!(
                "the last field is unterminated: no newline after byte {at}"
            ));
        };
        let head = &datagram[at..at + offset];
        if head.is_empty() {
            return Err(format!("the field at byte {at} has no name"));
        }
        at += offset + 1;
        if let Some(equals) = head.iter().position(|&byte| byte == b'=') {
            let name = std::str::from_utf8(&head[..equals])
                .map_err(|error| format!("a field name is not UTF-8: {error}"))?;
            fields.push(Field {
                name: name.to_string(),
                value: head[equals + 1..].to_vec(),
            });
            continue;
        }
        let name = std::str::from_utf8(head)
            .map_err(|error| format!("a binary field name is not UTF-8: {error}"))?;
        let length_end = at + 8;
        if length_end > datagram.len() {
            return Err(format!("the binary field {name} has no eight-byte length"));
        }
        let mut length = [0u8; 8];
        length.copy_from_slice(&datagram[at..length_end]);
        let length = u64::from_le_bytes(length) as usize;
        let value_end = length_end + length;
        if value_end + 1 > datagram.len() {
            return Err(format!(
                "the binary field {name} names {length} value bytes, more than the datagram carries"
            ));
        }
        if datagram[value_end] != b'\n' {
            return Err(format!(
                "the binary field {name} is not followed by a newline"
            ));
        }
        fields.push(Field {
            name: name.to_string(),
            value: datagram[length_end..value_end].to_vec(),
        });
        at = value_end + 1;
    }
    Ok(fields)
}

/// The fields of one received datagram, checked first for the trailing
/// newline journald requires.
fn datagram(bytes: &[u8]) -> Vec<Field> {
    assert_eq!(
        bytes.last(),
        Some(&b'\n'),
        "the datagram's last field is unterminated, so journald would discard it: {bytes:?}"
    );
    parse(bytes).unwrap_or_else(|reason| panic!("not a journald datagram: {reason}: {bytes:?}"))
}

/// The value of the field named `name`, or `None` when absent.
fn field<'a>(fields: &'a [Field], name: &str) -> Option<&'a [u8]> {
    fields
        .iter()
        .find(|field| field.name == name)
        .map(|field| field.value.as_slice())
}

/// The UTF-8 value of the field named `name`.
fn text<'a>(fields: &'a [Field], name: &str) -> &'a str {
    let value = field(fields, name).unwrap_or_else(|| panic!("no {name} field: {fields:?}"));
    std::str::from_utf8(value).unwrap_or_else(|error| panic!("{name} is not UTF-8: {error}"))
}

fn fields_and_one_datagram_per_line() {
    let dir = tempfile::Builder::new()
        .prefix("m6-")
        .tempdir_in(m6_platform::socket_scratch_root())
        .expect("a directory");
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

    let first_bytes = recv(&socket).expect("the warning's datagram");
    let first = datagram(&first_bytes);
    assert_eq!(
        field(&first, "PRIORITY"),
        Some(b"4".as_slice()),
        "{first_bytes:?}"
    );
    assert_eq!(
        field(&first, "M6_TYPE"),
        Some(b"event".as_slice()),
        "{first_bytes:?}"
    );
    let identifier = text(&first, "SYSLOG_IDENTIFIER");
    assert!(
        identifier.starts_with("journald"),
        "the program's own name, with the hash cargo gives a test binary: {identifier}"
    );
    let value: serde_json::Value =
        serde_json::from_str(text(&first, "MESSAGE")).expect("MESSAGE is one JSON line");
    assert_eq!(value["level"], "WARN");
    assert_eq!(value["type"], "event");
    assert_eq!(value["fields"]["message"], "warning");

    let second_bytes = recv(&socket).expect("the metrics datagram");
    let second = datagram(&second_bytes);
    assert_eq!(
        field(&second, "PRIORITY"),
        Some(b"6".as_slice()),
        "{second_bytes:?}"
    );
    assert_eq!(
        field(&second, "M6_TYPE"),
        Some(b"perf".as_slice()),
        "{second_bytes:?}"
    );
    let value: serde_json::Value =
        serde_json::from_str(text(&second, "MESSAGE")).expect("MESSAGE is JSON");
    assert_eq!(value["type"], "perf");
    assert_eq!(value["fields"]["perf"]["requests_total"], 7);

    assert!(
        recv(&socket).is_none(),
        "one line is one datagram, and there is no third"
    );
    m6_log::stop().expect("a clean stop");
}

fn an_oversized_datagram_is_refused_and_counted() {
    let dir = tempfile::Builder::new()
        .prefix("m6-")
        .tempdir_in(m6_platform::socket_scratch_root())
        .expect("a directory");
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
    let report_bytes = recv(&socket).expect("the dropped-count line");
    let report = datagram(&report_bytes);
    let message = text(&report, "MESSAGE");
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
    let dir = tempfile::Builder::new()
        .prefix("m6-")
        .tempdir_in(m6_platform::socket_scratch_root())
        .expect("a directory");
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
        // The probe is short, so --all changes nothing here today. It matches the
        // monitor's read, so a probe that grows past 4096 bytes is read whole.
        .args([
            "--all",
            "--output=json",
            "--since",
            "-2 min",
            "M6_TYPE=event",
        ])
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

/// The parser above is the phases' oracle, so it has to be shown to refuse
/// what journald refuses and to decode the binary form journald decodes. A
/// parser that accepted the old datagram would have let the defect through.
#[test]
fn the_parser_refuses_an_unterminated_field_and_decodes_the_binary_form() {
    let old = b"PRIORITY=4\nM6_TYPE=event\nSYSLOG_IDENTIFIER=test\nMESSAGE={}";
    let reason = parse(old).expect_err("journald discards an unterminated last field");
    assert!(reason.contains("unterminated"), "{reason}");

    let text_fields = parse(b"MESSAGE=hello\n").expect("the text form");
    assert_eq!(field(&text_fields, "MESSAGE"), Some(b"hello".as_slice()));

    let mut binary = Vec::from(*b"MESSAGE\n");
    binary.extend_from_slice(&5u64.to_le_bytes());
    binary.extend_from_slice(b"a\nb\nc");
    binary.push(b'\n');
    let fields = parse(&binary).expect("the binary form");
    assert_eq!(field(&fields, "MESSAGE"), Some(b"a\nb\nc".as_slice()));

    binary.pop();
    assert!(
        parse(&binary).is_err(),
        "a binary value without its newline is refused"
    );
}
