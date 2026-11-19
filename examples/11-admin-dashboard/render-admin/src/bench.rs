//! Bench jobs: one m6-bench child at a time, its output read from a pipe by
//! the loop.
//!
//! Starting a job spawns the child, hands its stdout to the caller as a pipe,
//! and opens the child's exit as a descriptor (`ChildExit`: a pidfd on Linux,
//! a kqueue on macOS). The caller registers both with the loop's poll. Every
//! readable turn on the pipe reads until the pipe would block. End of file
//! closes the pipe, the exit descriptor becomes readable when the child exits,
//! and the job is done when both have happened: the exit event reaps the child
//! and the result is recorded here.

use m6_core::platform::{ChildExit, Poll};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct BenchJob {
    pub id: String,
    pub target: String,
    pub status: JobStatus,
    pub output: String,
    pub started_at: u64,
    pub finished_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Running,
    Done,
    Failed,
}

pub type JobStore = HashMap<String, BenchJob>;

pub fn new_job_store() -> JobStore {
    HashMap::new()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A bench that is running now: the child, its exit descriptor, its output pipe
/// and the bytes read from it so far. One job at a time, so the loop holds at
/// most one.
pub struct Running {
    job_id: String,
    /// Held so the process handle lives as long as the job. Dropping it does not
    /// reap the child: [`ChildExit::reap`] does.
    _child: Child,
    /// Readable when the child exits.
    exit: ChildExit,
    output: ChildStdout,
    bytes: Vec<u8>,
    /// The pipe reached end of file.
    eof: bool,
    /// The exit event reaped the child.
    reaped: bool,
    /// The child exited with status zero.
    succeeded: bool,
}

impl Running {
    /// The pipe's descriptor, for the loop to register and deregister.
    pub fn pipe_fd(&self) -> RawFd {
        self.output.as_raw_fd()
    }

    /// The exit descriptor, for the loop to register and deregister. It becomes
    /// readable when the child exits.
    pub fn exit_fd(&self) -> RawFd {
        self.exit.raw_fd()
    }

    /// Read what the child has written since the last turn. Returns true at
    /// end of file, which leaves the job waiting for the exit event.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<bool> {
        loop {
            match self.output.read(buf) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(true);
                }
                Ok(n) => self.bytes.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Reap the child after its exit descriptor became readable. The status is
    /// kept for [`finish`].
    ///
    /// [`finish`]: Running::finish
    pub fn reap(&mut self) -> io::Result<()> {
        self.succeeded = self.exit.reap()?.success();
        self.reaped = true;
        Ok(())
    }

    /// Both the pipe reached end of file and the exit event reaped the child.
    pub fn done(&self) -> bool {
        self.eof && self.reaped
    }

    /// Record the job's output and exit status. Called once [`done`] is true.
    ///
    /// [`done`]: Running::done
    pub fn finish(self, jobs: &mut JobStore) {
        if let Some(job) = jobs.get_mut(&self.job_id) {
            job.output = String::from_utf8_lossy(&self.bytes).to_string();
            job.status = if self.succeeded {
                JobStatus::Done
            } else {
                JobStatus::Failed
            };
            job.finished_at = Some(now_secs());
        }
    }
}

/// One readiness event for the running bench, from the loop's token.
pub enum BenchEvent<'a> {
    /// The output pipe is readable: read what the child wrote into this.
    Pipe(&'a mut [u8]),
    /// The exit descriptor is readable: reap the child.
    Exit,
}

/// Handle one bench event: read the pipe or reap the child, then record the
/// job once its pipe has reached end of file and its child has been reaped.
/// The loop calls this from both its bench arms.
pub fn bench_event(
    bench: &mut Option<Running>,
    jobs: &mut JobStore,
    poll: &Poll,
    event: BenchEvent<'_>,
) -> io::Result<()> {
    let Some(run) = bench.as_mut() else {
        return Ok(());
    };
    match event {
        BenchEvent::Pipe(buf) => {
            if run.read(buf)? {
                poll.registry().deregister_fd(run.pipe_fd())?;
            }
        }
        BenchEvent::Exit => {
            // Deregister before reaping: a reaped pidfd is reported once more.
            poll.registry().deregister_fd(run.exit_fd())?;
            run.reap()?;
        }
    }
    if run.done() {
        let run = bench.take().expect("a done bench is still running");
        run.finish(jobs);
    }
    Ok(())
}

/// Derive bench targets from the site.toml bind address.
/// Returns JSON blob: { "targets": [ { "id": "h1", "label": "HTTP/1.1 127.0.0.1:8080",
///                                     "addr": "127.0.0.1:8080", "proto": "h1" }, ... ] }
pub fn targets_blob(system_toml: &Path) -> Value {
    let targets = discover_targets(system_toml);
    json!({ "targets": targets })
}

/// Bench targets on the port m6-http listens on; none when the system config names no bind.
pub fn discover_targets(system_toml: &Path) -> Vec<Value> {
    let Some(bind) = crate::server_bind(system_toml) else {
        return vec![];
    };
    let port = bind.rsplit(':').next().unwrap_or(&bind);
    let addr = format!("127.0.0.1:{port}");

    vec![
        json!({ "id": "h1",  "label": format!("HTTP/1.1 {addr}"), "addr": addr, "proto": "h1"  }),
        json!({ "id": "h2",  "label": format!("HTTP/2   {addr}"), "addr": addr, "proto": "h2"  }),
        json!({ "id": "h3",  "label": format!("HTTP/3   {addr}"), "addr": addr, "proto": "h3"  }),
        json!({ "id": "all", "label": format!("All      {addr}"), "addr": addr, "proto": "all" }),
    ]
}

/// Start a bench job and hand back its id and the pipe and exit descriptor the
/// loop watches. The running bench is `None` when the binary cannot be started:
/// the job is recorded as failed.
pub fn start_bench(
    bench_bin: &Path,
    jobs: &mut JobStore,
    addr: &str,
    proto: &str,
    duration_secs: u64,
    concurrency: u64,
) -> (String, Option<Running>) {
    let id = uuid::Uuid::new_v4().to_string();
    let job = BenchJob {
        id: id.clone(),
        target: format!("{addr} ({proto})"),
        status: JobStatus::Running,
        output: String::new(),
        started_at: now_secs(),
        finished_at: None,
    };
    jobs.insert(id.clone(), job);

    let mut cmd = Command::new(bench_bin);
    cmd.arg("--addr")
        .arg(addr)
        .arg("--duration")
        .arg(duration_secs.to_string())
        .arg("--concurrency")
        .arg(concurrency.to_string())
        .arg("--skip-verify");

    match proto {
        "h1" => {
            cmd.arg("--http11-only");
        }
        "h2" => {
            cmd.arg("--http2-only");
        }
        "h3" => {
            cmd.arg("--http3-only");
        }
        _ => {} // "all": run all protocols
    }

    cmd.stdout(Stdio::piped());
    match spawn_watched(&mut cmd) {
        Ok((child, exit, output)) => (
            id.clone(),
            Some(Running {
                job_id: id,
                _child: child,
                exit,
                output,
                bytes: Vec::new(),
                eof: false,
                reaped: false,
                succeeded: false,
            }),
        ),
        Err(e) => {
            if let Some(job) = jobs.get_mut(&id) {
                job.output = format!("failed to start m6-bench: {e}");
                job.status = JobStatus::Failed;
                job.finished_at = Some(now_secs());
            }
            (id, None)
        }
    }
}

/// Spawn the child and open its exit descriptor and its non-blocking stdout.
fn spawn_watched(cmd: &mut Command) -> io::Result<(Child, ChildExit, ChildStdout)> {
    let mut child = cmd.spawn()?;
    let exit = ChildExit::new(&child)?;
    let output = child.stdout.take().expect("the child's stdout is piped");
    m6_core::platform::fd::set_nonblocking(&output, true)?;
    Ok((child, exit, output))
}

/// Poll a bench job by id.
/// Returns: { "id": "...", "target": "...", "status": "running"|"done"|"failed",
///            "output": "...", "started_at": 0, "finished_at": null }
pub fn poll_job(jobs: &JobStore, job_id: &str) -> Option<Value> {
    jobs.get(job_id).map(|j| {
        json!({
            "id":          j.id,
            "target":      j.target,
            "status":      match j.status { JobStatus::Running => "running", JobStatus::Done => "done", JobStatus::Failed => "failed" },
            "output":      j.output,
            "started_at":  j.started_at,
            "finished_at": j.finished_at,
        })
    })
}
