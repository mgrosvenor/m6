#![allow(
    dead_code,
    reason = "one shared helper, and each test binary uses part of it"
)]

//! A `CheckServer` that is already answering, for the process-level fixtures.
//!
//! `CheckServer::serve_one` is caller-driven, so a test that blocks in a TLS
//! client cannot call it. This wrapper moves the fake into one scoped thread
//! that serves until the fixture is dropped. Production code starts no thread:
//! this module is compiled only into test binaries.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use m6_core::auth::Identity;
use m6_core::testkit::{CheckServer, Reply};

/// One fake authority, serving on its own thread until dropped.
pub struct RunningCheck {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RunningCheck {
    /// Bind `path` and answer the table, then serve every connection.
    pub fn start(path: impl Into<PathBuf>, answers: Vec<(String, Reply)>) -> io::Result<Self> {
        let path = path.into();
        let mut server = CheckServer::bind(&path)?;
        for (token, reply) in answers {
            server.answer(&token, reply);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("running-check".to_string())
            .spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    if server.serve_one().is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            path,
            stop,
            worker: Some(worker),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RunningCheck {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept behind `serve_one`; the loop sees the flag and ends.
        let _ = UnixStream::connect(&self.path);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// An identity answer with the given groups and roles.
pub fn identity(groups: &[&str], roles: &[&str]) -> Identity {
    Identity {
        sub: "user-1".to_string(),
        username: "alice".to_string(),
        roles: roles.iter().map(|role| role.to_string()).collect(),
        groups: groups.iter().map(|group| group.to_string()).collect(),
    }
}
