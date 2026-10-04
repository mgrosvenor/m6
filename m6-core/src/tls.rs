//! What the certificate this process loaded says about its own expiry.
//!
//! Certificate renewal is done by something outside m6, typically an *ACME*
//! (Automatic Certificate Management Environment) client on a timer. The
//! documented failure mode of that arrangement is silence: the renewal fails,
//! the timer keeps firing and keeps failing, nothing looks, and the first
//! signal is the server refusing connections on the day the certificate
//! expires. A server already holds the answer, so it is the cheapest place to
//! ask.
//!
//! Two halves live here. [`not_after_unix`] reads `notAfter` out of a DER
//! certificate, and [`record_loaded`] stores what the server loaded so the
//! monitoring endpoints can report it without re-reading a file. The second
//! half matters as much as the first: a path on disk and the material a running
//! process is serving are different facts, and reading the file would report
//! the one nobody asked about. A certificate renewed on disk and never reloaded
//! is exactly the case an operator needs to see.
//!
//! # Why the parse is written out here
//!
//! `rustls` hands over `CertificateDer` and parses no validity from it, and
//! `rustls-webpki` keeps its own parse private. Reading one integer field
//! therefore meant either a general ASN.1 dependency, eight crates for one
//! number, or the walk below. `notAfter` sits at a fixed structural position in
//! every X.509 certificate, so the walk is tag-length-value skipping over a
//! known shape rather than general parsing.
//!
//! The dates themselves go through `chrono`, which m6-core already depends on.
//! Civil-from-days arithmetic looks like four lines and is not: `m6-md` wrote
//! its own and it was wrong for 7,281 days out of 29,200. That history is in
//! `util::iso_date_from`, and it is the reason no date arithmetic is written
//! here either.

use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// Why a certificate could not be read.
///
/// Separate variants rather than one string because each one says something
/// different about where to look: a truncated buffer is a different problem
/// from a well-formed certificate whose `notAfter` this code cannot represent.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CertificateError {
    /// An element ran past the end of the buffer.
    #[error("certificate is truncated")]
    Truncated,
    /// A length that DER forbids: the indefinite form, or one too wide to
    /// address the buffer it came in.
    #[error("certificate carries a length DER does not allow")]
    BadLength,
    /// The outer shape is not `Certificate ::= SEQUENCE { tbsCertificate ... }`.
    #[error("not an X.509 certificate")]
    NotACertificate,
    /// The certificate parsed as far as the validity field and that field is
    /// not the `SEQUENCE { notBefore, notAfter }` X.509 requires.
    #[error("certificate has no validity period")]
    NoValidity,
    /// `notAfter` is present and is not a UTC time this code can read. RFC 5280
    /// §4.1.2.5 narrows ASN.1 to `YYMMDDHHMMSSZ` and `YYYYMMDDHHMMSSZ`, both
    /// UTC with seconds and a trailing `Z`, which is what this accepts.
    #[error("certificate has an unreadable notAfter")]
    BadTime,
}

/// One certificate out of the chain the server loaded.
///
/// `depth` is the position as loaded, so 0 is the leaf: the certificate a
/// client validates against the hostname, and the one an *ACME* client renews.
/// The intermediates are carried too because they expire as well, on a much
/// longer cycle, and a chain is only good until its soonest expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsCertificate {
    pub depth: usize,
    /// `notAfter`, as seconds since the Unix epoch.
    pub not_after_unix: i64,
    /// Seconds from the moment the report was built until `not_after_unix`.
    ///
    /// Negative on a certificate that has already expired, which is a state
    /// worth reporting as a number rather than clamping to zero: how long ago
    /// is the difference between a renewal that failed last night and one that
    /// stopped running in March.
    pub expires_in_seconds: i64,
}

/// `notAfter` from a DER certificate, as seconds since the Unix epoch.
///
/// Takes the bytes rather than a path so the caller passes the material it
/// actually loaded.
pub fn not_after_unix(der: &[u8]) -> Result<i64, CertificateError> {
    // Tags, named rather than spelled at each use. Only four are needed: this
    // walk skips six elements without caring what they hold.
    const SEQUENCE: u8 = 0x30;
    const CONTEXT_0: u8 = 0xA0;
    const UTC_TIME: u8 = 0x17;
    const GENERALIZED_TIME: u8 = 0x18;

    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
    let (tag, certificate, _) = element(der)?;
    if tag != SEQUENCE {
        return Err(CertificateError::NotACertificate);
    }
    let (tag, tbs, _) = element(certificate)?;
    if tag != SEQUENCE {
        return Err(CertificateError::NotACertificate);
    }

    // TBSCertificate's first field is `version [0] EXPLICIT Version DEFAULT
    // v1`, and DER omits a field at its default. So it is present on a v3
    // certificate, which is everything a public CA issues, and absent on a v1
    // one. When it is absent the first element read here is already
    // serialNumber, and `rest` must go back to the start of the sequence so the
    // skip below does not eat a field it was not meant to.
    let (tag, _, after_version) = element(tbs)?;
    let mut rest = if tag == CONTEXT_0 { after_version } else { tbs };

    // serialNumber, signature, issuer. Skipped by length, which is the whole
    // reason this is cheap: an issuer Name is a nest of sets and sequences and
    // none of it has to be understood to step over it.
    for _ in 0..3 {
        rest = element(rest)?.2;
    }

    // Validity ::= SEQUENCE { notBefore Time, notAfter Time }
    let (tag, validity, _) = element(rest)?;
    if tag != SEQUENCE {
        return Err(CertificateError::NoValidity);
    }
    let after_not_before = element(validity)?.2;
    let (tag, not_after, _) = element(after_not_before)?;

    let text = std::str::from_utf8(not_after).map_err(|_| CertificateError::BadTime)?;
    // The two forms differ only in the width of the year. UTCTime's two digits
    // are resolved by RFC 5280 §4.1.2.5.1: 50 and above is 19YY, below it is
    // 20YY. That rule is the reason a certificate cannot express a date past
    // 2049 in UTCTime, and why long-dated roots use GeneralizedTime.
    let (year, rest) = match tag {
        UTC_TIME => {
            let (yy, rest) = text.split_at_checked(2).ok_or(CertificateError::BadTime)?;
            let yy: i32 = yy.parse().map_err(|_| CertificateError::BadTime)?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, rest)
        }
        GENERALIZED_TIME => {
            let (yyyy, rest) = text.split_at_checked(4).ok_or(CertificateError::BadTime)?;
            (yyyy.parse().map_err(|_| CertificateError::BadTime)?, rest)
        }
        _ => return Err(CertificateError::BadTime),
    };

    // MMDDHHMMSSZ, eleven characters. Checking the width and the `Z` up front
    // is what makes the field reads below total: a shorter string, a timezone
    // offset or a fractional second all fail here rather than being half read.
    if rest.len() != 11 || !rest.ends_with('Z') {
        return Err(CertificateError::BadTime);
    }
    let field = |from: usize, to: usize| -> Result<u32, CertificateError> {
        rest.get(from..to)
            .ok_or(CertificateError::BadTime)?
            .parse()
            .map_err(|_| CertificateError::BadTime)
    };
    // Read out before the date is built: `and_then` takes a closure returning
    // an `Option`, so a `?` on these inside it would have to throw the reason
    // away.
    let (month, day) = (field(0, 2)?, field(2, 4)?);
    let (hour, minute, second) = (field(4, 6)?, field(6, 8)?, field(8, 10)?);
    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|d| d.and_hms_opt(hour, minute, second))
        .ok_or(CertificateError::BadTime)?;
    Ok(date.and_utc().timestamp())
}

/// One DER element: its tag, its contents, and the bytes after it.
///
/// Returning the remainder as well as the contents is what lets the walk above
/// read one line per field. Every length is checked against the buffer, so a
/// truncated or hostile certificate ends as `Truncated` and never as a panic:
/// this runs at startup on an operator's own file, and a server that aborts
/// while reading the certificate it is about to serve is a worse failure than
/// the one being prevented.
fn element(buf: &[u8]) -> Result<(u8, &[u8], &[u8]), CertificateError> {
    let (&tag, after_tag) = buf.split_first().ok_or(CertificateError::Truncated)?;
    let (&first, after_first) = after_tag.split_first().ok_or(CertificateError::Truncated)?;
    let (length, body) = if first < 0x80 {
        // Short form: the byte is the length.
        (first as usize, after_first)
    } else {
        // Long form: the low seven bits count the length's own bytes. 0x80 on
        // its own is the indefinite form, which BER allows and DER forbids, and
        // a count wider than a usize cannot index this buffer. Both are refused
        // rather than truncated into something plausible.
        let count = (first & 0x7f) as usize;
        if count == 0 || count > std::mem::size_of::<usize>() {
            return Err(CertificateError::BadLength);
        }
        let (bytes, body) = after_first
            .split_at_checked(count)
            .ok_or(CertificateError::Truncated)?;
        let length = bytes.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize);
        (length, body)
    };
    let (contents, rest) = body
        .split_at_checked(length)
        .ok_or(CertificateError::Truncated)?;
    Ok((tag, contents, rest))
}

/// `notAfter` for each certificate the server has loaded, leaf first.
///
/// A `RwLock` rather than a `OnceLock` so a second load can replace the first.
/// Written once per load and read once per monitoring request, so the lock is
/// never contended.
///
/// **What this holds is the chain every protocol is serving.** A reload writes
/// it again, and `m6-http`'s `handle_tls_reload` writes it after the new
/// material is installed on both the rustls listener and the quiche
/// configuration, so a build that fails leaves the previous chain recorded and
/// still being served.
///
/// It was not always so. Until m6 #210 a reload rebuilt the quiche
/// configuration alone and re-recorded nothing, so after a renewal HTTP/3
/// served the new certificate, HTTP/1.1 and HTTP/2 served the old one, and the
/// number here was correct for the second pair and stale for the first.
/// Each entry is `(depth as loaded, notAfter)`, and an unreadable certificate
/// leaves a GAP rather than shifting everything after it.
///
/// A bare `Vec<i64>` re-derived depth from the vector position, so a chain
/// whose leaf failed to parse reported its intermediate as `depth: 0`, which
/// every doc here calls the certificate an ACME client renews. An operator
/// would read a multi-year expiry for the one that actually needed renewing.
static LOADED: RwLock<Vec<(usize, i64)>> = RwLock::new(Vec::new());

/// How many certificates in the loaded chain could not be read.
///
/// Carried separately because the list above cannot express it: a chain with
/// an unreadable member and a chain that is simply shorter look identical
/// once the failures are dropped. A monitor needs to tell "every certificate
/// reported" from "some certificate is unaccounted for", and reading the
/// second as the first is the absence-as-health failure this module exists to
/// prevent.
static UNREADABLE: RwLock<usize> = RwLock::new(0);

/// Record the chain the server has just loaded, replacing any earlier one.
///
/// Returns the certificates that could not be read, by depth and reason, and
/// records the ones that could. A certificate the parse cannot handle must not
/// stop a server serving it: the material is valid to rustls or the handshake
/// would already have failed, so the honest outcome is a reported absence that
/// the caller logs. Reporting nothing for it is also what keeps the monitor's
/// "cannot say" distinct from a measured number.
pub fn record_loaded<T: AsRef<[u8]>>(chain: &[T]) -> Vec<(usize, CertificateError)> {
    let mut not_after = Vec::with_capacity(chain.len());
    let mut failures = Vec::new();
    for (depth, der) in chain.iter().enumerate() {
        match not_after_unix(der.as_ref()) {
            // The depth is recorded, not implied by position, so a failure
            // earlier in the chain cannot renumber what follows it.
            Ok(secs) => not_after.push((depth, secs)),
            Err(e) => failures.push((depth, e)),
        }
    }
    *UNREADABLE.write().unwrap_or_else(|e| e.into_inner()) = failures.len();
    // A poisoned lock means another thread panicked while holding it. The value
    // behind it is a plain `Vec<i64>` that no panic can leave half written, so
    // the recovery is to take it anyway rather than to propagate.
    let mut slot = LOADED.write().unwrap_or_else(|e| e.into_inner());
    *slot = not_after;
    failures
}

/// The loaded chain's expiries, measured against `now_unix`.
///
/// Empty when nothing has been recorded, which is the state of a process
/// serving no TLS at all. An empty list and a list of expired certificates are
/// different answers and a reader of the report can tell them apart.
pub fn loaded_at(now_unix: i64) -> Vec<TlsCertificate> {
    let slot = LOADED.read().unwrap_or_else(|e| e.into_inner());
    slot.iter()
        .map(|&(depth, not_after_unix)| TlsCertificate {
            depth,
            not_after_unix,
            expires_in_seconds: not_after_unix - now_unix,
        })
        .collect()
}

/// How many certificates in the loaded chain could not be read.
///
/// Non-zero means the report is incomplete and a reader must say so rather
/// than judging the certificates it did get. A chain whose leaf is unreadable
/// and whose intermediate expires in two years is not a healthy chain, and
/// taking the minimum of what parsed would call it one.
pub fn unreadable() -> usize {
    *UNREADABLE.read().unwrap_or_else(|e| e.into_inner())
}

/// The loaded chain's expiries, measured against the clock now.
pub fn loaded() -> Vec<TlsCertificate> {
    loaded_at(chrono::Utc::now().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A certificate from `rcgen`, as DER, valid for `days` from now.
    ///
    /// Generated rather than checked in as a fixture: a fixture expires, and a
    /// test that starts failing on a date is a test that gets deleted.
    fn certificate(days: i64) -> (Vec<u8>, i64) {
        let mut params =
            rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("params");
        let not_after = time::OffsetDateTime::now_utc() + time::Duration::days(days);
        // Whole seconds, because that is all notAfter can carry and the
        // comparison below has to be exact rather than within a second.
        let not_after = not_after.replace_nanosecond(0).expect("whole second");
        params.not_after = not_after;
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.self_signed(&key).expect("self-signed");
        (cert.der().to_vec(), not_after.unix_timestamp())
    }

    #[test]
    fn not_after_matches_what_the_certificate_was_issued_with() {
        let (der, expected) = certificate(30);
        assert_eq!(not_after_unix(&der), Ok(expected));
    }

    /// The case the whole module exists for. An expiry already past must read
    /// as a negative number of seconds, not as zero and not as an error: "it
    /// expired" and "it expires in a moment" are the same reading otherwise.
    #[test]
    fn an_expired_certificate_reports_a_negative_remainder() {
        let (der, expected) = certificate(-5);
        let not_after = not_after_unix(&der).expect("parses");
        assert_eq!(not_after, expected);
        let now = chrono::Utc::now().timestamp();
        assert!(not_after - now < 0, "expected a negative remainder");
    }

    #[test]
    fn utc_time_resolves_two_digit_years_by_rfc_5280() {
        // notAfter on its own, as a certificate would carry it: tag 0x17,
        // length 13, then YYMMDDHHMMSSZ. Built here rather than through rcgen
        // because no certificate can be issued into 1999 to test the other
        // half of the rule.
        let utc = |text: &str| {
            let mut der = vec![0x17, text.len() as u8];
            der.extend_from_slice(text.as_bytes());
            der
        };
        // 50 and above is 19YY and 49 and below is 20YY, so the window runs
        // 1950 to 2049. A validity built by hand is the only way to reach a
        // 19xx notAfter, because no certificate can be issued into the past.
        assert_eq!(
            time_of(&utc("500101000000Z")),
            chrono::NaiveDate::from_ymd_opt(1950, 1, 1)
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|d| d.and_utc().timestamp())
                .expect("1950 is representable")
        );
        assert_eq!(
            time_of(&utc("490101000000Z")),
            chrono::NaiveDate::from_ymd_opt(2049, 1, 1)
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|d| d.and_utc().timestamp())
                .expect("2049 is representable")
        );
    }

    /// `not_after_unix` for a bare Time element, by wrapping it in the
    /// structure the walk expects. Keeps the year-window test above about the
    /// year window rather than about DER construction.
    fn time_of(time_der: &[u8]) -> i64 {
        // Validity ::= SEQUENCE { notBefore, notAfter }, with notBefore a
        // copy of notAfter: this test is about notAfter and the walk only
        // steps over the first one.
        let mut validity = Vec::new();
        validity.extend_from_slice(time_der);
        validity.extend_from_slice(time_der);
        let validity = der_sequence(&validity);

        // TBSCertificate, with the three skipped fields present and empty. A
        // zero-length SEQUENCE is a legal element to step over, which is all
        // the walk does with them.
        let mut tbs = vec![0xA0, 0x03, 0x02, 0x01, 0x02]; // version [0] v3
        tbs.extend_from_slice(&[0x02, 0x01, 0x01]); // serialNumber 1
        tbs.extend_from_slice(&der_sequence(&[])); // signature
        tbs.extend_from_slice(&der_sequence(&[])); // issuer
        tbs.extend_from_slice(&validity);
        let certificate = der_sequence(&der_sequence(&tbs));
        not_after_unix(&certificate).expect("parses")
    }

    fn der_sequence(contents: &[u8]) -> Vec<u8> {
        let mut out = vec![0x30];
        // Short form only: everything this helper wraps is well under 128
        // bytes, and the long form is covered against real certificates.
        assert!(contents.len() < 0x80, "test helper is short-form only");
        out.push(contents.len() as u8);
        out.extend_from_slice(contents);
        out
    }

    /// A v1 certificate has no `version` field, so the walk must not step over
    /// serialNumber in its place. The bug this pins reads notBefore as notAfter
    /// and reports a certificate as expired on the day it was issued.
    #[test]
    fn a_certificate_without_a_version_field_still_parses() {
        let time = {
            let text = "300101000000Z";
            let mut der = vec![0x17, text.len() as u8];
            der.extend_from_slice(text.as_bytes());
            der
        };
        let mut validity = Vec::new();
        validity.extend_from_slice(&time);
        validity.extend_from_slice(&time);
        let mut tbs = vec![0x02, 0x01, 0x01]; // serialNumber first: no version
        tbs.extend_from_slice(&der_sequence(&[])); // signature
        tbs.extend_from_slice(&der_sequence(&[])); // issuer
        tbs.extend_from_slice(&der_sequence(&validity));
        let certificate = der_sequence(&der_sequence(&tbs));
        assert_eq!(
            not_after_unix(&certificate),
            Ok(chrono::NaiveDate::from_ymd_opt(2030, 1, 1)
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|d| d.and_utc().timestamp())
                .expect("2030 is representable"))
        );
    }

    /// Every truncation of a real certificate must come back as an error. This
    /// is the test that makes the hand-written walk defensible: there is no
    /// prefix of a valid certificate that panics or reads past its buffer.
    #[test]
    fn no_truncation_of_a_certificate_panics() {
        let (der, expected) = certificate(30);
        for end in 0..der.len() {
            // The result is not asserted on, only that there is one.
            let _ = not_after_unix(&der[..end]);
        }
        assert_eq!(not_after_unix(&der), Ok(expected));
    }

    #[test]
    fn garbage_is_refused_rather_than_guessed_at() {
        assert_eq!(not_after_unix(&[]), Err(CertificateError::Truncated));
        assert_eq!(
            not_after_unix(&[0x02, 0x01, 0x01]),
            Err(CertificateError::NotACertificate)
        );
        // 0x80 is the indefinite form: legal BER, forbidden in DER.
        assert_eq!(
            not_after_unix(&[0x30, 0x80, 0x30, 0x00]),
            Err(CertificateError::BadLength)
        );
    }

    /// A time that is well formed as a string and not a date. March 32nd is
    /// the case `chrono` catches and a hand-written civil-from-days would not.
    #[test]
    fn an_impossible_date_is_an_error() {
        let text = "300332000000Z";
        let mut der = vec![0x17, text.len() as u8];
        der.extend_from_slice(text.as_bytes());
        let mut validity = Vec::new();
        validity.extend_from_slice(&der);
        validity.extend_from_slice(&der);
        let mut tbs = vec![0x02, 0x01, 0x01];
        tbs.extend_from_slice(&der_sequence(&[]));
        tbs.extend_from_slice(&der_sequence(&[]));
        tbs.extend_from_slice(&der_sequence(&validity));
        assert_eq!(
            not_after_unix(&der_sequence(&der_sequence(&tbs))),
            Err(CertificateError::BadTime)
        );
    }

    /// The registry, in one test deliberately.
    ///
    /// `LOADED` is process-wide and `cargo test` runs a crate's tests on
    /// several threads, so two tests recording different chains would race and
    /// fail on whichever ran second. One test that records in sequence is the
    /// honest way to cover shared state, and the remainder is measured against
    /// a clock passed in rather than the real one so every assertion is exact.
    #[test]
    fn the_registry_reports_the_loaded_chain() {
        // Leaf first, and an intermediate on its own much longer cycle.
        let (leaf, leaf_not_after) = certificate(30);
        let (intermediate, intermediate_not_after) = certificate(400);
        assert!(record_loaded(&[leaf.clone(), intermediate]).is_empty());

        let now = leaf_not_after - 86_400;
        let loaded = loaded_at(now);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].depth, 0);
        assert_eq!(loaded[0].not_after_unix, leaf_not_after);
        assert_eq!(loaded[0].expires_in_seconds, 86_400);
        assert_eq!(loaded[1].depth, 1);
        assert_eq!(loaded[1].not_after_unix, intermediate_not_after);
        assert_eq!(
            loaded[1].expires_in_seconds,
            intermediate_not_after - now,
            "an intermediate expires on its own schedule, not the leaf's"
        );

        // A certificate the walk cannot read comes back to the caller and stays
        // out of the registry, so a monitor sees "cannot say" for it rather
        // than a number nobody measured.
        let failures = record_loaded(&[leaf, vec![0x02, 0x01, 0x01]]);
        assert_eq!(failures, vec![(1, CertificateError::NotACertificate)]);
        let loaded = loaded_at(0);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].not_after_unix, leaf_not_after);

        // A later load replaces the earlier one rather than adding to it. This
        // is what makes the report say what is being served now.
        assert!(record_loaded::<Vec<u8>>(&[]).is_empty());
        assert!(loaded_at(0).is_empty());

        // THE DEPTH IS THE POSITION AS LOADED, so a failure earlier in the
        // chain cannot renumber what follows it. The previous test only ever
        // failed at depth 1, which structurally cannot catch this: a leaf that
        // does not parse used to report the INTERMEDIATE as depth 0, and every
        // document here calls depth 0 the certificate an ACME client renews.
        let (intermediate, intermediate_not_after) = certificate(400);
        let failures = record_loaded(&[vec![0x02, 0x01, 0x01], intermediate]);
        assert_eq!(failures.len(), 1, "the leaf is the one that failed");
        assert_eq!(failures[0].0, 0);
        let loaded = loaded_at(0);
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].depth, 1,
            "an unreadable leaf must leave a gap, never promote the intermediate to depth 0"
        );
        assert_eq!(loaded[0].not_after_unix, intermediate_not_after);
        assert_eq!(unreadable(), 1, "the unreadable leaf must be counted");

        // And the count resets with a clean load, so it describes the chain
        // in hand rather than accumulating.
        let (leaf, _) = certificate(30);
        assert!(record_loaded(&[leaf]).is_empty());
        assert_eq!(unreadable(), 0);
    }
}
