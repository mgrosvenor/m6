//! Spawning a real m6 service from a test, and killing it again.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How much of a child's stderr to keep. The tail is what matters when
/// diagnosing a death, so once the buffer is full the oldest bytes go.
const STDERR_CAP: usize = 256 * 1024;

/// How long a terminating child gets to exit on SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(1);

/// A child service process, killed when this value drops.
///
/// **The stderr drain is not a convenience.** Every suite that predates this
/// type spawned children with `Stdio::piped()` stderr and never read the pipe.
/// That is two bugs at once. A child that logs more than one pipe buffer
/// (64 KiB on Linux, 16 KiB on macOS) blocks in `write` and stops serving,
/// which presents as a hang or a wedged accept loop somewhere unrelated. And
/// when a child dies, everything it said about why is discarded with the pipe,
/// so the test reports `ConnectionRefused` and the cause is unrecoverable.
///
/// Here a thread drains stderr continuously into a bounded buffer, and every
/// assertion this type makes about the service prints that buffer on failure.
pub struct Service {
    name: String,
    child: Option<Child>,
    stderr: Arc<Mutex<Vec<u8>>>,
    drain: Option<JoinHandle<()>>,
}

impl Service {
    /// Spawn `cmd` as a named service.
    ///
    /// The caller configures the command (program, arguments, environment);
    /// this overrides only the standard streams, which it must own.
    pub fn spawn(name: &str, cmd: &mut Command) -> Service {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {name}: {e}"));

        let stderr = Arc::new(Mutex::new(Vec::new()));
        let pipe = child.stderr.take().expect("stderr was piped");
        let sink = Arc::clone(&stderr);
        let drain = std::thread::Builder::new()
            .name(format!("{name}-stderr"))
            .spawn(move || drain_stderr(pipe, sink))
            .expect("spawn stderr drain");

        Service { name: name.to_string(), child: Some(child), stderr, drain: Some(drain) }
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
                     --- stderr ---\n{}",
                    self.name,
                    tail(&self.stderr_text(), 40)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything the child has written to stderr so far.
    pub fn stderr_text(&self) -> String {
        let buf = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
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
                    "{} never listened on port {port} within {timeout:?}\n--- stderr ---\n{}",
                    self.name,
                    tail(&self.stderr_text(), 40)
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
                    "{} never created {} within {timeout:?}\n--- stderr ---\n{}",
                    self.name,
                    path.display(),
                    tail(&self.stderr_text(), 40)
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn death_report(&self, context: &str, status: ExitStatus) -> String {
        format!(
            "{} exited while {context}: {status}\n--- stderr ---\n{}",
            self.name,
            tail(&self.stderr_text(), 40)
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
        // The child is gone, so its end of the pipe is closed and the drain
        // thread's read returns 0. Joining here keeps the thread from
        // outliving the buffer it writes into.
        if let Some(h) = self.drain.take() {
            let _ = h.join();
        }
    }
}

fn drain_stderr(mut pipe: std::process::ChildStderr, sink: Arc<Mutex<Vec<u8>>>) {
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut out = sink.lock().unwrap_or_else(|e| e.into_inner());
                out.extend_from_slice(&buf[..n]);
                if out.len() > STDERR_CAP {
                    let excess = out.len() - STDERR_CAP;
                    out.drain(..excess);
                }
            }
        }
    }
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
