//! Shared helpers for the integration suites.
//!
//! Exists for one reason so far: making test startup deterministic, so a suite
//! that passes alone also passes under a full workspace run.
//!
//! `dead_code` is allowed because Rust compiles this module separately into
//! every test binary that declares it, and no single binary uses all of it.
//! Without this, each binary warns about the helpers it happens not to call.
#![allow(dead_code)]

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

fn claim_dir() -> PathBuf {
    // Beside the build output, so `cargo clean` disposes of it and it is never
    // shared between checkouts.
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target")
        .join("test-ports");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Allocate a TCP port no other concurrently running test will also use.
///
/// **Why this is not `TcpListener::bind(":0")`.** That was the previous
/// implementation, in four copies:
///
/// ```ignore
/// let l = TcpListener::bind("127.0.0.1:0").unwrap();
/// l.local_addr().unwrap().port()      // listener dropped here — port released
/// ```
///
/// It is a time-of-check/time-of-use race. The port is released the moment the
/// function returns, but the server does not bind it until seconds later, after
/// a child process has been spawned and its socket waited for. Anything else
/// asking the kernel for an ephemeral port in that window can be handed the
/// same number — including another test, in another test binary, that cargo is
/// running at the same moment.
///
/// The result was a suite that passed alone and failed in a full run: two
/// servers racing for one port, so one failed to start, or a client connected
/// to the wrong server. It surfaced as `ConnectionReset` in `analytics_e2e` and
/// as assorted wrong-response failures across `edge_proxy` — different symptoms,
/// one cause. Retrying made it look intermittent rather than wrong.
///
/// **What this does instead.** A candidate port is claimed by atomically
/// creating a marker file for it (`create_new`, i.e. `O_EXCL`), which is a real
/// mutex across processes, not just across threads. Only after the claim
/// succeeds is the port probed to confirm nothing already holds it. The claim
/// outlives this call deliberately: it must still be held while the caller
/// spawns its server, which is precisely the window the old code left open.
///
/// A claim left behind by a killed run is reclaimed once it is older than
/// `STALE_AFTER`, so a crashed suite cannot slowly poison the range.
pub fn free_port() -> u16 {
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
                return port;
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

/// Wait for a filesystem path to appear.
///
/// Backend readiness is a unix socket showing up, not a fixed delay. A
/// `sleep(300ms)` in its place passed when a suite ran alone and failed under a
/// full workspace run, where a dozen stacks start at once: the server came up
/// with an empty backend pool and answered 502.
pub fn wait_for_path(p: &std::path::Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if p.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}
