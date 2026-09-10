//! Claiming a TCP port that no concurrently running test will also use.

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, SystemTime};

/// Ports live below both platforms' ephemeral ranges (macOS starts at 49152,
/// Linux at 32768), so the kernel will never hand one of these to an unrelated
/// socket behind our back.
const PORT_LO: u16 = 20_000;
const PORT_HI: u16 = 29_999;

/// A claim older than this is treated as leaked by a killed test run and
/// reclaimed. Comfortably longer than any suite's server lifetime.
const STALE_AFTER: Duration = Duration::from_secs(600);

static NEXT: AtomicU16 = AtomicU16::new(0);

/// An exclusive claim on a loopback port, held until dropped.
///
/// **Hold this for as long as the server holds the port.** Dropping it
/// releases the port back to other tests, so binding it to a local that goes
/// out of scope before the service does reintroduces exactly the race this
/// type exists to close. Store it beside the [`Service`](super::Service) it
/// belongs to.
#[must_use = "dropping the claim releases the port while the server is still using it"]
pub struct PortClaim {
    port: u16,
    marker: PathBuf,
}

impl PortClaim {
    /// The claimed port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The claimed port as a loopback address, for formatting into a config.
    pub fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for PortClaim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.marker);
    }
}

fn claim_dir() -> PathBuf {
    // Beside the build output, so `cargo clean` disposes of it and it is never
    // shared between checkouts.
    let d = super::paths::target_dir().join("test-ports");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Claim a TCP port no other concurrently running test will also use.
///
/// **Why this is not `TcpListener::bind(":0")`.** That was the previous
/// implementation, in four copies:
///
/// ```ignore
/// let l = TcpListener::bind("127.0.0.1:0").unwrap();
/// l.local_addr().unwrap().port()      // listener dropped here — port released
/// ```
///
/// It is a time-of-check-to-time-of-use race. The port is released the moment
/// the function returns, but the server does not bind it until seconds later,
/// after a child process has been spawned and its socket waited for. Anything
/// else asking the kernel for an ephemeral port in that window can be handed
/// the same number, including another test, in another test binary, that cargo
/// is running at the same moment.
///
/// The result was a suite that passed alone and failed in a full run: two
/// servers racing for one port, so one failed to start, or a client connected
/// to the wrong server. It surfaced as `ConnectionReset` in `analytics_e2e`
/// and as assorted wrong-response failures across `edge_proxy`, different
/// symptoms with one cause. Retrying made it look intermittent rather than
/// wrong.
///
/// **What this does instead.** A candidate port is claimed by atomically
/// creating a marker file for it (`create_new`, meaning `O_EXCL`), which is a
/// real mutex across processes and not just across threads. Only after the
/// claim succeeds is the port probed to confirm nothing already holds it. The
/// claim outlives this call deliberately: it must still be held while the
/// caller spawns its server, which is precisely the window the old code left
/// open. It is released when the returned [`PortClaim`] drops.
///
/// A claim left behind by a killed run is reclaimed once it is older than
/// `STALE_AFTER`, so a crashed suite cannot slowly poison the range.
pub fn claim_port() -> PortClaim {
    let dir = claim_dir();
    // Start each process at a different offset so two binaries starting
    // together do not contend on the same first candidate.
    let start = NEXT.fetch_add(1, Ordering::Relaxed) as u32
        + (std::process::id() % (PORT_HI - PORT_LO) as u32);

    let span = (PORT_HI - PORT_LO) as u32;
    for i in 0..span {
        let port = PORT_LO + ((start + i) % span) as u16;
        let marker = dir.join(port.to_string());

        // Reclaim a claim left behind by a run that was killed.
        if let Ok(meta) = std::fs::metadata(&marker) {
            let stale = meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .map(|age| age > STALE_AFTER)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(&marker);
            }
        }

        // O_EXCL create: succeeds for exactly one process.
        if std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .is_err()
        {
            continue; // claimed by someone else
        }

        // Claimed. Confirm nothing outside the test suite holds it.
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => {
                drop(l);
                return PortClaim { port, marker };
            }
            Err(_) => {
                // Occupied by something we do not control; release the claim
                // and move on rather than handing back a port that cannot bind.
                let _ = std::fs::remove_file(&marker);
            }
        }
    }
    panic!("no free port available in {PORT_LO}..={PORT_HI}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_are_distinct_and_bindable() {
        let a = claim_port();
        let b = claim_port();
        assert_ne!(a.port(), b.port());
        // Both must still be bindable: claiming must not itself occupy them.
        let la = TcpListener::bind(("127.0.0.1", a.port())).expect("bind a");
        let lb = TcpListener::bind(("127.0.0.1", b.port())).expect("bind b");
        drop((la, lb));
    }

    #[test]
    fn dropping_a_claim_releases_it() {
        let port = {
            let c = claim_port();
            c.port()
        };
        // The marker must be gone, so the same port can be claimed again.
        let dir = claim_dir();
        assert!(!dir.join(port.to_string()).exists(), "marker outlived the claim");
    }
}
