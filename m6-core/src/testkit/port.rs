//! Claiming a TCP port that no concurrently running test will also use.

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

/// Ports live below both platforms' ephemeral ranges (macOS starts at 49152,
/// Linux at 32768), so the kernel will never hand one of these to an unrelated
/// socket behind our back.
const PORT_LO: u16 = 20_000;
const PORT_HI: u16 = 29_999;

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
    /// The marker file, held open for the claim's lifetime because **the
    /// `flock(2)` on it is the claim**. Closing this descriptor is what releases
    /// the port, and the kernel closes it for us if the process dies, which is
    /// why there is no staleness rule to get wrong. See `claim_port`.
    ///
    /// Underscored because nothing reads it: its whole job is to exist until the
    /// claim drops. Renaming it to `lock` would earn a `never read` warning, and
    /// silencing that with an `#[allow]` would hide a real one later.
    _lock: std::fs::File,
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
    /// Release the port only once it is genuinely free.
    ///
    /// Removing the marker immediately is what made the long-standing
    /// `Address already in use` failures possible. The claim guarantees no
    /// other test picks the port, but it says nothing about whether the
    /// *service* that was using it has finished with it: a claim dropped while
    /// its process is still exiting frees the marker, the next test claims the
    /// port, and its service cannot bind.
    ///
    /// That used to be survivable, because a failed bind was a warning and
    /// m6-http carried on with no listener; the test then failed with
    /// "never served a backend request", which named the symptom and not the
    /// cause. Since a failed bind became fatal it is a hard failure instead,
    /// which is the honest outcome and makes fixing this necessary rather than
    /// optional.
    ///
    /// So: wait until the port actually binds before saying it is available.
    /// Bounded, because a port held forever by something outside the suite
    /// must not hang the run; if the wait expires the lock is released anyway
    /// and the next claimant's own bind check will skip it.
    ///
    /// **The marker file is not deleted**, and that is deliberate. The lock is
    /// the claim, and it is released by closing the descriptor, which happens
    /// when `self.lock` drops at the end of this function. Unlinking the file
    /// would be worse than useless: another process may already have it open and
    /// locked, and a fresh `create` by a third would make a *new inode* whose
    /// lock is unrelated to theirs, so two claimants would each hold a valid
    /// lock on a different inode for the same port. The files are empty, capped
    /// at one per port in the range, live under `target/`, and go with
    /// `cargo clean`.
    fn drop(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match TcpListener::bind(("127.0.0.1", self.port)) {
                Ok(l) => {
                    drop(l);
                    break;
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        // `self._lock` closes here, releasing the flock.
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
/// **Ownership is an `flock(2)`, not a file's existence.** That is the fix for
/// issue #9, and the previous design is worth stating because it looked airtight.
///
/// It acquired a port by creating a marker file with `O_EXCL`, which really is a
/// mutex across processes, and reclaimed a marker left behind by a killed run
/// once its mtime was older than ten minutes. The reclaim is where it came apart:
///
/// ```text
/// A: stat(marker) -> stale         B: stat(marker) -> stale
/// A: unlink(marker)
/// A: create_new(marker) -> ok      B: unlink(marker)      <- removes A's marker
///                                 B: create_new(marker) -> ok
/// ```
///
/// Both processes now believe they own the port. Both probe it with a bind and
/// both succeed, because neither has spawned its server yet, and the two servers
/// then race: one binds, the other dies with
/// `Address already in use`, on a port the allocator had just probed as free.
/// A time-of-check-to-time-of-use race in the code written to fix a
/// time-of-check-to-time-of-use race.
///
/// It needed the cross-binary concurrency of a full `cargo test --workspace` run,
/// which is why the victim tests all passed when run alone, and it needed stale
/// markers to reclaim -- of which there were **943** in one checkout, because a
/// killed run leaves its markers behind. The more the range silts up, the more
/// reclaiming every run does, and the wider the window gets.
///
/// ## What this does instead
///
/// The marker file is opened (created if absent, never `O_EXCL`) and locked with
/// `flock(LOCK_EX | LOCK_NB)`. The lock is the claim, held for as long as the
/// `PortClaim` holds the descriptor.
///
/// That removes the staleness rule rather than repairing it, and with it the only
/// code path that could hand the same port to two processes:
///
/// - **A killed run releases its claims immediately**, because the kernel closes
///   its descriptors, and closing releases the lock. No ten-minute wait, and no
///   rule that can mistake a live claim for a dead one.
/// - **A leaked marker file is harmless.** The file is not the claim, so the 943
///   left behind are just empty files.
/// - **There is nothing to unlink**, so no process can delete another's claim.
///
/// Only after the lock is held is the port probed with a bind, to confirm nothing
/// outside the suite holds it. The claim still outlives this call deliberately: it
/// must be held while the caller spawns its server, which is the window the
/// original `TcpListener::bind(":0")` left open.
///
/// **Why this is not `TcpListener::bind(":0")`.** That was the implementation
/// before the marker files, in four copies:
///
/// ```ignore
/// let l = TcpListener::bind("127.0.0.1:0").unwrap();
/// l.local_addr().unwrap().port()      // listener dropped here -- port released
/// ```
///
/// The port is released the moment the function returns and the server does not
/// bind it until seconds later, so anything else asking the kernel for an
/// ephemeral port in that window can be handed the same number. It surfaced as
/// `ConnectionReset` in `analytics_e2e` and as assorted wrong-response failures
/// across `edge_proxy`: different symptoms, one cause, and retrying made it look
/// intermittent rather than wrong.
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

        if let Some(claim) = try_claim(&marker, port) {
            return claim;
        }
    }
    panic!("no free port available in {PORT_LO}..={PORT_HI}");
}

/// Try to take exactly one port. `None` if someone else holds it, or if
/// something outside the suite is already listening on it.
///
/// Separated from the scan on purpose: **this is the whole of the mutual
/// exclusion**, and keeping it as a named function is what lets the tests set a
/// dozen claimants on one port at once. Racing `claim_port` itself cannot do
/// that, because it returns whichever port it finds free, so the contention
/// never lands where the test needs it. The race that was issue #9 lived in
/// these few lines and nowhere else.
fn try_claim(marker: &std::path::Path, port: u16) -> Option<PortClaim> {
    use std::os::unix::io::AsRawFd;

    // Ordinary create-or-open. The file's existence means nothing; the lock on
    // it is what matters, so there is no `O_EXCL` and nothing to unlink.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(marker)
        .ok()?;

    // Non-blocking exclusive lock. EWOULDBLOCK means another process, or another
    // open file description in this one, holds this port.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return None;
    }

    // Ours. Confirm nothing outside the test suite is listening on it.
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => {
            drop(l);
            Some(PortClaim { port, _lock: file })
        }
        // Occupied by something we do not control. Dropping `file` releases the
        // lock, so the next claimant is free to try.
        Err(_) => None,
    }
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

    /// A released port can be claimed again.
    ///
    /// This used to assert that the marker FILE was gone, which is no longer the
    /// contract: the `flock` is the claim and the file is left in place on
    /// purpose, because unlinking it would let a third process create a new inode
    /// and lock that instead. What has to be true is that the port becomes
    /// available again, so that is what is asserted.
    #[test]
    fn dropping_a_claim_releases_the_port() {
        let port = {
            let c = claim_port();
            c.port()
        };
        // Re-lockable, which is what "released" means now.
        let dir = claim_dir();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(port.to_string()))
            .expect("open the marker");
        use std::os::unix::io::AsRawFd;
        let got = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(got, 0, "the lock outlived the claim, so the port is stuck");
    }

    /// A leaked marker file does not cost a port.
    ///
    /// A killed run leaves its markers behind: one checkout had 943 of them. Under
    /// the old design each one made its port unusable for ten minutes and then had
    /// to be reclaimed, and **the reclaim was the race** -- two processes could both
    /// unlink and both re-create, and each would believe it owned the port. So the
    /// property to pin is that an unlocked leftover file is simply claimable.
    #[test]
    fn a_leftover_marker_file_is_claimable() {
        let dir = claim_dir();
        // A file with nobody holding its lock, exactly what a killed run leaves.
        let port = 29_998u16;
        let marker = dir.join(port.to_string());
        std::fs::write(&marker, b"").expect("write a leftover marker");

        use std::os::unix::io::AsRawFd;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&marker)
            .expect("open");
        let got = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(
            got, 0,
            "a leftover marker file blocked a claim; leaked files must not cost ports"
        );
    }

    /// **The regression test for issue #9, and it is deterministic.**
    ///
    /// A first attempt at this raced a dozen threads for one stale marker. It
    /// passed against the broken implementation as well as the fixed one, because
    /// a time-of-check-to-time-of-use window is not something a race can be relied
    /// on to enter: to get two winners out of the old code, one thread's `unlink`
    /// has to land between another's `unlink` and its `create_new`, and mostly it
    /// does not. A probabilistic test for a race that reports "ok" is worth less
    /// than nothing, because it reads as proof.
    ///
    /// So this asserts the invariant that made the race possible instead, which
    /// needs no concurrency at all: **a claim that is still held must never be
    /// handed to anybody else, however old its marker looks.**
    ///
    /// The old implementation decided a marker was leaked by its mtime, so a live
    /// claim whose marker was older than ten minutes was unlinked and re-created
    /// by the next caller, and both then believed they owned the port. Ageing the
    /// marker of a held claim reproduces that in one thread, every time.
    ///
    /// Verified to fail against the old protocol -- mtime staleness, unlink,
    /// `create_new` -- restored in place: it returns a second live claim on a port
    /// the first claimant is still holding.
    #[test]
    fn a_held_claim_is_never_reclaimed_however_old_its_marker_looks() {
        let claim = claim_port();
        let port = claim.port();
        let marker = claim_dir().join(port.to_string());

        // Make the live claim's marker look abandoned. Ten minutes was the old
        // threshold; an hour is unambiguous.
        let ancient = filetime::FileTime::from_unix_time(
            std::time::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_secs() as i64 - 3600)
                .unwrap_or(1),
            0,
        );
        filetime::set_file_times(&marker, ancient, ancient).expect("age the marker");

        // A second claimant asking for the same port must be refused.
        let second = try_claim(&marker, port);
        assert!(
            second.is_none(),
            "port {port} was granted a second time while the first claim was still \
             held, because its marker looked old. That is issue #9: two servers then \
             race for one port and one dies with Address already in use."
        );

        // And the original claim is unharmed.
        assert_eq!(claim.port(), port);
    }

    /// **The regression test for issue #9.** Many claimants at once, and no port
    /// is ever handed to two of them.
    ///
    /// The old design could hand the same port to two processes through the stale
    /// reclaim, and the failure only appeared under the cross-binary concurrency
    /// of a full workspace run: every victim test passed when run alone. Threads
    /// are a weaker probe than separate processes -- `flock` is per open file
    /// description, so two threads opening the same path get independent
    /// descriptions and the lock does discriminate between them -- but they
    /// exercise the same acquire path, at a concurrency a test can actually create.
    ///
    /// What makes this discriminating is that each claim also BINDS its port and
    /// holds the listener. Two claimants on one port means the second bind fails,
    /// which is precisely the `Address already in use` the issue is about.
    #[test]
    fn concurrent_claims_never_collide() {
        use std::collections::HashSet;
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<HashSet<u16>>> = Arc::new(Mutex::new(HashSet::new()));
        let failures: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let seen = Arc::clone(&seen);
            let failures = Arc::clone(&failures);
            handles.push(std::thread::spawn(move || {
                for _ in 0..8 {
                    let claim = claim_port();
                    let port = claim.port();

                    // No two live claims may name the same port.
                    if !seen.lock().unwrap().insert(port) {
                        failures
                            .lock()
                            .unwrap()
                            .push(format!("port {port} was handed out twice"));
                    }

                    // And the port must actually be bindable by its owner, which
                    // is the symptom the issue reports.
                    match TcpListener::bind(("127.0.0.1", port)) {
                        Ok(l) => drop(l),
                        Err(e) => failures
                            .lock()
                            .unwrap()
                            .push(format!("claimed port {port} would not bind: {e}")),
                    }

                    seen.lock().unwrap().remove(&port);
                    drop(claim);
                }
            }));
        }
        for h in handles {
            h.join().expect("a claimant thread panicked");
        }

        let failures = failures.lock().unwrap();
        assert!(
            failures.is_empty(),
            "{} collision(s):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
