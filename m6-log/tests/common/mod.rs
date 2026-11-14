//! Helpers for the tests that replace standard output with a pipe or a file.

#![allow(dead_code)]

use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::Path;

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::unistd::{dup, dup2_stdout, pipe};

/// A saved standard output, put back when this is dropped.
pub struct Stdout {
    saved: OwnedFd,
}

impl Stdout {
    /// Replace stdout with a pipe nobody reads, and return its read end.
    pub fn to_pipe() -> (Stdout, OwnedFd) {
        let (read, write) = pipe().expect("a pipe");
        let stdout = Stdout::save();
        dup2_stdout(&write).expect("stdout is the pipe");
        (stdout, read)
    }

    /// Replace stdout with `path`, created and opened for append.
    pub fn to_file(path: &Path) -> Stdout {
        let file = File::options()
            .create(true)
            .append(true)
            .open(path)
            .expect("the file");
        let stdout = Stdout::save();
        dup2_stdout(&file).expect("stdout is the file");
        stdout
    }

    /// Whether stdout carries `O_NONBLOCK` now.
    pub fn nonblocking() -> bool {
        let flags = fcntl(std::io::stdout(), FcntlArg::F_GETFL).expect("the flags");
        OFlag::from_bits_truncate(flags).contains(OFlag::O_NONBLOCK)
    }

    fn save() -> Stdout {
        Stdout {
            saved: dup(std::io::stdout()).expect("a copy of stdout"),
        }
    }
}

impl Drop for Stdout {
    fn drop(&mut self) {
        let _ = dup2_stdout(&self.saved);
    }
}
