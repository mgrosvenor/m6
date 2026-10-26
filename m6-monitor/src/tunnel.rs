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
//! monitor's own TLS work does not distort the round trip. `--check` runs the
//! same probe once, in its own process. Nothing runs in a request handler.

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use m6_core::wireguard::Handshakes;

/// The most connects per poll.
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

/// Time up to `PROBES_PER_POLL` connects, one after another, inside one
/// deadline of `timeout` for the whole probe.
///
/// Each connect is given what is left of the budget. A connect that times out
/// has waited out everything it was given, so nothing is left and the probe
/// ends. A dead tunnel therefore costs one `timeout`, which keeps a poll
/// inside the poller's stale limit however large `timeout` is.
///
/// One connect that gets through means the tunnel answered, so the outcome is
/// `Connected` and the samples are the connects that got through. The sample
/// count shows a partial loss. Only a poll with no successful connect is
/// `NoAnswer` or `Failed`. The stream is dropped at once and nothing is sent.
pub fn probe(addr: SocketAddr, timeout: Duration) -> ProbeResult {
    probe_with(timeout, |remaining| {
        TcpStream::connect_timeout(&addr, remaining).map(drop)
    })
}

fn probe_with(
    timeout: Duration,
    mut connect: impl FnMut(Duration) -> std::io::Result<()>,
) -> ProbeResult {
    let mut samples = Vec::with_capacity(PROBES_PER_POLL);
    let mut connected = 0;
    let mut refused = 0;
    let mut timed_out = false;
    let mut other_error = None;
    let deadline = Instant::now() + timeout;
    for _ in 0..PROBES_PER_POLL {
        let started = Instant::now();
        let remaining = deadline.saturating_duration_since(started);
        if remaining.is_zero() {
            break;
        }
        match connect(remaining) {
            Ok(()) => {
                samples.push(started.elapsed());
                connected += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                samples.push(started.elapsed());
                refused += 1;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                timed_out = true;
                // It waited out the whole budget it was given.
                break;
            }
            Err(e) => other_error = Some(e.to_string()),
        }
    }
    let outcome = if connected > 0 {
        Probe::Connected
    } else if refused > 0 {
        Probe::Refused
    } else if timed_out {
        Probe::NoAnswer(format!("no answer in {}ms", timeout.as_millis()))
    } else {
        Probe::Failed(other_error.unwrap_or_else(|| "no connect attempted".to_string()))
    };
    ProbeResult { samples, outcome }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Handshake {
    /// The node has a `tunnel` but no `public_key`, so no peer can be looked up.
    NoPublicKey,
    /// Tunnels are configured but `wg_handshakes_file` is not set.
    NoHandshakesFile,
    /// The handshake could not be read for a reason already reported elsewhere.
    Unknown(&'static str),
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
        return Err(Handshake::NoHandshakesFile);
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
    // A file problem is the monitor's and is reported first.
    let (handshakes, _) = match src {
        Err(h) => return h.clone(),
        Ok(ok) => ok,
    };
    let Some(key) = public_key else {
        return Handshake::NoPublicKey;
    };
    match handshakes.find(key) {
        None => Handshake::PeerAbsent,
        Some(p) => match p.latest_unix {
            None => Handshake::Never,
            Some(ts) => Handshake::At {
                age_s: now_unix.saturating_sub(ts),
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
            handshake: Handshake::Unknown("probe thread panicked"),
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

    fn scripted(results: Vec<std::io::Result<()>>) -> ProbeResult {
        let mut it = results.into_iter();
        probe_with(T, move |_| it.next().unwrap())
    }

    /// Run the probe with the scripted results and return the sum of the
    /// timeouts it handed to the connect, with the result.
    fn budget_used(results: Vec<std::io::Result<()>>) -> (Duration, ProbeResult) {
        let mut it = results.into_iter();
        let mut total = Duration::ZERO;
        let r = probe_with(T, |t| {
            total += t;
            it.next().unwrap_or_else(lost)
        });
        (total, r)
    }

    #[test]
    fn a_dead_tunnel_costs_one_timeout_in_total() {
        let (total, r) = budget_used(vec![lost(), lost(), lost()]);
        assert!(matches!(r.outcome, Probe::NoAnswer(_)), "{r:?}");
        assert!(total <= T, "handed out {total:?} of a {T:?} budget");
    }

    #[test]
    fn two_quick_successes_then_a_loss_is_connected() {
        let (total, r) = budget_used(vec![Ok(()), Ok(()), lost()]);
        assert_eq!(r.outcome, Probe::Connected, "{r:?}");
        assert_eq!(r.samples.len(), 2, "{r:?}");
        assert!(total <= T * 3, "{total:?}");
    }

    #[test]
    fn a_connect_is_never_given_more_than_what_is_left() {
        let mut seen = Vec::new();
        probe_with(T, |t| {
            seen.push(t);
            std::thread::sleep(Duration::from_millis(20));
            Ok(())
        });
        assert_eq!(seen.len(), PROBES_PER_POLL);
        assert!(seen.windows(2).all(|w| w[1] < w[0]), "{seen:?}");
        assert!(seen.iter().all(|t| *t <= T));
    }

    fn lost() -> std::io::Result<()> {
        Err(std::io::ErrorKind::TimedOut.into())
    }

    #[test]
    fn a_lost_connect_after_a_good_one_still_means_the_tunnel_answered() {
        for (results, n) in [
            (vec![Ok(()), lost(), Ok(())], 1),
            (vec![Ok(()), Ok(()), lost()], 2),
        ] {
            let r = scripted(results);
            assert_eq!(r.outcome, Probe::Connected, "{r:?}");
            assert_eq!(r.samples.len(), n, "{r:?}");
        }
    }

    #[test]
    fn only_zero_successes_is_no_answer() {
        let r = scripted(vec![lost(), lost(), lost()]);
        assert!(matches!(r.outcome, Probe::NoAnswer(_)), "{r:?}");
        assert!(r.samples.is_empty());
    }

    #[test]
    fn an_other_error_with_no_success_is_failed() {
        let r = scripted(vec![
            Err(std::io::ErrorKind::NetworkUnreachable.into()),
            Err(std::io::ErrorKind::NetworkUnreachable.into()),
            Err(std::io::ErrorKind::NetworkUnreachable.into()),
        ]);
        assert!(matches!(r.outcome, Probe::Failed(_)), "{r:?}");
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
    fn a_node_with_no_public_key_and_a_tunnel_with_no_file_say_which_is_missing() {
        assert_eq!(
            handshake_for(&src_with("K", 1), None, 10),
            Handshake::NoPublicKey
        );
        assert_eq!(
            handshake_for(&load(None, 600), Some("K"), 10),
            Handshake::NoHandshakesFile
        );
        // A missing file is the monitor's problem and is reported first.
        assert_eq!(
            handshake_for(&load(None, 600), None, 10),
            Handshake::NoHandshakesFile
        );
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
