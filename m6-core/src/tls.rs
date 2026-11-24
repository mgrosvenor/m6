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
//! metrics document can report it without re-reading a file. The second
//! half matters as much as the first: a path on disk and the material a running
//! process is serving are different facts, and reading the file would report
//! the one nobody asked about. A certificate renewed on disk and never
//! restarted into the server is exactly the case an operator needs to see.
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

use std::fmt;
use std::sync::RwLock;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

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

/// A certificate's identity: SHA-256 over the DER bytes the node is serving.
///
/// **What is hashed is the whole certificate, not its key.** A renewal that
/// reissues with the same key pair changes the serial number, the validity and
/// the signature, so the fingerprint moves with all three. Hashing the
/// `SubjectPublicKeyInfo` would stay the same across that reissue, which
/// answers "is this the same key" and not "is this the same certificate": two
/// nodes can hold one key in two certificates with different validity, so a
/// fleet check built on the SPKI would call them one certificate while a
/// client sees two. Leaf plus issuer is not used either, because the issuer is
/// already inside the leaf's DER and the fingerprint is one number per
/// certificate rather than a number per chain.
///
/// SHA-256 for the reason `Cargo.toml` gives: `openssl x509 -fingerprint
/// -sha256` is the tool an operator compares this number with.
///
/// **A node's fingerprint is not proof of anything and not a secret.** Every
/// client already receives these bytes in the handshake, so a fingerprint
/// discloses nothing a visitor does not have. It says which certificate a node
/// loaded, and it cannot say that the node told the truth about it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// SHA-256 of `der`, the certificate as it is served.
    ///
    /// Called once per certificate when a chain is loaded, never per request.
    pub fn of(der: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        Self(Sha256::digest(der).into())
    }

    /// Characters in the hex form, which is what the metrics document carries.
    pub const HEX_LEN: usize = 64;

    /// The digest as lower-case hex, the form the metrics document and
    /// `openssl` print.
    pub fn to_hex(self) -> String {
        String::from_utf8(self.hex().to_vec()).expect("hex digits are ASCII")
    }

    /// The first twelve hex characters, enough to compare by eye in a report.
    pub fn short(self) -> String {
        String::from_utf8(self.hex()[..12].to_vec()).expect("hex digits are ASCII")
    }

    /// The hex form in a fixed buffer, so serialising one allocates nothing.
    fn hex(self) -> [u8; Self::HEX_LEN] {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; Self::HEX_LEN];
        for (i, byte) in self.0.iter().enumerate() {
            out[i * 2] = DIGITS[(byte >> 4) as usize];
            out[i * 2 + 1] = DIGITS[(byte & 0x0f) as usize];
        }
        out
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(std::str::from_utf8(&self.hex()).expect("hex digits are ASCII"))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl Serialize for Fingerprint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(std::str::from_utf8(&self.hex()).expect("hex digits are ASCII"))
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Hex;
        impl serde::de::Visitor<'_> for Hex {
            type Value = Fingerprint;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("64 hex characters")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Fingerprint, E> {
                let bytes = v.as_bytes();
                if bytes.len() != Fingerprint::HEX_LEN {
                    return Err(E::invalid_length(bytes.len(), &self));
                }
                let bad = || E::invalid_value(serde::de::Unexpected::Str(v), &"64 hex characters");
                let mut out = [0u8; 32];
                for (i, byte) in out.iter_mut().enumerate() {
                    let hi = hex_digit(bytes[i * 2]).ok_or_else(bad)?;
                    let lo = hex_digit(bytes[i * 2 + 1]).ok_or_else(bad)?;
                    *byte = (hi << 4) | lo;
                }
                Ok(Fingerprint(out))
            }
        }
        deserializer.deserialize_str(Hex)
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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
    /// SHA-256 of this certificate's DER, the identity [`Fingerprint`] carries.
    ///
    /// The expiry above cannot tell two certificates apart when they expire on
    /// the same day, which is the ordinary shape of a renewal that reissues
    /// twelve hours later or of two ACME clients renewing on one schedule.
    /// This can. `serde(default)` is what makes a node older than this field
    /// still parse: its certificates arrive with no fingerprint, which a
    /// reader must treat as "cannot say" and never as "the same as the others".
    #[serde(default)]
    pub fingerprint: Option<Fingerprint>,
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

/// `notAfter` and the fingerprint for each certificate the server has loaded,
/// leaf first.
///
/// Written once at start, when the configuration is installed, and read once
/// per monitoring request. The `RwLock` keeps the write and read safe.
///
/// **What this holds is the chain every protocol is serving.**
///
/// Each entry is `(depth as loaded, notAfter, fingerprint)`, and an unreadable
/// certificate leaves a GAP rather than shifting everything after it. The
/// fingerprint is computed in the same pass, from the same bytes, so a
/// certificate's identity and its expiry cannot describe two different loads.
///
/// A bare `Vec<i64>` re-derived depth from the vector position, so a chain
/// whose leaf failed to parse reported its intermediate as `depth: 0`, which
/// every doc here calls the certificate an ACME client renews. An operator
/// would read a multi-year expiry for the one that actually needed renewing.
static LOADED: RwLock<Vec<(usize, i64, Fingerprint)>> = RwLock::new(Vec::new());

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
    let mut loaded = Vec::with_capacity(chain.len());
    let mut failures = Vec::new();
    for (depth, der) in chain.iter().enumerate() {
        let der = der.as_ref();
        match not_after_unix(der) {
            // The depth is recorded, not implied by position, so a failure
            // earlier in the chain cannot renumber what follows it. The
            // fingerprint is taken from the same bytes in the same pass, once
            // per load: nothing per request hashes anything.
            Ok(secs) => loaded.push((depth, secs, Fingerprint::of(der))),
            Err(e) => failures.push((depth, e)),
        }
    }
    *UNREADABLE.write().unwrap_or_else(|e| e.into_inner()) = failures.len();
    // A poisoned lock means another thread panicked while holding it. The value
    // behind it is a plain vector that no panic can leave half written, so the
    // recovery is to take it anyway rather than to propagate.
    let mut slot = LOADED.write().unwrap_or_else(|e| e.into_inner());
    *slot = loaded;
    failures
}

/// The loaded chain's expiries and identities, measured against `now_unix`.
///
/// Empty when nothing has been recorded, which is the state of a process
/// serving no TLS at all. An empty list and a list of expired certificates are
/// different answers and a reader of the report can tell them apart.
pub fn loaded_at(now_unix: i64) -> Vec<TlsCertificate> {
    let slot = LOADED.read().unwrap_or_else(|e| e.into_inner());
    slot.iter()
        .map(|&(depth, not_after_unix, fingerprint)| TlsCertificate {
            depth,
            not_after_unix,
            expires_in_seconds: not_after_unix - now_unix,
            fingerprint: Some(fingerprint),
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

/// The loaded chain's expiries and identities, measured against the clock now.
pub fn loaded() -> Vec<TlsCertificate> {
    let now: chrono::DateTime<chrono::Utc> = crate::platform::wall().into();
    loaded_at(now.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A certificate from `rcgen`, as DER, valid for `days` from now.
    ///
    /// Generated rather than checked in as a fixture: a fixture expires, and a
    /// test that starts failing on a date is a test that gets deleted.
    fn certificate(days: i64) -> (Vec<u8>, i64) {
        let now: chrono::DateTime<chrono::Utc> = crate::platform::wall().into();
        let now = time::OffsetDateTime::from_unix_timestamp(now.timestamp())
            .expect("a wall clock reading is a valid timestamp");
        let not_after = now + time::Duration::days(days);
        // Whole seconds, because that is all notAfter can carry and the
        // comparison below has to be exact rather than within a second.
        let not_after = not_after.replace_nanosecond(0).expect("whole second");
        (
            certificate_with_not_after(not_after),
            not_after.unix_timestamp(),
        )
    }

    /// A self-signed certificate for `localhost` with the expiry given.
    ///
    /// Separate from `certificate` so a test can issue two certificates into
    /// the same second on purpose, which is the case a fingerprint has to
    /// separate and a clock-relative helper cannot guarantee.
    fn certificate_with_not_after(not_after: time::OffsetDateTime) -> Vec<u8> {
        let mut params =
            rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("params");
        params.not_after = not_after;
        let key = rcgen::KeyPair::generate().expect("key");
        params
            .self_signed(&key)
            .expect("self-signed")
            .der()
            .to_vec()
    }

    #[test]
    fn not_after_matches_what_the_certificate_was_issued_with() {
        let (der, expected) = certificate(30);
        assert_eq!(not_after_unix(&der), Ok(expected));
    }

    /// The wire value is SHA-256, in lower-case hex, of the DER bytes. Pinned
    /// against the published digests of two known inputs rather than against
    /// this code's own output, so a change of algorithm or encoding fails here.
    #[test]
    fn the_fingerprint_is_the_published_sha256_of_the_bytes() {
        assert_eq!(
            Fingerprint::of(b"").to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            Fingerprint::of(b"abc").to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // The short form a report prints is the same number, cut.
        assert_eq!(Fingerprint::of(b"abc").short(), "ba7816bf8f01");
    }

    /// The value makes the round trip that the metrics document and the
    /// monitor need: one hex string on the wire, the same bytes on the other
    /// side, and a node older than the field still parses.
    #[test]
    fn a_fingerprint_survives_the_wire_and_an_old_node_still_parses() {
        let fp = Fingerprint::of(b"a certificate");
        let json = serde_json::to_string(&fp).expect("serialisable");
        assert_eq!(json, format!("\"{fp}\""));
        let back: Fingerprint = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, fp);
        assert!(serde_json::from_str::<Fingerprint>("\"not-hex\"").is_err());

        let old = r#"{"depth":0,"not_after_unix":1,"expires_in_seconds":1}"#;
        let cert: TlsCertificate = serde_json::from_str(old).expect("an old node parses");
        assert_eq!(
            cert.fingerprint, None,
            "no fingerprint is 'cannot say', which is not the same as agreement"
        );
    }

    /// **The case the fingerprint exists for.** Two certificates issued into
    /// one second expire on the same day and are different certificates. The
    /// expiry cannot tell them apart and the fingerprint can.
    #[test]
    fn two_certificates_with_one_expiry_have_different_fingerprints() {
        let at = time::OffsetDateTime::from_unix_timestamp(1_789_000_000).expect("a date");
        let a = certificate_with_not_after(at);
        let b = certificate_with_not_after(at);
        assert_ne!(a, b, "two issued certificates are two DERs");
        assert_eq!(
            not_after_unix(&a),
            not_after_unix(&b),
            "the premise of the test is one expiry"
        );
        assert_ne!(
            Fingerprint::of(&a),
            Fingerprint::of(&b),
            "one expiry and one fingerprint would make the two indistinguishable"
        );
    }

    /// The case the whole module exists for. An expiry already past must read
    /// as a negative number of seconds, not as zero and not as an error: "it
    /// expired" and "it expires in a moment" are the same reading otherwise.
    #[test]
    fn an_expired_certificate_reports_a_negative_remainder() {
        let (der, expected) = certificate(-5);
        let not_after = not_after_unix(&der).expect("parses");
        assert_eq!(not_after, expected);
        let now: chrono::DateTime<chrono::Utc> = crate::platform::wall().into();
        let now = now.timestamp();
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
        let leaf_fingerprint = Fingerprint::of(&leaf);
        assert!(record_loaded(&[leaf.clone(), intermediate]).is_empty());

        let now = leaf_not_after - 86_400;
        let loaded = loaded_at(now);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].depth, 0);
        assert_eq!(loaded[0].not_after_unix, leaf_not_after);
        assert_eq!(loaded[0].expires_in_seconds, 86_400);
        assert_eq!(
            loaded[0].fingerprint,
            Some(leaf_fingerprint),
            "the leaf's identity is the SHA-256 of the DER that was loaded"
        );
        // Read twice, as two builds of the metrics document do: the value is a
        // property of the loaded certificate, so nothing about the clock or
        // the request can move it.
        assert_eq!(
            loaded_at(now + 3_600)[0].fingerprint,
            loaded[0].fingerprint,
            "two reads of one load must agree"
        );
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
