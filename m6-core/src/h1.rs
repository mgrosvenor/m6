//! The one HTTP/1.1 request parser.
//!
//! There were four. Measured against h1spec, the independent RFC 9112
//! conformance tester:
//!
//! | implementation | score |
//! |---|---|
//! | `m6-http/src/http11.rs` (this one, moved here) | **27/32** |
//! | `m6-file/src/http.rs` | 15/32 |
//! | `m6-render/src/server.rs` | 15/32 |
//! | `m6-core/src/parse.rs` (deleted) | **14/32** |
//!
//! The migration plan said to consolidate onto `m6-core/src/parse.rs`, which
//! turned out to be the *worst* of the four. Measuring the candidate before
//! moving anything onto it is what caught that; the survivor is the parser the
//! edge already used, because it is the only one that has been attacked.
//!
//! **This function is pure.** The edge's version stripped proxy-owned headers
//! (`X-Forwarded-For` and friends) inline while parsing, which is ingress
//! policy wearing a parser's clothes. That moves to the caller:
//! `m6_http_lib::forward::strip_untrusted_inbound` already exists and does
//! exactly this, and a backend has no proxy headers to strip.

use crate::http::RawRequest;


/// Outcome of parsing a client request. Public so security tests can assert on
/// what the ingress boundary accepts, rejects, and strips.
pub enum ParseResult {
    Incomplete,
    Error,
    Complete(RawRequest),
}

pub fn parse_request(buf: &[u8]) -> ParseResult {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let body_offset = match req.parse(buf) {
        Ok(httparse::Status::Partial) => return ParseResult::Incomplete,
        Ok(httparse::Status::Complete(n)) => n,
        Err(_) => return ParseResult::Error,
    };

    // Reject a bare LF anywhere in the header block.
    //
    // httparse accepts LF alone as a line terminator, which is lenient in the
    // way most servers historically were. The consequence is that a header
    // VALUE containing a raw \n is silently split into two headers:
    //
    //     X-Test: a\nX-Injected: yes   ->   X-Test: a  +  X-Injected: yes
    //
    // and the second one is then forwarded to the backend. Found by a raw
    // socket test; no client library will send this, which is why it survived.
    //
    // Honest scope: this is not a demonstrated exploit against the current
    // topology. Ingress stripping runs after the split, so proxy-owned headers
    // are still removed, and m6-http re-serialises with CRLF, so its own
    // cache-node-to-origin hop cannot desync with itself. The hazard is the
    // classic one from RFC 9112 11.2 -- two hops disagreeing about where a
    // header ends -- and it goes live the moment anything is placed in front
    // of m6. Rejecting is what current hardened servers do, and it costs one
    // scan of a small buffer on a path that has already parsed it once.
    //
    // Scanning `buf[..body_offset]` rather than the whole buffer keeps a body
    // containing legitimate LF bytes out of it.
    {
        let head = &buf[..body_offset];
        let mut i = 0;
        while let Some(off) = head[i..].iter().position(|&c| c == b'\n') {
            let at = i + off;
            if at == 0 || head[at - 1] != b'\r' {
                return ParseResult::Error;
            }
            i = at + 1;
        }
    }

    let method = req.method.unwrap_or("GET").to_string();
    let raw_path = req.path.unwrap_or("/");

    let (path, query) = match raw_path.find('?') {
        Some(q) => (raw_path[..q].to_string(), Some(raw_path[q + 1..].to_string())),
        None => (raw_path.to_string(), None),
    };

    // Extract what we need from req.headers before dropping req.
    //
    // One pass does three jobs — framing validation, ingress stripping, and
    // materialisation — because this loop is the single hottest piece of
    // per-request work in the proxy. It used to be two passes plus a
    // `filter_map().collect()` whose size hint forced the Vec to grow.
    let nheaders = req.headers.len();

    // Request framing must be unambiguous. Two disagreeing `Content-Length`
    // values, or a `Content-Length` alongside a `Transfer-Encoding`, let two
    // hops disagree about where this request ends and the next begins — the
    // basis of request smuggling. We refuse such a request rather than pick a
    // winner and hope the backend picks the same one.
    let mut first_cl: Option<&str> = None;
    let mut saw_cl = false;
    let mut host_count = 0usize;
    let mut host_empty = false;
    let mut fwd_headers: Vec<(String, String)> = Vec::with_capacity(nheaders);

    for h in &req.headers[..nheaders] {
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            // Chunked bodies are not decoded here; accepting one would mean
            // forwarding a body we never read.
            return ParseResult::Error;
        }
        if h.name.eq_ignore_ascii_case("content-length") {
            let Ok(value) = std::str::from_utf8(h.value) else {
                return ParseResult::Error;
            };
            let value = value.trim();
            match first_cl {
                // Duplicates are tolerable only when they agree.
                Some(prev) if prev != value => return ParseResult::Error,
                Some(_) => {}
                None => first_cl = Some(value),
            }
            saw_cl = true;
        }

        if h.name.eq_ignore_ascii_case("host") {
            host_count += 1;
            host_empty = std::str::from_utf8(h.value)
                .map(|v| v.trim().is_empty())
                .unwrap_or(true);
        }

        // A header value that is not UTF-8 is dropped rather than rejected
        // (matching prior behaviour); `content-length` is the exception
        // handled above, where it is a framing error.
        let Ok(v) = std::str::from_utf8(h.value) else { continue };
        fwd_headers.push((h.name.to_string(), v.to_string()));
    }

    // Host validation, RFC 9112 3.2: a server MUST answer 400 to an HTTP/1.1
    // request that lacks a Host, carries more than one, or carries an invalid
    // value. All three returned 200 before this.
    //
    // More than one is the one with teeth: two hops can pick different Host
    // values and disagree about which site — or which origin — a request is
    // for. Rejected regardless of version for that reason.
    //
    // The version gate matters. HTTP/1.0 predates Host and is allowed to omit
    // it, so rejecting a 1.0 request for that would break clients that are
    // behaving correctly. httparse reports `version` as the minor number, so
    // 1 means HTTP/1.1.
    //
    // Note the H2C and HTTP/2 paths do not come through here (they build their
    // request in http2.rs, where `:authority` is already translated to Host),
    // and the synthetic refresh request is constructed directly rather than
    // parsed — so neither is affected by this.
    if host_count > 1 {
        return ParseResult::Error;
    }
    let is_http11 = req.version == Some(1);
    if is_http11 && (host_count == 0 || host_empty) {
        return ParseResult::Error;
    }

    let content_length: usize = match first_cl {
        Some(v) => match v.parse() {
            Ok(n) => n,
            Err(_) => return ParseResult::Error,
        },
        // `saw_cl` without a value is unreachable (the loop sets both
        // together), but keep the framing decision explicit rather than
        // silently defaulting a malformed header to zero.
        None if saw_cl => return ParseResult::Error,
        None => 0,
    };

    drop(req); // release borrow of `headers`

    let available = buf.len() - body_offset;
    if available < content_length {
        return ParseResult::Incomplete;
    }

    let body = buf[body_offset..body_offset + content_length].to_vec();

    ParseResult::Complete(RawRequest {
        method,
        path,
        query,
        // The version the client actually sent, not a constant.
        //
        // This was hardcoded to "HTTP/1.1" for every request, so the field was
        // a lie for anything that read it. It went unnoticed while nothing
        // did: the code above uses httparse's own `req.version` and never this
        // string. Persistent connections need it, because HTTP/1.1 defaults to
        // keeping the connection and HTTP/1.0 defaults to closing it -- and
        // with the constant in place an HTTP/1.0 client was told keep-alive.
        version: if is_http11 { "HTTP/1.1".to_string() } else { "HTTP/1.0".to_string() },
        headers: fwd_headers,
        body,
    })
}
