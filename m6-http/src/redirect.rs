//! Plain-HTTP `:80` listener that answers every request with a 301 to HTTPS.
//!
//! Runs as its **own m6-http process**, not as an extra listener inside the
//! `:443` instance. Different port, different failure domain: a slow or
//! malicious client here cannot stall TLS serving, because it is not sharing
//! that process's event loop. It also means this mode never builds QUIC, TLS,
//! backends, the cache or the route table — none of which a redirect needs.
//!
//! It replaces `deploy/http-redirect.py`, a Python shim that existed only
//! because m6-http had no redirect mode. That shim ran a single-threaded
//! blocking `http.server`, so one client holding a connection open stalled
//! every redirect on the node; the loop below is non-blocking throughout.
//!
//! Enable with `[server] redirect_bind = "0.0.0.0:80"`.
use crate::poller::{Poller, Token};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const TOKEN_LISTENER: Token = Token(0);
const TOKEN_CONN: Token = Token(1);

/// Cap on request bytes read before answering. A redirect needs the request
/// line and `Host`; anything beyond this is either a body we will never read
/// or an attempt to make us buffer indefinitely.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// How long a connection may take to send a complete header block. Without
/// this, a client that opens a socket and sends one byte a minute occupies a
/// slot forever — the classic slowloris.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// How often to sweep for timed-out connections when nothing else is happening.
const POLL_TIMEOUT_MS: i32 = 1_000;

struct Conn {
    stream:  TcpStream,
    buf:     Vec<u8>,
    started: Instant,
}

/// Percent-safe: the request target is echoed back into `Location` verbatim,
/// so anything that could terminate the header or inject one is rejected
/// rather than sanitised. A redirect has no reason to accept those.
fn target_is_safe(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 2048
        && t.starts_with('/')
        && !t.contains(['\r', '\n', '\0'])
}

/// A `Host` we are willing to echo into a `Location`. Rejects anything that
/// is not plausibly a hostname, for the same reason as above.
fn host_is_safe(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && !h.contains(['\r', '\n', '\0', '/', '\\', ' '])
}

/// Parse just enough of a request to build the redirect: the target from the
/// request line, and `Host`. Returns `None` while the header block is still
/// incomplete.
fn parse(buf: &[u8]) -> Option<Result<(String, String), &'static str>> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = match std::str::from_utf8(&buf[..end]) {
        Ok(s) => s,
        Err(_) => return Some(Err("request head is not valid UTF-8")),
    };
    let mut lines = head.split("\r\n");

    let Some(request_line) = lines.next() else {
        return Some(Err("empty request"));
    };
    // METHOD SP TARGET SP VERSION
    let mut parts = request_line.split(' ');
    let (_method, target) = match (parts.next(), parts.next()) {
        (Some(m), Some(t)) if !m.is_empty() => (m, t),
        _ => return Some(Err("malformed request line")),
    };

    let host = lines
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("host").then(|| v.trim())
        })
        .unwrap_or("");

    // Strip any port: the redirect always goes to the default HTTPS port, and
    // echoing ":80" back would send the client to https://host:80.
    let host = host.split(':').next().unwrap_or("");

    if !host_is_safe(host) {
        return Some(Err("missing or unusable Host header"));
    }
    if !target_is_safe(target) {
        return Some(Err("unusable request target"));
    }
    Some(Ok((host.to_string(), target.to_string())))
}

fn respond(stream: &mut TcpStream, body: &str) {
    // Best-effort: the peer may already be gone, and there is nothing useful
    // to do about it on a redirect.
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn redirect_response(host: &str, target: &str) -> String {
    format!(
        "HTTP/1.1 301 Moved Permanently\r\n\
         Location: https://{host}{target}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n"
    )
}

fn bad_request() -> String {
    "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
}

/// Run the redirect server. Never returns under normal operation.
pub fn run(bind: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind)?;
    listener.set_nonblocking(true)?;
    let listener_fd: RawFd = listener.as_raw_fd();

    let poller = Poller::new()?;
    poller.add(listener_fd, TOKEN_LISTENER)?;

    info!(bind = %bind, "HTTP->HTTPS redirect listener started");

    let mut conns: Vec<Conn> = Vec::new();
    let mut ev_buf = [Token(0); 64];

    loop {
        // Wake on listener readability or any connection becoming readable.
        // The timeout also drives the header-deadline sweep below when idle,
        // so a stalled connection is still reaped on a silent listener.
        if let Err(e) = poller.wait(&mut ev_buf, POLL_TIMEOUT_MS, None) {
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            warn!(error = %e, "redirect: poller error");
            continue;
        }

        // Accept everything pending.
        loop {
            match listener.accept() {
                Ok((stream, _peer)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    stream.set_nodelay(true).ok();
                    poller.add(stream.as_raw_fd(), TOKEN_CONN).ok();
                    conns.push(Conn { stream, buf: Vec::with_capacity(1024), started: Instant::now() });
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    warn!(error = %e, "redirect: accept failed");
                    break;
                }
            }
        }

        // Advance every connection. Non-blocking throughout: a connection that
        // has nothing to give us costs one WouldBlock and is left alone.
        let mut i = 0;
        while i < conns.len() {
            let mut done = false;
            let mut chunk = [0u8; 2048];

            loop {
                match conns[i].stream.read(&mut chunk) {
                    Ok(0) => { done = true; break; }               // peer closed
                    Ok(n) => {
                        if conns[i].buf.len() + n > MAX_HEADER_BYTES {
                            let mut s = conns[i].stream.try_clone().ok();
                            if let Some(ref mut s) = s { respond(s, &bad_request()); }
                            debug!("redirect: header block over {MAX_HEADER_BYTES} bytes");
                            done = true;
                            break;
                        }
                        conns[i].buf.extend_from_slice(&chunk[..n]);
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => { done = true; break; }
                }
            }

            if !done {
                match parse(&conns[i].buf) {
                    Some(Ok((host, target))) => {
                        let body = redirect_response(&host, &target);
                        respond(&mut conns[i].stream, &body);
                        done = true;
                    }
                    Some(Err(reason)) => {
                        debug!(reason, "redirect: rejecting request");
                        let body = bad_request();
                        respond(&mut conns[i].stream, &body);
                        done = true;
                    }
                    None => {
                        // Header block still incomplete — enforce the deadline.
                        if conns[i].started.elapsed() > HEADER_TIMEOUT {
                            debug!("redirect: header timeout");
                            done = true;
                        }
                    }
                }
            }

            if done {
                poller.delete(conns[i].stream.as_raw_fd()).ok();
                conns.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(s: &str) -> Option<Result<(String, String), &'static str>> {
        parse(s.as_bytes())
    }

    #[test]
    fn incomplete_header_block_yields_none() {
        // The whole point of returning None: keep reading, do not guess.
        assert!(head("GET / HTTP/1.1\r\nHost: a.com\r\n").is_none());
        assert!(head("GE").is_none());
        assert!(head("").is_none());
    }

    #[test]
    fn extracts_host_and_target() {
        let (h, t) = head("GET /a/b HTTP/1.1\r\nHost: x.com\r\n\r\n").unwrap().unwrap();
        assert_eq!((h.as_str(), t.as_str()), ("x.com", "/a/b"));
    }

    #[test]
    fn preserves_query_and_encoding_verbatim() {
        let (_, t) = head("GET /p?a=1&b=%20c HTTP/1.1\r\nHost: x.com\r\n\r\n").unwrap().unwrap();
        assert_eq!(t, "/p?a=1&b=%20c");
    }

    #[test]
    fn strips_the_port_from_host() {
        // Echoing ":80" back would send the client to https://x.com:80.
        let (h, _) = head("GET / HTTP/1.1\r\nHost: x.com:80\r\n\r\n").unwrap().unwrap();
        assert_eq!(h, "x.com");
    }

    #[test]
    fn host_header_is_matched_case_insensitively() {
        let (h, _) = head("GET / HTTP/1.1\r\nhOsT:  x.com  \r\n\r\n").unwrap().unwrap();
        assert_eq!(h, "x.com");
    }

    #[test]
    fn rejects_missing_host() {
        // HTTP/1.1 requires it, and without one there is nowhere to redirect to.
        assert!(head("GET / HTTP/1.1\r\n\r\n").unwrap().is_err());
    }

    #[test]
    fn rejects_header_injection_in_host_and_target() {
        // Both are echoed into the Location header, so anything that could
        // terminate or inject a header is refused rather than sanitised.
        assert!(head("GET / HTTP/1.1\r\nHost: x.com\rX: y\r\n\r\n").unwrap().is_err());
        assert!(head("GET /a\rX: y HTTP/1.1\r\nHost: x.com\r\n\r\n").unwrap().is_err());
        assert!(head("GET / HTTP/1.1\r\nHost: x.com/evil.com\r\n\r\n").unwrap().is_err());
        assert!(head("GET / HTTP/1.1\r\nHost: x.com\\evil\r\n\r\n").unwrap().is_err());
    }

    #[test]
    fn rejects_absolute_form_and_non_slash_targets() {
        // An absolute-form target would let a caller pick the redirect host.
        assert!(head("GET http://evil.com/ HTTP/1.1\r\nHost: x.com\r\n\r\n").unwrap().is_err());
        assert!(head("GET * HTTP/1.1\r\nHost: x.com\r\n\r\n").unwrap().is_err());
    }

    #[test]
    fn rejects_malformed_request_line() {
        assert!(head("GET\r\nHost: x.com\r\n\r\n").unwrap().is_err());
        assert!(head(" / HTTP/1.1\r\nHost: x.com\r\n\r\n").unwrap().is_err());
    }

    #[test]
    fn rejects_oversized_host_and_target() {
        let long_host = "a".repeat(300);
        assert!(head(&format!("GET / HTTP/1.1\r\nHost: {long_host}\r\n\r\n")).unwrap().is_err());
        let long_target = format!("/{}", "a".repeat(3000));
        assert!(head(&format!("GET {long_target} HTTP/1.1\r\nHost: x.com\r\n\r\n")).unwrap().is_err());
    }

    #[test]
    fn response_is_a_well_formed_301() {
        let r = redirect_response("x.com", "/a?b=1");
        assert!(r.starts_with("HTTP/1.1 301 Moved Permanently\r\n"));
        assert!(r.contains("Location: https://x.com/a?b=1\r\n"));
        assert!(r.contains("Content-Length: 0\r\n"));
        assert!(r.ends_with("\r\n\r\n"));
    }

    #[test]
    fn any_method_redirects() {
        // POST included: a 301 is the honest answer to "you used the wrong
        // scheme", regardless of method.
        for m in ["GET", "POST", "HEAD", "PUT", "DELETE"] {
            let (h, t) = head(&format!("{m} /x HTTP/1.1\r\nHost: a.com\r\n\r\n")).unwrap().unwrap();
            assert_eq!((h.as_str(), t.as_str()), ("a.com", "/x"));
        }
    }
}
