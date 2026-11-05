//! A woff2 response is never compressed.
//!
//! `font/woff2` is already compressed, so `should_compress_default` does not
//! list it. Adding it by mistake would waste CPU on every font request and
//! send a `Content-Encoding` header the client never asked for, with a body
//! the client cannot decode as a font. No suite requests a woff2 file, so the
//! mistake would go unnoticed.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{binary, wait, Service};

/// A running `m6-file`, its socket, and the directory both live in.
///
/// Field order is drop order: the service is killed before the directory
/// holding its socket is removed.
struct Server {
    _svc: Service,
    _dir: tempfile::TempDir,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn config_path() -> PathBuf {
    fixtures_dir().join("m6-file-test.conf")
}

/// Spawn `m6-file` on a socket of its own and wait until it is answering.
///
/// Readiness is a successful connect, not an existing path. `bind` creates the
/// socket file before the server calls `listen`, so the file existing proves
/// nothing.
fn spawn_server(id: &str) -> (Server, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join(format!("{id}.sock"));

    let mut svc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(fixtures_dir())
            .arg(config_path())
            .env("M6_SOCKET_OVERRIDE", &socket_path),
    );
    svc.wait_for_path(&socket_path, Duration::from_secs(10));
    assert!(
        wait::for_unix(&socket_path, Duration::from_secs(10)),
        "m6-file created {} but never accepted a connection",
        socket_path.display()
    );

    (
        Server {
            _svc: svc,
            _dir: dir,
        },
        socket_path,
    )
}

/// Send a raw HTTP request over a Unix socket and return the raw response.
fn raw_request(socket_path: &Path, request: &str) -> Vec<u8> {
    let mut stream = UnixStream::connect(socket_path)
        .unwrap_or_else(|e| panic!("connect to {:?}: {}", socket_path, e));
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();

    let method = request.split(' ').next().unwrap_or("");
    m6_core::testkit::read_one(&mut stream, method).expect("read one response")
}

/// Split a raw response into its header block and body bytes.
fn split(raw: &[u8]) -> (&str, &[u8]) {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header terminator");
    let head = std::str::from_utf8(&raw[..sep]).expect("headers are ASCII");
    (head, &raw[sep + 4..])
}

/// Guards against `font/woff2` being added to the compress-by-default list:
/// a GET with `Accept-Encoding: br, gzip` must answer with no
/// `Content-Encoding` header and the file's exact bytes.
#[test]
fn woff2_with_br_and_gzip_is_served_uncompressed_with_exact_bytes() {
    let (_guard, socket_path) = spawn_server("woff2");
    let fixture = fixtures_dir()
        .join("assets")
        .join("fonts")
        .join("example.woff2");
    let expected = std::fs::read(&fixture).expect("fixture");

    let req = "GET /assets/fonts/example.woff2 HTTP/1.1\r\n\
               Host: localhost\r\nAccept-Encoding: br, gzip\r\n\r\n";
    let raw = raw_request(&socket_path, req);
    let (head, body) = split(&raw);

    assert!(head.contains("200 OK"), "expected 200, head:\n{head}");
    assert!(
        !head.to_ascii_lowercase().contains("content-encoding:"),
        "woff2 got a Content-Encoding despite Accept-Encoding: br, gzip, head:\n{head}"
    );
    assert_eq!(
        body,
        expected.as_slice(),
        "woff2 bytes changed on the wire (compressed?)"
    );
}
