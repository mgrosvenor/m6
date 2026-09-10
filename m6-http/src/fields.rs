//! Request field validation shared by HTTP/2 and HTTP/3.
//!
//! **Every rule here is RFC 9113 or RFC 9114, not RFC 9110.** That matters for
//! where this lives. `m6-decisions.md` used to justify keeping h2 and h3 in
//! `m6-http` by claiming the one symbol the H3 path imports from `http2.rs` is
//! "RFC 9110 semantics, not h2 wire format". Checked against the code, that is
//! backwards: the rules are 8.2.1 (lowercase field names), 8.2.2
//! (connection-specific fields and `TE: trailers`), 8.3 (pseudo-header set,
//! ordering, at-most-once) and 8.3.1 (CONNECT, `:path`, `:authority`/`Host`).
//! HTTP/1.1 has none of those concepts; it has no pseudo-headers, it allows
//! any field-name case, and it *requires* `Connection`.
//!
//! So this must not move to `m6-core`, which is protocol-version-independent.
//! It moved out of `http2.rs` and into a module named for what it is, because
//! the H3 path importing from a file called `http2.rs` is what made it look
//! like a layering problem when it is only a naming one. Both protocols
//! import it from here.
//!
//! RFC 9114 4.3 restates RFC 9113 8.3 almost verbatim: same pseudo-header set,
//! same ordering rule, same at-most-once rule, same ban on connection-specific
//! fields. Only the error code the transport reports differs, which is why one
//! implementation serves both.

/// Validate a decoded request header list (RFC 9113 8.3.1, 8.2.1).
///
/// Returns `Err` with a short reason when the request is malformed. Every one
/// of these is a **stream error of type PROTOCOL_ERROR**, not something to
/// serve.
///
/// m6 did none of this: it took whatever HPACK produced and served a 200. An
/// uppercase field name, an unknown pseudo-header, a missing or empty `:path`,
/// a `Connection:` header, a duplicated `:method` -- all were accepted and
/// answered normally. That was 21 of the 49 h2spec failures measured on
/// 2026-09-09, the single largest cluster, and it is pure input validation
/// over an already-decoded list: no stream state or framing involvement.
///
/// The rules, in the order the RFC states them:
///
/// - **8.2.1**: field names must be lowercase. HTTP/2 has no case-insensitive
///   header names on the wire -- uppercase is malformed, not normalised.
/// - **8.2.2**: connection-specific fields must not appear. They are HTTP/1.1
///   hop-by-hop metadata with no meaning in H2, and forwarding one into an H1
///   backend is the smuggling primitive `check_forwardable` guards on egress.
///   `TE` is the one exception, and only with the exact value `trailers`.
/// - **8.3**: pseudo-headers precede regular fields, must be known, must not
///   repeat, and a request must not carry a response pseudo-header.
/// - **8.3.1**: `:method`, `:scheme` and `:path` are mandatory for anything
///   that is not CONNECT, and `:path` must not be empty.
pub(crate) fn validate_request_headers(headers: &[(String, String)]) -> Result<(), &'static str> {
    validate_request_header_bytes(headers.iter().map(|(k, v)| (k.as_bytes(), v.as_bytes())))
}

/// The rules above, over raw bytes, so HTTP/3 can share them.
///
/// RFC 9114 4.3 restates RFC 9113 8.3 almost verbatim: HTTP/3 has the same
/// pseudo-header set, the same "pseudo-headers first" ordering, the same
/// at-most-once rule, and the same ban on connection-specific fields. The only
/// thing that differs is the error code the transport reports.
///
/// H3 headers arrive as `quiche::h3::Header` byte slices, so keeping the core
/// on `&[u8]` lets both paths call it without allocating a `String` per field
/// merely to check it. Duplicating the rules per protocol was the alternative,
/// and duplicated validation drifts.
pub fn validate_request_header_bytes<'a, I>(headers: I) -> Result<(), &'static str>
where
    I: IntoIterator<Item = (&'a [u8], &'a [u8])>,
{
    // Connection-specific fields (8.2.2). `upgrade` is included: neither HTTP/2
    // nor HTTP/3 has an upgrade mechanism, so its presence is always malformed.
    const CONNECTION_SPECIFIC: &[&[u8]] =
        &[b"connection", b"keep-alive", b"proxy-connection", b"transfer-encoding", b"upgrade"];

    let mut seen_regular = false;
    let (mut method, mut scheme, mut path, mut authority) = (0u32, 0u32, 0u32, 0u32);
    let mut path_value: Option<&[u8]> = None;
    let mut method_value: Option<&[u8]> = None;
    let mut scheme_value: Option<&[u8]> = None;
    let mut seen_host = false;

    for (name, value) in headers {
        if name.is_empty() {
            return Err("empty field name");
        }
        // 8.2.1 -- lowercase only. Checked before anything else, because every
        // comparison below assumes it.
        if name.iter().any(|b| b.is_ascii_uppercase()) {
            return Err("uppercase field name");
        }

        if name.first() == Some(&b':') {
            let pseudo = &name[1..];
            // 8.3 -- pseudo-headers must all precede regular fields.
            if seen_regular {
                return Err("pseudo-header after regular field");
            }
            match pseudo {
                b"method"    => { method += 1; method_value = Some(value); }
                b"scheme"    => { scheme += 1; scheme_value = Some(value); }
                b"path"      => { path += 1; path_value = Some(value); }
                b"authority" => authority += 1,
                // `:status` is a RESPONSE pseudo-header; in a request it is
                // malformed rather than merely unknown, but the outcome is the
                // same and the reason is more useful spelled out.
                b"status"    => return Err("response pseudo-header in request"),
                _            => return Err("unknown pseudo-header"),
            }
        } else {
            seen_regular = true;
            if CONNECTION_SPECIFIC.contains(&name) {
                return Err("connection-specific header field");
            }
            if name == b"host".as_slice() {
                seen_host = true;
            }
            // 8.2.2: TE may appear, but only as exactly `trailers`.
            if name == b"te".as_slice() && value != b"trailers".as_slice() {
                return Err("TE header with a value other than trailers");
            }
        }
    }

    // 8.3 -- each pseudo-header at most once.
    if method > 1 || scheme > 1 || path > 1 || authority > 1 {
        return Err("duplicate pseudo-header");
    }

    // 8.3.1 -- CONNECT omits :scheme and :path and requires :authority.
    // Everything else requires all three.
    if method_value == Some(b"CONNECT".as_slice()) {
        if scheme != 0 || path != 0 {
            return Err("CONNECT with :scheme or :path");
        }
        if authority == 0 {
            return Err("CONNECT without :authority");
        }
        return Ok(());
    }

    if method == 0 { return Err("missing :method"); }
    if scheme == 0 { return Err("missing :scheme"); }
    if path == 0   { return Err("missing :path"); }


    // 8.3.1 -- :path must not be empty. `OPTIONS *` is the one legitimate
    // asterisk-form, carried as :path = "*".
    match path_value {
        Some(b"") => return Err("empty :path"),
        Some(b"*") if method_value != Some(b"OPTIONS".as_slice()) => {
            return Err("asterisk :path on non-OPTIONS")
        }
        _ => {}
    }

    // RFC 9113 8.3.1 and RFC 9114 4.3.1, the same sentence in both: if :scheme
    // names a scheme with a mandatory authority component (http and https do),
    // the request MUST carry either :authority or a Host header.
    //
    // A request with neither names no origin at all, so the only thing left to
    // route it by is a server-side default. That ambiguity is the routing-
    // confusion primitive, which is why both RFCs make it malformed rather than
    // something to paper over with a default.
    //
    // Host satisfies it, because a proxy fronting HTTP/1.1 may forward Host
    // rather than synthesise :authority. Both of m6's own clients
    // (`h2s_client`, `h2c_client`) always send :authority, so neither the edge
    // to origin path nor any conforming browser is affected.
    //
    // Checked last so a request with a more specific defect still reports that
    // defect rather than this one.
    if matches!(scheme_value, Some(b"http") | Some(b"https")) && authority == 0 && !seen_host {
        return Err("neither :authority nor Host");
    }

    Ok(())
}

#[cfg(test)]
mod pseudo_header_tests {
    use super::validate_request_headers;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    fn ok(pairs: &[(&str, &str)]) {
        assert_eq!(validate_request_headers(&h(pairs)), Ok(()), "should be valid: {pairs:?}");
    }
    fn bad(pairs: &[(&str, &str)]) -> &'static str {
        validate_request_headers(&h(pairs))
            .expect_err(&format!("should be rejected: {pairs:?}"))
    }

    const GOOD: &[(&str, &str)] =
        &[(":method", "GET"), (":scheme", "https"), (":path", "/"), (":authority", "example.com")];

    /// A well-formed request must still pass. Without this the whole module
    /// could "fix" 21 failures by rejecting everything.
    #[test]
    fn well_formed_requests_pass() {
        ok(GOOD);
        ok(&[(":method", "GET"), (":scheme", "https"), (":path", "/x"),
             (":authority", "example.com"), ("user-agent", "curl/8"), ("accept", "*/*")]);
        // TE: trailers is the one permitted connection-ish header.
        ok(&[(":method", "GET"), (":scheme", "https"), (":path", "/"),
             (":authority", "example.com"), ("te", "trailers")]);
        // OPTIONS * is the legitimate asterisk-form.
        ok(&[(":method", "OPTIONS"), (":scheme", "https"), (":path", "*"),
             (":authority", "example.com")]);
    }

    /// RFC 9113 8.3.1 / RFC 9114 4.3.1: an http or https request must name its
    /// origin, via :authority or Host.
    ///
    /// These three cases previously read as well-formed, which is what h3spec's
    /// "MUST send H3_MESSAGE_ERROR if mandatory pseudo-header fields are
    /// absent" was catching: it sends exactly :method/:scheme/:path and nothing
    /// else. The assertions above were updated rather than worked around,
    /// because they encoded the permissive behaviour rather than the rule.
    #[test]
    fn a_request_must_name_its_origin() {
        assert_eq!(
            bad(&[(":method", "GET"), (":scheme", "https"), (":path", "/")]),
            "neither :authority nor Host"
        );
        // Host alone satisfies it: a proxy fronting HTTP/1.1 may forward Host
        // instead of synthesising :authority.
        ok(&[(":method", "GET"), (":scheme", "https"), (":path", "/"),
             ("host", "example.com")]);
        // A scheme with no mandatory authority component is exempt.
        ok(&[(":method", "GET"), (":scheme", "ftp"), (":path", "/")]);
        // A more specific defect still reports itself, not this rule.
        assert_eq!(
            bad(&[(":method", "GET"), (":scheme", "https"), (":path", "")]),
            "empty :path"
        );
    }

    /// RFC 9113 8.2.1. HTTP/2 field names are lowercase on the wire; uppercase
    /// is malformed, NOT something to normalise. h2spec:
    /// "Sends a HEADERS frame that contains the header field name in uppercase letters".
    #[test]
    fn uppercase_field_names_are_rejected() {
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"), (":path", "/"),
                         ("User-Agent", "x")]), "uppercase field name");
        assert_eq!(bad(&[(":Method", "GET")]), "uppercase field name");
    }

    /// RFC 9113 8.3. h2spec: "Sends a HEADERS frame that contains a unknown
    /// pseudo-header field".
    #[test]
    fn unknown_and_misplaced_pseudo_headers_are_rejected() {
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"), (":path", "/"),
                         (":unknown", "x")]), "unknown pseudo-header");
        // A response pseudo-header has no place in a request.
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"), (":path", "/"),
                         (":status", "200")]), "response pseudo-header in request");
        // Pseudo-headers must all come first.
        assert_eq!(bad(&[(":method", "GET"), ("user-agent", "x"), (":path", "/")]),
                   "pseudo-header after regular field");
    }

    /// RFC 9113 8.3.1. h2spec: "Sends a HEADERS frame with empty \":path\"".
    #[test]
    fn mandatory_pseudo_headers_are_enforced() {
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"), (":path", "")]), "empty :path");
        assert_eq!(bad(&[(":scheme", "https"), (":path", "/")]), "missing :method");
        assert_eq!(bad(&[(":method", "GET"), (":path", "/")]), "missing :scheme");
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https")]), "missing :path");
        // Asterisk-form is only legal for OPTIONS.
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"), (":path", "*")]),
                   "asterisk :path on non-OPTIONS");
    }

    #[test]
    fn duplicate_pseudo_headers_are_rejected() {
        assert_eq!(bad(&[(":method", "GET"), (":method", "POST"),
                         (":scheme", "https"), (":path", "/")]), "duplicate pseudo-header");
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"),
                         (":path", "/"), (":path", "/y")]), "duplicate pseudo-header");
    }

    /// RFC 9113 8.2.2. These are HTTP/1.1 hop-by-hop metadata with no meaning
    /// in H2, and forwarding one into an H1 backend is exactly the smuggling
    /// primitive `check_forwardable` guards against on egress. Rejecting on
    /// ingress means it never gets that far.
    #[test]
    fn connection_specific_fields_are_rejected() {
        for f in ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade"] {
            let mut v = GOOD.to_vec();
            v.push((f, "x"));
            assert_eq!(bad(&v), "connection-specific header field", "for {f}");
        }
        // TE is permitted, but only as exactly "trailers".
        let mut v = GOOD.to_vec();
        v.push(("te", "gzip"));
        assert_eq!(bad(&v), "TE header with a value other than trailers");
    }

    /// CONNECT is the one method that legitimately omits :scheme and :path,
    /// and it requires :authority. Getting this wrong would break the method
    /// while "fixing" conformance.
    #[test]
    fn connect_has_its_own_rules() {
        assert_eq!(validate_request_headers(&h(&[(":method", "CONNECT"),
                                                 (":authority", "example.com:443")])), Ok(()));
        assert_eq!(bad(&[(":method", "CONNECT"), (":authority", "x"), (":path", "/")]),
                   "CONNECT with :scheme or :path");
        assert_eq!(bad(&[(":method", "CONNECT")]), "CONNECT without :authority");
    }

    #[test]
    fn empty_field_name_is_rejected() {
        assert_eq!(bad(&[("", "x")]), "empty field name");
    }
}
