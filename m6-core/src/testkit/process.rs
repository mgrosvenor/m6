//! Spawning a real m6 service from a test, and killing it again.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How much of a child's output to keep. The tail is what matters when
/// diagnosing a death, so once the buffer is full the oldest bytes go.
const OUTPUT_CAP: usize = 256 * 1024;

/// How long a terminating child gets to exit on SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(1);

/// A child service process, killed when this value drops.
///
/// **The output drain is not a convenience.** Every suite that predates this
/// type spawned children with `Stdio::piped()` stderr and never read the pipe.
/// That is two bugs at once. A child that logs more than one pipe buffer
/// (64 KiB on Linux, 16 KiB on macOS) blocks in `write` and stops serving,
/// which presents as a hang or a wedged accept loop somewhere unrelated. And
/// when a child dies, everything it said about why is discarded with the pipe,
/// so the test reports `ConnectionRefused` and the cause is unrecoverable.
///
/// Here a thread per stream drains into one bounded buffer, and every
/// assertion this type makes about the service prints that buffer on failure.
///
/// **Both streams, because m6 services log to stdout.** `m6_core::log` builds
/// its writer over `std::io::stdout()`. An earlier version of this type piped
/// stderr and nulled stdout, which threw away every log line the services
/// produce; the tests that assert on a service's own output caught it.
pub struct Service {
    name: String,
    child: Option<Child>,
    output: Arc<Mutex<Vec<u8>>>,
    drains: Vec<JoinHandle<()>>,
}

impl Service {
    /// Spawn `cmd` as a named service.
    ///
    /// The caller configures the command (program, arguments, environment);
    /// this overrides only the standard streams, which it must own.
    pub fn spawn(name: &str, cmd: &mut Command) -> Service {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {name}: {e}"));

        let output = Arc::new(Mutex::new(Vec::new()));
        let mut drains = Vec::with_capacity(2);
        let out = child.stdout.take().expect("stdout was piped");
        drains.push(spawn_drain(name, "stdout", out, Arc::clone(&output)));
        let err = child.stderr.take().expect("stderr was piped");
        drains.push(spawn_drain(name, "stderr", err, Arc::clone(&output)));

        Service { name: name.to_string(), child: Some(child), output, drains }
    }

    /// The child's process id.
    pub fn pid(&self) -> u32 {
        self.child.as_ref().map(Child::id).unwrap_or(0)
    }

    /// `Some(status)` once the child has exited, `None` while it runs.
    pub fn exited(&mut self) -> Option<ExitStatus> {
        self.child.as_mut().and_then(|c| c.try_wait().ok().flatten())
    }

    /// Kill the service now rather than at drop, for a test whose subject is
    /// what happens after a backend goes away.
    ///
    /// SIGKILL, not SIGTERM: the point of calling this is usually to simulate a
    /// crash, and a clean shutdown is a different scenario.
    pub fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Send SIGTERM and wait for the child to exit, returning its status.
    ///
    /// This is how to test a service's shutdown path: SIGTERM, then assert on
    /// what it did. Panics if the child is still running after `timeout`,
    /// which is itself the failure a graceful-shutdown test is looking for.
    pub fn terminate(&mut self, timeout: Duration) -> ExitStatus {
        let child = self.child.as_mut().expect("service already reaped");
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait().ok().flatten() {
                return status;
            }
            if Instant::now() >= deadline {
                let pid = child.id();
                panic!(
                    "{} (pid {pid}) did not exit within {timeout:?} of SIGTERM\n\
                     --- output ---\n{}",
                    self.name,
                    tail(&self.output(), 40)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything the child has written to stdout and stderr so far.
    pub fn output(&self) -> String {
        let buf = self.output.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Panic with the exit status and stderr if the child is no longer running.
    ///
    /// Call this wherever a test is about to blame the network for something
    /// that is actually a dead server.
    pub fn assert_alive(&mut self, context: &str) {
        if let Some(status) = self.exited() {
            panic!("{}", self.death_report(context, status));
        }
    }

    /// Wait until `port` accepts a connection, or fail with a real reason.
    ///
    /// Returns early, without burning the whole timeout, if the child exits
    /// first: waiting ten seconds for a process that died in fifty
    /// milliseconds wastes time and reports the wrong fact.
    pub fn wait_for_tcp(&mut self, port: u16, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if super::wait::for_tcp(port, Duration::from_millis(25)) {
                return;
            }
            if let Some(status) = self.exited() {
                panic!("{}", self.death_report(&format!("waiting for port {port}"), status));
            }
            if Instant::now() >= deadline {
                panic!(
                    "{} never listened on port {port} within {timeout:?}\n--- output ---\n{}",
                    self.name,
                    tail(&self.output(), 40)
                );
            }
        }
    }

    /// Wait until `path` exists, or fail with a real reason.
    ///
    /// Backend readiness is a unix socket showing up, not a fixed delay. A
    /// `sleep(300ms)` in its place passed when a suite ran alone and failed
    /// under a full workspace run, where a dozen stacks start at once: the
    /// server came up with an empty backend pool and answered 502.
    pub fn wait_for_path(&mut self, path: &Path, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if path.exists() {
                return;
            }
            if let Some(status) = self.exited() {
                panic!(
                    "{}",
                    self.death_report(&format!("waiting for {}", path.display()), status)
                );
            }
            if Instant::now() >= deadline {
                panic!(
                    "{} never created {} within {timeout:?}\n--- output ---\n{}",
                    self.name,
                    path.display(),
                    tail(&self.output(), 40)
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn death_report(&self, context: &str, status: ExitStatus) -> String {
        format!(
            "{} exited while {context}: {status}\n--- output ---\n{}",
            self.name,
            tail(&self.output(), 40)
        )
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().ok().flatten().is_none() {
                // SIGTERM first, so the service's own shutdown path runs and a
                // test that wedges it still gets cleaned up by the SIGKILL.
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
                let deadline = Instant::now() + TERM_GRACE;
                while Instant::now() < deadline {
                    if child.try_wait().ok().flatten().is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        // The child is gone, so its ends of the pipes are closed and each
        // drain thread's read returns 0. Joining here keeps them from
        // outliving the buffer they write into.
        for h in self.drains.drain(..) {
            let _ = h.join();
        }
    }
}

fn spawn_drain(
    name: &str,
    stream: &str,
    mut pipe: impl Read + Send + 'static,
    sink: Arc<Mutex<Vec<u8>>>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("{name}-{stream}"))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        let mut out = sink.lock().unwrap_or_else(|e| e.into_inner());
                        out.extend_from_slice(&buf[..n]);
                        if out.len() > OUTPUT_CAP {
                            let excess = out.len() - OUTPUT_CAP;
                            out.drain(..excess);
                        }
                    }
                }
            }
        })
        .expect("spawn output drain")
}

/// The last `n` lines of `s`, for a panic message that stays readable.
fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    if start == 0 {
        s.to_string()
    } else {
        format!("({} earlier lines omitted)\n{}", start, lines[start..].join("\n"))
    }
}

/// Assert a service logged the lifecycle lines `m6_core::signal` emits for
/// every service.
///
/// The lines are core's, not the app's, so this is the same assertion
/// everywhere and it is the guard against them drifting apart again. Startup
/// and shutdown used to be logged unevenly: two of five services said
/// "starting" and never "started", `m6-md` said nothing at all, and all three
/// render apps logged `m6-render` rather than their own name.
///
/// `output` is [`Service::output`]; `name` is the service's own name, so a
/// test also catches a service logging under the wrong one.
pub fn assert_lifecycle_logged(name: &str, output: &str) {
    for expected in [
        format!("{name} started"),
        format!("{name} shutdown signal received"),
        format!("{name} shutdown complete"),
    ] {
        assert!(
            output.contains(&expected),
            "{name} never logged {expected:?}\n--- output ---\n{output}"
        );
    }
}
