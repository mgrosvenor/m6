//! The WireGuard tunnel from this host to each node.
//!
//! Two figures per node, taken from the monitor host:
//!
//! - the round trip to the node's tunnel address, as timed TCP connects;
//! - the age of the peer's latest WireGuard handshake, from a file a
//!   privileged timer writes (see `m6_core::wireguard`).
//!
//! # Why a TCP connect and not ICMP
//!
//! A raw ICMP socket needs `CAP_NET_RAW`, which the monitor's unit does not
//! hold. A connect needs only the address families it already has, adds no
//! request and no TLS handshake sample on the peer (it connects and closes),
//! and a reset from a closed port is a round trip too.
//!
//! # Which signal is fast
//!
//! The probe is the fast signal and the handshake age is the slow one. The
//! latest handshake is the last SUCCESSFUL one, so after a key changes under a
//! tunnel the time stops moving and still looks recent for minutes. An age
//! check alone passed on a tunnel that had just died, which is why the probe
//! exists beside it.
//!
//! # What this does not cover
//!
//! Cache-to-origin links are not probed from here. A cache node that cannot
//! reach its origin already shows it as backend errors on that node's `/perf`,
//! and the existing backend-error finding reports them.
//!
//! All of this runs on the background poller, after the HTTPS polls, so the
//! monitor's own TLS work does not distort the round trip. Nothing runs in a
//! request handler.

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use m6_core::wireguard::Handshakes;

/// Connects per poll.
///
/// The first SYN after a WireGuard session has lapsed also carries a WireGuard
/// handshake, so one sample mixes two costs. Three samples let the minimum show
/// the path itself, and the count is printed beside the figure.
pub const PROBES_PER_POLL: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// The peer accepted the connection.
    Connected,
    /// The peer reset the connection. The path works.
    Refused,
    /// Nothing came back within the timeout.
    NoAnswer(String),
    /// Any other error.
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub samples: Vec<Duration>,
    pub outcome: Probe,
}

/// Time up to `PROBES_PER_POLL` connects, one after another.
///
/// Stops at the first timeout or error, so a dead tunnel costs one timeout.
/// The stream is dropped at once and nothing is sent.
pub fn probe(addr: SocketAddr, timeout: Duration) -> ProbeResult {
    let mut samples = Vec::with_capacity(PROBES_PER_POLL);
    let mut outcome = Probe::Connected;
    for _ in 0..PROBES_PER_POLL {
        let started = Instant::now();
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => {
                samples.push(started.elapsed());
                drop(s);
                outcome = Probe::Connected;
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                samples.push(started.elapsed());
                outcome = Probe::Refused;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                outcome = Probe::NoAnswer(format!("no answer in {}ms", timeout.as_millis()));
                break;
            }
            Err(e) => {
                outcome = Probe::Failed(e.to_string());
                break;
            }
        }
    }
    ProbeResult { samples, outcome }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Handshake {
    NotConfigured(&'static str),
    FileMissing(String),
    FileError(String),
    /// The file is older than `tunnel_handshake_max_age_s`, so its collector
    /// has probably stopped and its contents say nothing about now.
    FileStale {
        path: String,
        age_s: u64,
    },
    PeerAbsent,
    Never,
    At {
        age_s: u64,
    },
}

/// The handshake file as loaded, with its age in seconds, or the state every
/// node with a public key reports when it could not be loaded.
pub type HandshakeSource = Result<(Handshakes, Option<u64>), Handshake>;

/// Read the handshake file once per poll.
///
/// A file last written more than `max_age_s` ago is `FileStale`. Its
/// timestamps are not judged, because they were true when it was written.
pub fn load(path: Option<&str>, max_age_s: u64) -> HandshakeSource {
    let Some(path) = path else {
        return Err(Handshake::NotConfigured("no wg_handshakes_file configured"));
    };
    match Handshakes::from_file(std::path::Path::new(path)) {
        Ok(None) => Err(Handshake::FileMissing(path.to_string())),
        Err(e) => Err(Handshake::FileError(format!("{path}: {e:#}"))),
        Ok(Some(handshakes)) => {
            let file_age_s = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d.as_secs());
            match file_age_s {
                Some(age_s) if age_s > max_age_s => Err(Handshake::FileStale {
                    path: path.to_string(),
                    age_s,
                }),
                _ => Ok((handshakes, file_age_s)),
            }
        }
    }
}

pub fn handshake_for(src: &HandshakeSource, public_key: Option<&str>, now_unix: u64) -> Handshake {
    let Some(key) = public_key else {
        return Handshake::NotConfigured("no public_key for this node");
    };
    match src {
        Err(h) => h.clone(),
        Ok((handshakes, _)) => match handshakes.find(key) {
            None => Handshake::PeerAbsent,
            Some(p) => match p.latest_unix {
                None => Handshake::Never,
                Some(ts) => Handshake::At {
                    age_s: now_unix.saturating_sub(ts),
                },
            },
        },
    }
}

#[derive(Debug, Clone)]
pub struct TunnelReading {
    pub address: String,
    /// First 8 characters of the configured peer public key, for messages.
    pub key_prefix: Option<String>,
    pub probe: ProbeResult,
    pub handshake: Handshake,
    /// Seconds since the handshake file was last written.
    pub file_age_s: Option<u64>,
}

impl TunnelReading {
    /// The reading for a tunnel whose probe thread panicked.
    fn panicked(address: String) -> Self {
        Self {
            address,
            key_prefix: None,
            probe: ProbeResult {
                samples: Vec::new(),
                outcome: Probe::Failed("probe thread panicked".to_string()),
            },
            handshake: Handshake::NotConfigured("probe thread panicked"),
            file_age_s: None,
        }
    }
}

fn key_prefix(key: Option<&str>) -> Option<String> {
    key.map(|k| k.trim().chars().take(8).collect())
}

/// One entry per node, in order, `None` for a node with no tunnel.
pub fn fleet(f: &crate::fleet::Fleet, timeout: Duration) -> Vec<Option<TunnelReading>> {
    let src = load(
        f.wg_handshakes_file.as_deref(),
        f.tunnel_handshake_max_age_s,
    );
    let file_age_s = match &src {
        Ok((_, age)) => *age,
        Err(Handshake::FileStale { age_s, .. }) => Some(*age_s),
        Err(_) => None,
    };
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let src = &src;
    std::thread::scope(|scope| {
        let handles: Vec<_> = f
            .nodes
            .iter()
            .map(|n| {
                n.tunnel.as_ref().map(|t| {
                    scope.spawn(move || {
                        let probe = match t.address.parse::<SocketAddr>() {
                            Ok(a) => probe(a, timeout),
                            Err(e) => ProbeResult {
                                samples: Vec::new(),
                                outcome: Probe::Failed(format!("bad tunnel address: {e}")),
                            },
                        };
                        TunnelReading {
                            address: t.address.clone(),
                            key_prefix: key_prefix(t.public_key.as_deref()),
                            probe,
                            handshake: handshake_for(src, t.public_key.as_deref(), now_unix),
                            file_age_s,
                        }
                    })
                })
            })
            .collect();
        handles
            .into_iter()
            .zip(&f.nodes)
            .map(|(h, n)| {
                h.map(|h| {
                    h.join().unwrap_or_else(|_| {
                        TunnelReading::panicked(
                            n.tunnel
                                .as_ref()
                                .map(|t| t.address.clone())
                                .unwrap_or_default(),
                        )
                    })
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const T: Duration = Duration::from_millis(500);

    #[test]
    fn a_listening_port_is_measured_as_connected() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let r = probe(l.local_addr().unwrap(), T);
        assert_eq!(r.outcome, Probe::Connected);
        assert_eq!(r.samples.len(), PROBES_PER_POLL);
        assert!(r.samples.iter().all(|s| *s > Duration::ZERO));
    }

    #[test]
    fn a_closed_port_is_a_refusal_with_a_round_trip() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let r = probe(addr, T);
        assert_eq!(r.outcome, Probe::Refused, "{r:?}");
        assert!(!r.samples.is_empty());
    }

    fn src_with(key: &str, ts: u64) -> HandshakeSource {
        Ok((
            Handshakes::parse_latest_handshakes(&format!("{key}\t{ts}\n")).unwrap(),
            Some(5),
        ))
    }

    #[test]
    fn handshake_age_is_now_minus_the_timestamp() {
        let h = handshake_for(&src_with("K", 1000), Some("K"), 1042);
        assert_eq!(h, Handshake::At { age_s: 42 });
    }

    #[test]
    fn a_timestamp_in_the_future_reads_as_zero_age() {
        let h = handshake_for(&src_with("K", 2000), Some("K"), 1000);
        assert_eq!(h, Handshake::At { age_s: 0 });
    }

    #[test]
    fn no_public_key_and_no_file_are_not_configured_rather_than_absent() {
        assert!(matches!(
            handshake_for(&src_with("K", 1), None, 10),
            Handshake::NotConfigured(_)
        ));
        assert!(matches!(
            handshake_for(&load(None, 600), Some("K"), 10),
            Handshake::NotConfigured(_)
        ));
    }

    #[test]
    fn a_file_older_than_the_limit_is_stale_and_names_its_path() {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), "K\t1\n").unwrap();
        let old = SystemTime::now() - Duration::from_secs(900);
        f.as_file().set_modified(old).unwrap();
        let path = f.path().to_str().unwrap();
        match load(Some(path), 600) {
            Err(Handshake::FileStale { path: p, age_s }) => {
                assert_eq!(p, path);
                assert!((900..910).contains(&age_s), "{age_s}");
            }
            other => panic!("{other:?}"),
        }
        assert!(load(Some(path), 1200).is_ok());
    }

    #[test]
    fn a_missing_file_and_an_absent_peer_are_different_states() {
        let missing = handshake_for(&Err(Handshake::FileMissing("/x".into())), Some("K"), 10);
        assert_eq!(missing, Handshake::FileMissing("/x".into()));
        let absent = handshake_for(&src_with("OTHER", 1), Some("K"), 10);
        assert_eq!(absent, Handshake::PeerAbsent);
        let never = handshake_for(&src_with("K", 0), Some("K"), 10);
        assert_eq!(never, Handshake::Never);
    }
}
