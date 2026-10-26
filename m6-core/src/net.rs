//! What counts as a private address.
//!
//! One definition, so every part of m6 that has to decide "is this our own
//! network" decides it the same way. m6-http uses it to classify a listener by
//! its bind address (and so to decide whether to trust forwarded-address
//! headers on it), and a service can use it to decide whether a peer address
//! belongs to its own network.
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
//! Names are never resolved. [`bind_ip`] returns `None` for a host name, and a
//! host name is therefore never private.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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
    if let Ok(ip) = bind.parse::<IpAddr>() {
        return Some(ip);
    }
    bind.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .and_then(|s| s.parse::<IpAddr>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
