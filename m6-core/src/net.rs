//! What counts as a private address.
//!
//! One definition of "is this our own network". A service can use it to decide whether a
//! peer address belongs to its own network. A check on a peer address only
//! means something on a socket that faces the client directly; behind a proxy
//! the peer is the proxy.
//!
//! # What is private
//!
//! | family | ranges |
//! |---|---|
//! | IPv4 | `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`, `100.64.0.0/10` |
//! | IPv6 | `::1`, `fc00::/7`, `fe80::/10` |
//!
//! An IPv4-mapped IPv6 address (`::ffff:10.0.0.1`) is classified by its IPv4
//! form. The unspecified addresses (`0.0.0.0`, `::`) are not private: a socket
//! bound to one listens on every interface, public ones included.
//!
//! The predicates never resolve a name. [`bind_ip`] returns `None` for a host
//! name, and a host name is therefore never private. [`resolve`] is the one
//! lookup in this module, and it is for start only.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::time::Instant;

fn v4_is_private(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    // 100.64.0.0/10 (carrier-grade NAT, and the usual tunnel backbone range).
    // `Ipv4Addr::is_shared` would say this but is not stable.
    let shared = o[0] == 100 && (o[1] & 0xC0) == 64;
    ip.is_loopback() || ip.is_private() || ip.is_link_local() || shared
}

/// True when `ip` is on a private network. See the module doc for the ranges.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_private(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_is_private(v4);
            }
            v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
        }
    }
}

/// The IP address in a bind string, if it holds one.
///
/// Accepts `ip:port`, `[v6]:port` and a bare `ip` (with or without brackets).
/// A host name gives `None`; nothing here resolves a name.
pub fn bind_ip(bind: &str) -> Option<IpAddr> {
    if let Ok(sa) = bind.parse::<SocketAddr>() {
        return Some(sa.ip());
    }
    literal_ip(bind)
}

/// The address `host:port` names, resolved once at start.
///
/// An IP literal (with or without brackets) returns at once, so a config that
/// names addresses needs no lookup. A name goes to the system resolver, which
/// blocks: this belongs in the start phase, never in a loop. The first IPv4
/// address wins over a later IPv6 one, because a host whose IPv6 route does not
/// work would otherwise fail on the first connect. A name that does not resolve
/// is an error naming it, so a service can refuse to start. An address that
/// changes later needs a restart.
pub fn resolve(host: &str, port: u16) -> io::Result<SocketAddr> {
    if let Some(ip) = literal_ip(host) {
        return Ok(SocketAddr::new(ip, port));
    }
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| io::Error::new(e.kind(), format!("{host}: {e}")))?;
    let mut first = None;
    for addr in addrs {
        if addr.is_ipv4() {
            return Ok(addr);
        }
        first.get_or_insert(addr);
    }
    first.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{host} resolved to no address"),
        )
    })
}

/// The IP address in `host`, with or without brackets, or `None` for a name.
fn literal_ip(host: &str) -> Option<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip);
    }
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .and_then(|s| s.parse::<IpAddr>().ok())
}

// ── Listeners ───────────────────────────────────────────────────────────────

/// A non-blocking unix listener. Start only. [`crate::platform::net::bind_unix`]
/// under the name every service calls: a stale socket file is replaced, the
/// parent directory is created, `mode` is set, and a path over the platform's
/// limit fails here with the path and the limit in the message.
pub fn listen_unix(
    path: &std::path::Path,
    mode: u32,
) -> std::io::Result<crate::platform::net::UnixListener> {
    crate::platform::net::bind_unix(path, mode)
}

/// A non-blocking TCP listener with `SO_REUSEADDR`. Start only.
/// [`crate::platform::net::listen_tcp`] under the name every service calls.
pub fn listen_tcp(addr: SocketAddr) -> std::io::Result<crate::platform::net::TcpListener> {
    crate::platform::net::listen_tcp(addr)
}

// ── Descriptor pressure ─────────────────────────────────────────────────────

/// The message an accept loop writes when it refuses a connection for want of
/// a file descriptor. One wording, so one search finds every service.
pub const REFUSED_MESSAGE: &str = "connection rejected: no free file descriptors";

/// How often a refusal burst may write a line: no more than one a second while
/// the pressure lasts.
pub const REFUSED_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// What one [`REFUSED_MESSAGE`] line reports.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RefusedCounts {
    /// Refusals since the previous line. Exact, so the counts in two lines add
    /// up to what happened between them.
    pub since_last: u64,
    /// Refusals since the process started.
    pub total: u64,
}

/// The refusal count and the rate limit an accept loop keeps beside its
/// listener.
///
/// Every refusal is counted. A line carries the count since the previous line,
/// so rate limiting delays a count and never loses one. [`Self::due`] asks for
/// a line at the end of a turn that refused and wrote nothing it could serve,
/// which is one line a burst and no more than one a second while the pressure
/// lasts. [`Self::flush`] asks for the last line of a burst, at the first
/// connection the loop serves again; a flush inside the interval does not
/// write, and it does not reset the interval either, so a pass that both
/// refused and served cannot turn a burst into a line per accepted
/// connection.
#[derive(Debug)]
pub struct Refusals {
    last: Option<Instant>,
    owed: u64,
    total: u64,
}

impl Default for Refusals {
    fn default() -> Self {
        Self::new()
    }
}

impl Refusals {
    pub const fn new() -> Self {
        Refusals {
            last: None,
            owed: 0,
            total: 0,
        }
    }

    /// Count one refused connection.
    pub fn refused(&mut self) {
        self.owed += 1;
        self.total += 1;
    }

    /// Refusals not yet written in a line.
    pub fn owed(&self) -> u64 {
        self.owed
    }

    /// Refusals since start.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// The counts a line should carry now, or `None` while the rate limit holds
    /// the line back or nothing is owed.
    pub fn due(&mut self, now: Instant) -> Option<RefusedCounts> {
        if self.owed == 0 {
            return None;
        }
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < REFUSED_INTERVAL)
        {
            return None;
        }
        Some(self.write(now))
    }

    /// The counts the last line of a burst should carry now, or `None` when
    /// every refusal has been reported or the interval has not turned.
    ///
    /// Flushing reports the count; it does not reset the interval. `last` is
    /// when a line was last written, so a pass that both served and refused
    /// cannot write a line per accepted connection: if the last line is inside
    /// the interval, this returns `None` and the count stays owed for the next
    /// turn that asks.
    pub fn flush(&mut self, now: Instant) -> Option<RefusedCounts> {
        if self.owed == 0 {
            return None;
        }
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < REFUSED_INTERVAL)
        {
            return None;
        }
        Some(self.write(now))
    }

    /// The count the process owes when it is leaving, or `None` when nothing
    /// is owed.
    ///
    /// The interval does not apply. A process that is exiting has no later turn
    /// to carry a count the rate limit held back, so the last line reports
    /// whatever is owed.
    pub fn closing(&mut self, now: Instant) -> Option<RefusedCounts> {
        if self.owed == 0 {
            return None;
        }
        Some(self.write(now))
    }

    fn write(&mut self, now: Instant) -> RefusedCounts {
        let counts = RefusedCounts {
            since_last: self.owed,
            total: self.total,
        };
        self.owed = 0;
        self.last = Some(now);
        counts
    }
}

/// Write the one line a refused connection gets, at `warn`, naming the service
/// that refused and the counts the caller's [`Refusals`] reports.
pub fn warn_refused(service: &str, counts: RefusedCounts) {
    crate::log::warn!(
        target: "m6_core::net",
        service,
        rejected = counts.since_last,
        total = counts.total,
        "{}",
        REFUSED_MESSAGE
    );
}

#[cfg(test)]
mod listener_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stale socket from a previous process is replaced, the mode is the one
    /// asked for, and a missing parent directory is created.
    #[test]
    fn listen_unix_replaces_a_stale_socket_and_sets_the_mode() {
        let dir = tempfile::tempdir_in(crate::platform::socket_scratch_root()).unwrap();
        let path = dir.path().join("nested").join("m6.sock");

        // The parent does not exist yet: binding must create it.
        let first = listen_unix(&path, 0o660).expect("first bind");

        // Leave a stale socket file behind the way a killed process does, and
        // bind again: the file must be replaced rather than refused.
        drop(first);
        assert!(path.exists(), "the socket file must be left behind");
        let listener = listen_unix(&path, 0o660).expect("a stale socket is replaced");
        drop(listener);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o660);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rate limit writes one line a second and carries every count: a
    /// burst inside one interval writes one line, and the refusals it held back
    /// ride out on the next one.
    ///
    /// Break: a line per refusal, or a line that reports one refusal when
    /// several happened since the last one.
    #[test]
    fn refusals_write_once_an_interval_and_carry_every_count() {
        let mut refusals = Refusals::new();
        let start = crate::platform::now();
        // Three refusals in one burst, the last listener event of each of three
        // passes, all inside the interval.
        for _ in 0..3 {
            refusals.refused();
        }
        assert_eq!(
            refusals.due(start),
            Some(RefusedCounts {
                since_last: 3,
                total: 3
            })
        );
        assert_eq!(
            refusals.due(start + std::time::Duration::from_millis(1)),
            None
        );
        assert_eq!(refusals.total(), 3);

        // Two more, still inside the interval: held back, not dropped.
        refusals.refused();
        refusals.refused();
        assert_eq!(refusals.due(start + REFUSED_INTERVAL / 2), None);
        assert_eq!(refusals.owed(), 2);

        // The interval turns, and the line carries both.
        assert_eq!(
            refusals.due(start + REFUSED_INTERVAL),
            Some(RefusedCounts {
                since_last: 2,
                total: 5
            })
        );
        // Nothing owed: no line, however long the loop waits.
        assert_eq!(refusals.due(start + REFUSED_INTERVAL * 4), None);

        // A burst that ends after the interval flushes its own count, so the
        // last refusals of a shortage are never left uncounted.
        refusals.refused();
        assert_eq!(
            refusals.flush(start + REFUSED_INTERVAL * 4),
            Some(RefusedCounts {
                since_last: 1,
                total: 6
            })
        );
        assert_eq!(refusals.flush(start + REFUSED_INTERVAL * 4), None);
        assert_eq!(refusals.total(), 6);
    }

    /// A flush inside the interval writes nothing and leaves the owed count
    /// where it was. The wrong outcome: an accept that succeeds after a refusal
    /// ends the interval, so a loop alternating refuse and serve writes a line
    /// per accepted connection.
    #[test]
    fn a_flush_does_not_defeat_the_interval() {
        let mut refusals = Refusals::new();
        let start = crate::platform::now();
        refusals.refused();
        assert_eq!(
            refusals.due(start),
            Some(RefusedCounts {
                since_last: 1,
                total: 1
            })
        );

        // Pressure eases and one connection is served: the loop flushes. The
        // interval has not turned, so no line and nothing is lost.
        refusals.refused();
        assert_eq!(refusals.flush(start + REFUSED_INTERVAL / 4), None);
        assert_eq!(refusals.owed(), 1);
        assert_eq!(refusals.due(start + REFUSED_INTERVAL / 2), None);

        // A refusal one microsecond before the interval still waits.
        refusals.refused();
        assert_eq!(
            refusals.flush(start + REFUSED_INTERVAL - std::time::Duration::from_micros(1)),
            None
        );
        assert_eq!(refusals.owed(), 2);

        // At the interval, one line carries both, and the counts add up.
        assert_eq!(
            refusals.flush(start + REFUSED_INTERVAL),
            Some(RefusedCounts {
                since_last: 2,
                total: 3
            })
        );
        assert_eq!(refusals.total(), 3);
    }

    /// A count the rate limit is holding when the process leaves is written on
    /// the way out. The wrong outcome: the refusals of the last interval are
    /// never reported.
    #[test]
    fn a_count_owed_at_exit_is_written() {
        let mut refusals = Refusals::new();
        let start = crate::platform::now();
        refusals.refused();
        assert_eq!(
            refusals.due(start),
            Some(RefusedCounts {
                since_last: 1,
                total: 1
            })
        );

        // A second refusal inside the interval is owed, and the loop's own
        // flush will not write it.
        refusals.refused();
        assert_eq!(refusals.flush(start + REFUSED_INTERVAL / 2), None);
        assert_eq!(refusals.owed(), 1);

        // Leaving writes it anyway, with the interval still open.
        assert_eq!(
            refusals.closing(start + REFUSED_INTERVAL / 2),
            Some(RefusedCounts {
                since_last: 1,
                total: 2
            })
        );
        assert_eq!(refusals.owed(), 0);
        // Nothing owed is no empty line.
        assert_eq!(refusals.closing(start + REFUSED_INTERVAL / 2), None);
    }

    fn private(s: &str) -> bool {
        is_private_ip(s.parse().unwrap())
    }

    #[test]
    fn private_v4_ranges_are_private() {
        for ip in [
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "127.0.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "100.127.255.255",
        ] {
            assert!(private(ip), "{ip}");
        }
    }

    #[test]
    fn public_v4_is_not_private() {
        for ip in [
            "172.15.255.255",
            "172.32.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "11.0.0.1",
            "192.0.2.10",
            "203.0.113.7",
            "0.0.0.0",
            "255.255.255.255",
        ] {
            assert!(!private(ip), "{ip}");
        }
    }

    #[test]
    fn private_v6_ranges_are_private() {
        for ip in ["::1", "fd00::1", "fc00::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(private(ip), "{ip}");
        }
    }

    #[test]
    fn public_v6_is_not_private() {
        for ip in ["2001:db8::1", "::", "::ffff:203.0.113.7"] {
            assert!(!private(ip), "{ip}");
        }
    }

    #[test]
    fn bind_ip_reads_literals_and_never_resolves() {
        assert!(bind_ip("10.0.0.1:80").is_some());
        assert!(bind_ip("[fd00::1]:80").is_some());
        assert!(bind_ip("10.0.0.1").is_some());
        assert!(bind_ip("localhost:80").is_none());
        assert!(bind_ip("fdexample.com:80").is_none());
        let any = bind_ip("0.0.0.0:443").expect("a literal");
        assert!(!is_private_ip(any));
    }

    #[test]
    fn a_literal_needs_no_lookup() {
        assert_eq!(
            resolve("192.0.2.1", 8080).expect("an IPv4 literal"),
            "192.0.2.1:8080"
                .parse::<SocketAddr>()
                .expect("the same address")
        );
        assert_eq!(
            resolve("[2001:db8::1]", 443).expect("a bracketed IPv6 literal"),
            "[2001:db8::1]:443"
                .parse::<SocketAddr>()
                .expect("the same address")
        );
    }

    #[test]
    fn localhost_resolves() {
        let addr = resolve("localhost", 80).expect("localhost resolves");
        assert_eq!(addr.port(), 80);
        assert!(addr.ip().is_loopback(), "{addr}");
    }

    #[test]
    fn a_name_that_does_not_exist_is_an_error() {
        // `.invalid` is reserved by RFC 6761, so no resolver can hand out an
        // address for it.
        let err = resolve("does-not-exist.invalid", 80).expect_err("must not resolve");
        assert!(
            err.to_string().contains("does-not-exist.invalid"),
            "the error must name the host: {err}"
        );
    }
}
