//! Plain-HTTP `:80` listener that answers every request with a 301 to HTTPS.
//!
//! Runs as its **own m6-http process**, not as an extra listener inside the
//! `:443` instance. Different port, different failure domain: a slow or
//! malicious client here cannot stall TLS serving, because it is not sharing
//! that process's event loop. It also means this mode never builds QUIC, TLS,
//! backends, the cache or the route table — none of which a redirect needs.
//!
//! Enable with `[server] redirect_bind = "0.0.0.0:80"`.
//!
//! # One HTTP/1.1 implementation
//!
//! This file used to contain a second one: 325 lines with its own poll loop,
//! request parser, response writer and connection reaping, none of it shared
//! with the server on `:443`. Predictably, the two drifted.
//!
//! Measured against h1spec, the independent RFC 9112 conformance tester, that
//! implementation scored **5 to 7 out of 32, varying run to run on identical
//! code**, while the real one is exercised by the whole test suite. The reason
//! for both the low score and the variance was one line: it treated a `read`
//! returning 0 as "peer gone" and skipped the parse-and-respond for a request
//! it had already buffered. A client that half-closes its write side after
//! sending — ordinary, RFC-conformant behaviour, and what most scanners and
//! several HTTP libraries do — got **no answer at all about 80% of the time**,
//! the remainder depending on whether the request bytes and the FIN happened
//! to land in the same poll cycle.
//!
//! HTTP/1.1 is the only version reachable here and that is not a limitation to
//! design around: HTTP/2 needs ALPN, which needs TLS, and HTTP/3 needs QUIC,
//! which mandates it. A browser following an `http://` link speaks HTTP/1.1.
//!
//! So this is now the same `Http11Listener` as `:443`, bound without TLS, and
//! the redirect is what it should always have been: a response code.
use crate::forward::HttpRequest;
use crate::http11::{Http11Listener, RequestOutcome};
use crate::poller::{Poller, Token};
use std::io::ErrorKind;
use std::sync::Arc;
use tracing::{info, warn};

const TOKEN_LISTENER: Token = Token(0);
const TOKEN_CONN: Token = Token(1);
const POLL_TIMEOUT_MS: i32 = 1_000;

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

/// Every method this listener answers. It redirects them all, so the list is
/// what it accepts rather than what it implements.
const ALLOW: &str = "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS";

fn ready(status: u16, headers: Vec<(String, String)>) -> RequestOutcome {
    RequestOutcome::Ready(status, headers, Vec::new(), String::new(), Arc::new(Vec::new()))
}

/// The whole of the redirect: one status, one header.
///
/// Two request targets are not resources and so have nothing to redirect to.
/// Both used to be answered with 400, which says the request was malformed
/// when it was not.
fn redirect_for(req: &HttpRequest) -> RequestOutcome {
    // RFC 9110 9.3.6: CONNECT establishes a tunnel through a proxy. This is an
    // origin server and will not be one, so the method is refused -- 405, the
    // answer for a method this resource does not support, with `Allow` naming
    // the ones it does (RFC 9110 15.5.6 requires that field).
    if req.method.eq_ignore_ascii_case("CONNECT") {
        return ready(
            405,
            vec![
                ("Allow".into(), ALLOW.into()),
                ("Content-Length".into(), "0".into()),
            ],
        );
    }

    // RFC 9110 9.3.7: `OPTIONS *` asks about the server rather than any
    // resource, so there is no target to send anywhere. Answering for the
    // server is the whole point of the form.
    if req.method.eq_ignore_ascii_case("OPTIONS") && req.path == "*" {
        return ready(
            200,
            vec![
                ("Allow".into(), ALLOW.into()),
                ("Content-Length".into(), "0".into()),
            ],
        );
    }

    let host = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim())
        .unwrap_or("");

    // The query is part of the target and must survive the hop. It did not:
    // `req.path` is the path alone, so `http://host/x?v=1` redirected to
    // `https://host/x` and every query parameter on an http:// link was
    // silently dropped -- a versioned asset URL, a tracking parameter, a form
    // GET. Found by a test written for the OPTIONS/CONNECT work above.
    let target = match req.query.as_deref() {
        Some(q) => format!("{}?{}", req.path, q),
        None => req.path.clone(),
    };
    let target = target.as_str();

    if !host_is_safe(host) || !target_is_safe(target) {
        // No `Connection` here: the listener owns persistence (RFC 9112 9.3)
        // and drops any copy a handler supplies, so one written here would be
        // silently discarded and read as policy that is not being applied.
        return ready(400, vec![("Content-Length".into(), "0".into())]);
    }

    ready(
        301,
        vec![
            ("Location".into(), format!("https://{host}{target}")),
            ("Content-Length".into(), "0".into()),
        ],
    )
}

pub fn run(bind: &str) -> anyhow::Result<()> {
    // `main` blocks the shutdown signals as its first statement, which makes
    // them deliverable only to core's sigwait thread. Redirect mode returned
    // before ever installing that thread, so SIGTERM was blocked and nothing
    // was listening for it: the process could not be stopped by anything short
    // of SIGKILL. Found by a conformance run that could not reclaim its port.
    //
    // No wake fd and no socket: this loop parks in `poll` with a one second
    // timeout and re-checks the flag each time round, so it needs neither.
    let shutdown = m6_core::signal::ShutdownHandle::install(
        m6_core::signal::Service::new("m6-http-redirect"),
    );

    let mut listener = Http11Listener::bind_plain(bind)?;
    let poller = Poller::new()?;
    poller.add(listener.raw_fd(), TOKEN_LISTENER)?;

    info!(bind = %bind, "HTTP->HTTPS redirect listener started");

    let mut ev_buf = [Token(0); 64];
    loop {
        // The timeout also drives the connection sweep inside `drive_all` when
        // nothing else is happening, so a stalled client is still reaped on an
        // otherwise silent listener.
        if let Err(e) = poller.wait(&mut ev_buf, POLL_TIMEOUT_MS) {
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            warn!(error = %e, "redirect: poller error");
            continue;
        }

        listener.accept_pending(&poller, TOKEN_CONN);
        listener.drive_all(
            |req, _client_ip| redirect_for(req),
            // No URL backends here, so no pending response can ever arrive.
            |_resp, _ctx| (500, Vec::new(), Vec::new(), String::new(), Arc::new(Vec::new())),
            &poller,
        );

        if m6_core::signal::is_shutdown() {
            shutdown.complete();
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, target: &str) -> HttpRequest {
        let raw = format!("{method} {target} HTTP/1.1\r\nHost: mgrosvenor.com\r\n\r\n");
        match m6_core::h1::parse_request(raw.as_bytes()) {
            m6_core::h1::ParseResult::Complete(r) => r,
            _ => panic!("fixture did not parse: {raw:?}"),
        }
    }

    fn status_of(outcome: &RequestOutcome) -> u16 {
        match outcome {
            RequestOutcome::Ready(s, ..) => *s,
            RequestOutcome::Pending { .. } => panic!("a redirect is never pending"),
        }
    }

    fn header_of(outcome: &RequestOutcome, name: &str) -> Option<String> {
        match outcome {
            RequestOutcome::Ready(_, h, ..) => h
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone()),
            RequestOutcome::Pending { .. } => None,
        }
    }

    /// RFC 9110 9.3.7: `OPTIONS *` asks about the server, not a resource, so
    /// there is nothing to redirect. It was answered 400 -- "your request is
    /// malformed" for a request that is not.
    #[test]
    fn options_asterisk_is_answered_for_the_server() {
        let out = redirect_for(&request("OPTIONS", "*"));
        assert_eq!(status_of(&out), 200);
        assert_eq!(header_of(&out, "allow").as_deref(), Some(ALLOW));
    }

    /// RFC 9110 9.3.6: CONNECT is for proxies. This is an origin server, so
    /// the method is refused (405, with Allow per RFC 9110 15.5.6) rather than
    /// called malformed.
    #[test]
    fn connect_is_refused_as_a_method_not_as_a_bad_request() {
        let out = redirect_for(&request("CONNECT", "example.com:443"));
        assert_eq!(status_of(&out), 405);
        assert_eq!(header_of(&out, "allow").as_deref(), Some(ALLOW));
    }

    /// An ordinary request still redirects, and absolute-form -- which the
    /// parser reduces to origin-form -- redirects to this server's own host,
    /// not the one in the target.
    #[test]
    fn ordinary_targets_still_redirect() {
        let out = redirect_for(&request("GET", "/capabilities?v=1"));
        assert_eq!(status_of(&out), 301);
        assert_eq!(
            header_of(&out, "location").as_deref(),
            Some("https://mgrosvenor.com/capabilities?v=1")
        );

        let out = redirect_for(&request("GET", "http://elsewhere.example/x"));
        assert_eq!(
            header_of(&out, "location").as_deref(),
            Some("https://mgrosvenor.com/x"),
            "the Location host comes from Host, never from the request target"
        );
    }

    #[test]
    fn unsafe_targets_and_hosts_are_refused() {
        // These are the response-splitting primitives: anything that could
        // terminate the Location header or start a second one.
        assert!(!target_is_safe("/a\r\nX-Injected: 1"));
        assert!(!target_is_safe("/a\nX: 1"));
        assert!(!target_is_safe("/a\0b"));
        assert!(!target_is_safe("no-leading-slash"));
        assert!(!target_is_safe(""));
        assert!(!target_is_safe(&format!("/{}", "a".repeat(2048))));
        assert!(target_is_safe("/capabilities?v=1"));

        assert!(!host_is_safe("evil.com\r\nX: 1"));
        assert!(!host_is_safe("a/b"));
        assert!(!host_is_safe("a b"));
        assert!(!host_is_safe(""));
        assert!(host_is_safe("mgrosvenor.com"));
        assert!(host_is_safe("mgrosvenor.com:8443"));
    }
}
