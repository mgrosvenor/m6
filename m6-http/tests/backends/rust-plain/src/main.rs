//! An m6 backend in Rust with **no `m6-core`**, written from
//! `docs/m6-backend-protocol.md`.
//!
//! Usage: `m6-example-rust-plain <socket-path> <status-json-path>`
//!
//! Why plain Rust, per `docs/m6-backend-examples.md` §4: backends that must be
//! both fast and safe, with no GC pause between arrival and answer, and where
//! the input is untrusted or the parsing intricate.
//!
//! # This file is the control, and that is its main job
//!
//! Its pair, `rust-m6core`, does exactly the same thing with the socket
//! lifecycle, thread pool, signal handling and content handling supplied by
//! `m6-core` instead of written out. Same language, same compiler, same payload,
//! so the difference between the two is precisely what linking the library
//! costs. It has **no dependencies at all** for that reason: anything linked
//! here that the pair does not link would show up in the delta and be misread as
//! core's overhead.
//!
//! The second thing it measures is the contract itself. If this file were much
//! harder to write than `go/main.go`, the contract would have drifted toward
//! Rust, and that would be a problem with the platform rather than with the
//! example. It is about the same length, which is the answer.
//!
//! Standard library only: `std::os::unix`, `std::net` is not even needed.
//! `docs/m6-backend-protocol.md` §9 is the checklist this implements.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;

const LANGUAGE: &str = "Rust (no m6-core)";

/// The proxy refuses a response whose header section exceeds 8192 bytes
/// (spec §3.2). The same bound is applied to what is accepted, so a peer cannot
/// make this process grow a buffer without limit (spec §6.2).
const MAX_HEADERS: usize = 8192;
const MAX_BODY: usize = 1024 * 1024;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    // Exit 2 means "failed before binding", so a supervisor can tell a
    // misconfiguration from a crash (spec §8.3).
    if args.len() != 3 {
        eprintln!("usage: {} <socket-path> <status-json-path>", args[0]);
        return ExitCode::from(2);
    }
    let sock_path = args[1].clone();
    let status_path = &args[2];

    // The payload is read from the one copy in the repository rather than
    // embedded per language, so byte-identity across the six examples is
    // structural: there is nothing to keep in step.
    let status_body = match fs::read(status_path) {
        Ok(b) => Arc::new(b),
        Err(e) => {
            eprintln!("cannot read status payload {status_path}: {e}");
            return ExitCode::from(2);
        }
    };

    let listener = match bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on {sock_path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Shutdown, spec §8.2: stop accepting, finish what is in flight, remove the
    // socket, exit 0. A second signal exits immediately.
    //
    // Done without a signal-handling crate, which is the point of this example:
    // `signal(2)` through a tiny extern block, a flag, and a close of the
    // listening fd so the blocked `accept` returns. `rust-m6core` gets all of
    // this from core.
    //
    // The fd is published BEFORE the handlers are installed: a SIGTERM arriving
    // between the two would otherwise find -1 and leave the process blocked in
    // accept forever. That is not hypothetical, it is what the first version of
    // this file did, and the shared conformance test for clean shutdown is what
    // found it.
    LISTEN_FD.store(listener.as_raw_fd(), Ordering::SeqCst);
    install_signal_handlers();

    for conn in listener.incoming() {
        if SIGNALLED.load(Ordering::SeqCst) {
            break;
        }
        match conn {
            Ok(stream) => {
                let body = Arc::clone(&status_body);
                // Spec §7 requires only that a new connection can be accepted
                // while another is handled, and allows threads, processes or an
                // event loop. A thread per connection is the clearest to read.
                if thread::Builder::new()
                    .name("conn".into())
                    .spawn(move || serve(stream, &body))
                    .is_err()
                {
                    // Out of threads: the connection is dropped, which closes
                    // it. The proxy reports that as a 502 promptly rather than
                    // waiting out its 30 second timeout (spec §5).
                    continue;
                }
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break, // the listener was shut down by the signal handler
        }
    }
    // Removing the socket is what withdraws this member from the pool. Left
    // behind, the proxy keeps selecting a member that refuses every connection
    // (spec §8.2).
    let _ = fs::remove_file(&sock_path);
    ExitCode::SUCCESS
}

/// Set when SIGTERM or SIGINT arrives. Read by the accept loop.
static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// The listening socket's fd, so the signal handler can close it. `accept` is a
/// blocking call and a flag alone does not interrupt it.
static LISTEN_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_signal(_sig: i32) {
    // Only async-signal-safe operations here. Atomic loads and stores are, and
    // so is close(2); anything that allocates or takes a lock is not, which is
    // why there is no logging in this function.
    if SIGNALLED.swap(true, Ordering::SeqCst) {
        // Second signal: exit immediately, as the spec requires.
        unsafe { libc_exit(0) };
    }
    // Closing the listener is what makes the blocked `accept` return, so the
    // loop notices the flag without polling for it.
    let fd = LISTEN_FD.swap(-1, Ordering::SeqCst);
    if fd >= 0 {
        unsafe { libc_close(fd) };
    }
}

// The three libc calls this example needs. Declared here rather than taking a
// dependency, so the control really does link nothing: see the note at the top
// about anything extra showing up in the measurement as core's cost.
extern "C" {
    #[link_name = "signal"]
    fn libc_signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    #[link_name = "_exit"]
    fn libc_exit(code: i32) -> !;
    #[link_name = "close"]
    fn libc_close(fd: i32) -> i32;
}

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
const SIGPIPE: i32 = 13;
const SIG_IGN: usize = 1;

fn install_signal_handlers() {
    unsafe {
        libc_signal(SIGTERM, handle_signal);
        libc_signal(SIGINT, handle_signal);
        // A write to a socket the proxy has already closed would otherwise kill
        // the process. Rust's runtime already ignores SIGPIPE, but saying so
        // keeps this example readable beside the C one, which must do it.
        let ignore: extern "C" fn(i32) = std::mem::transmute(SIG_IGN);
        libc_signal(SIGPIPE, ignore);
    }
}

/// The binding sequence, spec §1.2, in order.
fn bind(path: &str) -> std::io::Result<UnixListener> {
    // 0. The parent directory SHOULD be created if absent.
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    // 1. Remove any existing file. A stale socket from an unclean exit makes
    //    bind fail with EADDRINUSE.
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // 2. Bind, and 4. listen. `UnixListener::bind` does both, with a backlog of
    //    128 on Linux and macOS, which satisfies the SHOULD of 64.
    let listener = UnixListener::bind(path)?;
    // 3. chmod 0666. The proxy runs as a different user and cannot connect
    //    otherwise. The spec calls this the single most common cause of a
    //    backend that starts cleanly and is never contacted.
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// Read the request, route it, respond, close. One request per connection
/// (spec §1.3): nothing here loops waiting for a second.
fn serve(mut stream: UnixStream, status_body: &[u8]) {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];

    let sep = loop {
        if buf.len() >= MAX_HEADERS {
            break None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break find_sep(&buf),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = find_sep(&buf) {
                    break Some(i);
                }
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    };

    let Some(sep) = sep else {
        // 400 rather than silence: the proxy reports a dropped connection as
        // 502, which would blame the wrong side.
        let page = html_page("400 Bad Request", "Malformed or oversized request.");
        respond(
            &mut stream,
            400,
            "Bad Request",
            "text/html; charset=utf-8",
            page.as_bytes(),
            false,
        );
        return;
    };

    let head = &buf[..sep];
    let first_line_end = find(head, b"\r\n").unwrap_or(head.len());
    let request_line = &head[..first_line_end];

    let mut parts = request_line.split(|b| *b == b' ');
    let Some(method) = parts.next() else {
        let page = html_page("400 Bad Request", "Unparseable request line.");
        respond(
            &mut stream,
            400,
            "Bad Request",
            "text/html; charset=utf-8",
            page.as_bytes(),
            false,
        );
        return;
    };
    let Some(target) = parts.next() else {
        let page = html_page("400 Bad Request", "Unparseable request line.");
        respond(
            &mut stream,
            400,
            "Bad Request",
            "text/html; charset=utf-8",
            page.as_bytes(),
            false,
        );
        return;
    };

    let head_only = method == b"HEAD";

    // Drain exactly Content-Length bytes. Reading beyond blocks until the
    // proxy's timeout (spec §2.6); leaving it unread leaves bytes queued on a
    // connection about to close. Nothing here uses the body.
    if let Some(want) = content_length(head) {
        if want > 0 && want <= MAX_BODY {
            let mut have = buf.len() - (sep + 4);
            while have < want {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => have += n,
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }
    }

    // The target is NOT percent-decoded by the proxy (spec §2.1). Nothing below
    // uses it to touch the filesystem, so it is matched as received bytes. The
    // query string is stripped so /status?x=1 is /status.
    let path = match find(target, b"?") {
        Some(i) => &target[..i],
        None => target,
    };

    match path {
        b"/" => {
            let page = html_page(
                &format!("m6 backend example: {LANGUAGE}"),
                &format!("A minimal m6 backend written in {LANGUAGE}."),
            );
            respond(
                &mut stream,
                200,
                "OK",
                "text/html; charset=utf-8",
                page.as_bytes(),
                head_only,
            );
        }
        // Byte-identical across every example: the one file, served verbatim.
        b"/status" => respond(
            &mut stream,
            200,
            "OK",
            "application/json; charset=utf-8",
            status_body,
            head_only,
        ),
        b"/health" => respond(
            &mut stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            b"ok",
            head_only,
        ),
        // Fails on purpose. The proxy counts 5xx as a backend error and may
        // replace it with a styled error page (spec §4).
        b"/boom" => {
            let page = html_page(
                "500 Internal Server Error",
                "This endpoint fails on purpose.",
            );
            respond(
                &mut stream,
                500,
                "Internal Server Error",
                "text/html; charset=utf-8",
                page.as_bytes(),
                head_only,
            );
        }
        // A real 404, not a 200 with an error page, so the status stays honest
        // to caches and crawlers (spec §4).
        _ => {
            let page = html_page(
                "404 Not Found",
                &format!("This {LANGUAGE} backend does not serve that path."),
            );
            respond(
                &mut stream,
                404,
                "Not Found",
                "text/html; charset=utf-8",
                page.as_bytes(),
                head_only,
            );
        }
    }
}

/// Status line, `Content-Type` with a charset, an accurate `Content-Length`,
/// then the body unless the request forbids one.
///
/// `head_only` covers spec §3.3: no body for HEAD, 1xx, 204 or 304. For HEAD the
/// `Content-Length` of the equivalent GET is still sent, which is what lets the
/// proxy frame the response without waiting for bytes that never arrive.
fn respond(
    stream: &mut UnixStream,
    status: u16,
    reason: &str,
    ctype: &str,
    body: &[u8],
    head_only: bool,
) {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    // One write for the whole response where possible: two writes on a stream
    // socket can be two datagrams' worth of syscalls for no reason.
    let mut out = Vec::with_capacity(head.len() + if head_only { 0 } else { body.len() });
    out.extend_from_slice(head.as_bytes());
    if !head_only {
        out.extend_from_slice(body);
    }
    // A closed peer is survivable and not worth a panic.
    let _ = stream.write_all(&out);
    let _ = stream.flush();
}

fn html_page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><meta charset=\"utf-8\">\n<title>{title}</title>\n<h1>{title}</h1>\n<p>{detail}</p>\n</html>\n"
    )
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn find_sep(buf: &[u8]) -> Option<usize> {
    find(buf, b"\r\n\r\n")
}

/// `Content-Length` from the header section, case-insensitively.
fn content_length(head: &[u8]) -> Option<usize> {
    let mut rest = head;
    while let Some(nl) = find(rest, b"\r\n") {
        let line = &rest[..nl];
        if let Some(colon) = find(line, b":") {
            let (name, value) = (&line[..colon], &line[colon + 1..]);
            if name.eq_ignore_ascii_case(b"content-length") {
                return std::str::from_utf8(value).ok()?.trim().parse().ok();
            }
        }
        rest = &rest[nl + 2..];
    }
    None
}
