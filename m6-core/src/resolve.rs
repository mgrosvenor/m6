//! Reverse DNS, bounded, for checking a claim against its source.
//!
//! A `User-Agent` is an assertion. Any client can send any string, so a
//! request claiming to be a well-known crawler is evidence of nothing until
//! the address it came from is checked. Every major crawler operator publishes
//! the same verification method: resolve the address to a name, and see
//! whether the name belongs to who the string claims.
//!
//! # Three outcomes, and they are not two
//!
//! [`Ptr`] distinguishes a name that was found from a lookup that found
//! nothing and from a lookup that could not be completed. Collapsing the last
//! two into "not verified" would be a small lie in the expensive direction: an
//! address with no PTR record is unverified and may be perfectly honest, while
//! a resolver that timed out says nothing about the address at all.
//!
//! # Why this is here and not in the serving path
//!
//! **`m6-http` must never do this.** A monitoring endpoint that performs a
//! network round trip is a monitoring endpoint that learns to hang, which is
//! why `/perf` reports URL backends by presence and never by reachability. A
//! monitor is the opposite case: it already makes network calls with a
//! deadline to poll every node, and going to look is the whole of its job.
//!
//! It lives in core because core is the only crate a service links.
//!
//! # Why a thread and a deadline
//!
//! `getnameinfo(3)` is blocking with no timeout parameter, and the resolver it
//! consults may be slow, unreachable, or controlled by the same person as the
//! address being looked up. An unbounded call in a monitoring run is a hostile
//! address away from stalling the run it was meant to report. So the call runs
//! on a worker thread and the caller waits with a deadline. A thread still
//! blocked at the deadline is abandoned rather than joined: it holds nothing
//! the caller needs, and the alternative is the stall this exists to prevent.
//!
//! No new dependency. `libc` is already in core.

use std::sync::mpsc;
use std::time::Duration;

/// The result of asking what name an address has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ptr {
    /// A name was returned.
    Name(String),
    /// The lookup completed and the address has no PTR record. Unverified,
    /// which is not the same as false.
    None,
    /// The lookup did not complete in time, or the resolver refused. Says
    /// nothing about the address.
    Failed,
}

impl Ptr {
    /// The name, if there is one.
    pub fn name(&self) -> Option<&str> {
        match self {
            Ptr::Name(n) => Some(n),
            _ => None,
        }
    }

    /// Whether this name supports a claim to be `domain`.
    ///
    /// A suffix match on a dot boundary, so `bot.example.com` matches
    /// `example.com` and `example.com.attacker.net` does not. The trailing dot
    /// a resolver may return is ignored. Case-insensitive, because DNS is.
    ///
    /// This is the check every crawler operator documents, and the dot
    /// boundary is the whole of it: `notexample.com` ends with `example.com`
    /// as a string and is a different domain.
    pub fn verifies(&self, domain: &str) -> bool {
        let Ptr::Name(name) = self else {
            return false;
        };
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        if domain.is_empty() {
            return false;
        }
        name == domain || name.ends_with(&format!(".{domain}"))
    }
}

/// Default deadline for one lookup.
///
/// Short on purpose. This runs once per distinct address in a monitoring run,
/// and a resolver that cannot answer in two seconds is one whose answer is not
/// worth delaying a report for. The outcome of waiting longer is the same
/// report, later.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// Look up the PTR name for an address, giving up after `timeout`.
///
/// `addr` is parsed rather than passed through: `getnameinfo` is being asked
/// about a numeric address, so anything that is not one is a caller error and
/// is reported as [`Ptr::Failed`] rather than sent to a resolver.
pub fn reverse(addr: &str, timeout: Duration) -> Ptr {
    let Ok(ip) = addr.parse::<std::net::IpAddr>() else {
        return Ptr::Failed;
    };
    let (tx, rx) = mpsc::channel();
    // Detached deliberately: see the module header. The handle is dropped, so
    // a thread still inside the resolver at the deadline finishes on its own
    // and its send goes nowhere.
    std::thread::spawn(move || {
        let _ = tx.send(blocking_reverse(ip));
    });
    rx.recv_timeout(timeout).unwrap_or(Ptr::Failed)
}

/// Look up the PTR name for an address with the default deadline.
pub fn reverse_default(addr: &str) -> Ptr {
    reverse(addr, DEFAULT_TIMEOUT)
}

/// The blocking call itself.
///
/// The sockaddr is a stack local in each arm and `getnameinfo` is called while
/// it is still in scope, so there is no allocation to free and no pointer that
/// outlives what it points at. An earlier shape built the sockaddr, returned a
/// pointer to it, and called afterwards, which is exactly how FFI comes to
/// read freed memory.
fn blocking_reverse(ip: std::net::IpAddr) -> Ptr {
    use std::net::IpAddr;

    // NI_MAXHOST.
    let mut host = [0 as libc::c_char; 1025];
    // NI_NAMEREQD makes "no PTR record" an error rather than the numeric
    // address echoed back, which is the distinction this module exists for.
    // NI_NUMERICSERV stops the service half being looked up at all.
    let flags = libc::NI_NAMEREQD | libc::NI_NUMERICSERV;

    let rc = match ip {
        IpAddr::V4(v4) => {
            let mut raw: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            raw.sin_family = libc::AF_INET as libc::sa_family_t;
            // `octets()` is network order and `s_addr` is stored in network
            // order, so the bytes go across unchanged.
            raw.sin_addr.s_addr = u32::from_ne_bytes(v4.octets());
            // SAFETY: `raw` is a fully initialised sockaddr_in alive for this
            // call, its exact size is passed as salen, and `host` is bounded
            // by its own length.
            unsafe {
                libc::getnameinfo(
                    &raw as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    host.as_mut_ptr(),
                    host.len() as libc::socklen_t,
                    std::ptr::null_mut(),
                    0,
                    flags,
                )
            }
        }
        IpAddr::V6(v6) => {
            let mut raw: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            raw.sin6_addr.s6_addr = v6.octets();
            // SAFETY: as above, for sockaddr_in6.
            unsafe {
                libc::getnameinfo(
                    &raw as *const libc::sockaddr_in6 as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    host.as_mut_ptr(),
                    host.len() as libc::socklen_t,
                    std::ptr::null_mut(),
                    0,
                    flags,
                )
            }
        }
    };

    if rc != 0 {
        // EAI_NONAME is the expected "no PTR record" outcome under
        // NI_NAMEREQD. Everything else is a resolver that could not answer,
        // and the two are reported differently on purpose.
        return if rc == libc::EAI_NONAME {
            Ptr::None
        } else {
            Ptr::Failed
        };
    }

    // SAFETY: getnameinfo returned 0, so `host` holds a NUL-terminated string
    // within its own length.
    let name = unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) };
    match name.to_str() {
        Ok(s) if !s.is_empty() => Ptr::Name(s.to_string()),
        _ => Ptr::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dot boundary is the whole of the check, so it gets the most tests.
    /// A suffix match without it accepts `example.com.attacker.net`, which is
    /// a domain the attacker controls and which ends with the string being
    /// looked for.
    #[test]
    fn verification_matches_on_a_dot_boundary_only() {
        let n = |s: &str| Ptr::Name(s.to_string());
        assert!(n("crawl-1-2-3.example.com").verifies("example.com"));
        assert!(n("example.com").verifies("example.com"));
        // The trailing dot a resolver may return.
        assert!(n("crawl.example.com.").verifies("example.com"));
        // DNS is case-insensitive.
        assert!(n("Crawl.Example.COM").verifies("example.com"));

        // The attack this exists to refuse.
        assert!(!n("example.com.attacker.net").verifies("example.com"));
        // A different domain that happens to end with the same letters.
        assert!(!n("notexample.com").verifies("example.com"));
        // A subdomain match must not run the other way.
        assert!(!n("example.com").verifies("crawl.example.com"));
    }

    /// An absent name and a failed lookup both fail verification, and neither
    /// is reported as the other.
    #[test]
    fn an_absent_name_and_a_failed_lookup_are_different_and_neither_verifies() {
        assert!(!Ptr::None.verifies("example.com"));
        assert!(!Ptr::Failed.verifies("example.com"));
        assert_ne!(Ptr::None, Ptr::Failed);
        assert_eq!(Ptr::None.name(), None);
        assert_eq!(Ptr::Failed.name(), None);
        assert_eq!(
            Ptr::Name("a.example.com".into()).name(),
            Some("a.example.com")
        );
    }

    /// An empty domain must never verify. A sighting whose expected domain is
    /// unknown would otherwise be reported as confirmed by any name at all.
    #[test]
    fn an_empty_domain_never_verifies() {
        assert!(!Ptr::Name("anything.example.com".into()).verifies(""));
        assert!(!Ptr::Name("anything.example.com".into()).verifies("."));
    }

    /// Anything that is not a numeric address is a caller error and never
    /// reaches a resolver.
    #[test]
    fn a_non_address_is_not_looked_up() {
        assert_eq!(reverse("not-an-address", DEFAULT_TIMEOUT), Ptr::Failed);
        assert_eq!(reverse("", DEFAULT_TIMEOUT), Ptr::Failed);
        assert_eq!(reverse("example.com", DEFAULT_TIMEOUT), Ptr::Failed);
    }

    /// The deadline is honoured rather than advisory. A zero timeout cannot
    /// wait for any resolver, so it must come back immediately and say it
    /// could not answer, which is the behaviour that keeps a hostile address
    /// from stalling a monitoring run.
    #[test]
    fn a_deadline_that_cannot_be_met_reports_failure_rather_than_waiting() {
        let started = std::time::Instant::now();
        let got = reverse("192.0.2.1", Duration::from_millis(0));
        assert_eq!(got, Ptr::Failed);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a zero deadline must not block"
        );
    }

    /// Loopback resolves on essentially every system, and the address is
    /// local so the test needs no network. Any of the three outcomes is
    /// acceptable because a machine may have no resolver at all; what is
    /// asserted is that the call returns, bounded, without panicking, for
    /// both address families.
    #[test]
    fn a_real_lookup_returns_within_its_deadline() {
        for addr in ["127.0.0.1", "::1"] {
            let started = std::time::Instant::now();
            let got = reverse(addr, Duration::from_secs(3));
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{addr} exceeded its deadline"
            );
            if let Ptr::Name(n) = &got {
                assert!(!n.is_empty(), "{addr} returned an empty name");
            }
        }
    }
}
