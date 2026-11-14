//! The macOS implementation of the platform layer. Selected by OS family in
//! `platform.rs`, so it is the same code on arm64 and x86_64.
//!
//! Every `pub(super)` item here has a twin of the same name in `linux.rs`, and
//! `tests/platform_gate.rs` holds the two lists equal. No tests live here: the
//! layer's tests are shared and run unchanged on every target. A port to
//! another kqueue system (FreeBSD, OpenBSD) starts from this file.

use std::io;
use std::os::fd::{AsFd, AsRawFd, RawFd};

use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

use super::host::{
    DiskIo, LoadAverage, Memory, MemorySource, NetDevice, Pressure, TcpHealth, ThermalZone,
};
use super::Signal;

/// A zero timeout, for the drains below.
const NOW: nix::libc::timespec = nix::libc::timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

// ── Signals ──────────────────────────────────────────────────────────────────

/// The three signals as a kqueue registered for `EVFILT_SIGNAL`, which
/// reports a signal when it is posted, whatever its disposition.
///
/// The three are also set to `SIG_IGN`. A kqueue reports a signal and does
/// not dequeue it, so a blocked signal would stay pending for the life of the
/// process and be delivered at its default disposition the moment any thread
/// unblocked it. Ignored, it is reported and then discarded, which is what
/// `signalfd`'s read does on Linux. An ignored disposition survives `exec`, so
/// [`super::process::spawn`] resets it in every child.
pub(super) struct Signals {
    kq: Kqueue,
}

impl Signals {
    pub(super) fn new() -> io::Result<Signals> {
        ignore_default_disposition()?;
        let kq = Kqueue::new()?;
        for sig in super::managed().iter() {
            let ev = KEvent::new(
                sig as usize,
                EventFilter::EVFILT_SIGNAL,
                EvFlags::EV_ADD | EvFlags::EV_ENABLE,
                FilterFlag::empty(),
                0,
                0,
            );
            kq.kevent(std::slice::from_ref(&ev), &mut [], None)?;
        }
        Ok(Signals { kq })
    }

    pub(super) fn raw_fd(&self) -> RawFd {
        self.kq.as_fd().as_raw_fd()
    }

    pub(super) fn read(&mut self) -> Option<Signal> {
        let mut out = KEvent::new(
            0,
            EventFilter::EVFILT_SIGNAL,
            EvFlags::empty(),
            FilterFlag::empty(),
            0,
            0,
        );
        let n = self
            .kq
            .kevent(&[], std::slice::from_mut(&mut out), Some(NOW))
            .ok()?;
        if n == 0 {
            return None;
        }
        super::signal_from_signo(out.ident() as i32)
    }
}

/// Set the three signals to `SIG_IGN`, so a posted signal is reported by the
/// kqueue and discarded. `sigaction` is `unsafe` in nix because a handler
/// could be unsound. `SIG_IGN` installs no handler code, so no signal runs any.
#[allow(unsafe_code)]
fn ignore_default_disposition() -> io::Result<()> {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet};
    let action = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    for sig in super::managed().iter() {
        // SAFETY: `SIG_IGN` runs no handler code, so the disposition is the
        // kernel's own and cannot call back into unsound Rust.
        unsafe { sigaction(sig, &action)? };
    }
    Ok(())
}

// ── A child's exit ───────────────────────────────────────────────────────────

/// A kqueue registered for the child's exit. `EVFILT_PROC` with `NOTE_EXIT`
/// reports a live child. The kernel refuses it with `ESRCH` for a child that
/// has already exited, and then one triggered user event makes the kqueue
/// readable at once, which is what a `pidfd` on a zombie does on Linux.
pub(super) struct ChildExit {
    kq: Kqueue,
}

impl ChildExit {
    pub(super) fn new(pid: u32) -> io::Result<ChildExit> {
        let kq = Kqueue::new()?;
        let exit = KEvent::new(
            pid as usize,
            EventFilter::EVFILT_PROC,
            EvFlags::EV_ADD | EvFlags::EV_ENABLE | EvFlags::EV_ONESHOT,
            FilterFlag::NOTE_EXIT,
            0,
            0,
        );
        match kq.kevent(std::slice::from_ref(&exit), &mut [], None) {
            Ok(_) => {}
            Err(nix::errno::Errno::ESRCH) => {
                let already = KEvent::new(
                    pid as usize,
                    EventFilter::EVFILT_USER,
                    EvFlags::EV_ADD | EvFlags::EV_ENABLE,
                    FilterFlag::NOTE_TRIGGER,
                    0,
                    0,
                );
                kq.kevent(std::slice::from_ref(&already), &mut [], None)?;
            }
            Err(e) => return Err(e.into()),
        }
        Ok(ChildExit { kq })
    }

    pub(super) fn raw_fd(&self) -> RawFd {
        self.kq.as_fd().as_raw_fd()
    }
}

/// Every macOS kernel has kqueue.
pub(super) fn child_exit_supported() -> io::Result<()> {
    Ok(())
}

// ── Sockets, paths and limits ────────────────────────────────────────────────

/// `sockaddr_un::sun_path` is 104 bytes on macOS.
pub(super) const MAX_SOCKET_PATH: usize = 104;

/// `SO_NOSIGPIPE`, so a write to a closed peer is an error and never a
/// signal, whatever the process's disposition. An accepted socket inherits it
/// from its listener.
pub(super) fn prepare_socket(sock: &socket2::Socket) -> io::Result<()> {
    sock.set_nosigpipe(true)
}

/// The hard limit reads as unlimited and the kernel enforces
/// `kern.maxfilesperproc`, so that is the cap.
pub(super) fn max_open_files() -> io::Result<Option<u64>> {
    use sysctl::Sysctl;
    let value = sysctl::Ctl::new("kern.maxfilesperproc")
        .and_then(|c| c.value())
        .map_err(|e| io::Error::other(format!("kern.maxfilesperproc: {e}")))?;
    match value {
        sysctl::CtlValue::Int(n) => Ok(Some(n.max(0) as u64)),
        sysctl::CtlValue::Uint(n) => Ok(Some(u64::from(n))),
        sysctl::CtlValue::Long(n) => Ok(Some(n.max(0) as u64)),
        sysctl::CtlValue::Ulong(n) => Ok(Some(n)),
        other => Err(io::Error::other(format!(
            "kern.maxfilesperproc is not a number: {other:?}"
        ))),
    }
}

// ── Process facts, through libproc ───────────────────────────────────────────

/// The task information of `pid`. `NotFound` for a pid that is gone,
/// `PermissionDenied` for a process that exists and is not ours to read.
fn task_info(pid: u32) -> io::Result<libproc::task_info::TaskAllInfo> {
    libproc::proc_pid::pidinfo::<libproc::task_info::TaskAllInfo>(pid as i32, 0).map_err(|e| {
        if super::process::is_alive(pid) {
            io::Error::new(io::ErrorKind::PermissionDenied, format!("pid {pid}: {e}"))
        } else {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no process with pid {pid}"),
            )
        }
    })
}

pub(super) fn thread_count(pid: u32) -> io::Result<usize> {
    Ok(task_info(pid)?.ptinfo.pti_threadnum.max(0) as usize)
}

/// The open descriptors, listed: `pbi_nfiles` is the table's size, so the
/// list is asked for up to that many and its length is the count.
pub(super) fn open_descriptors(pid: u32) -> io::Result<usize> {
    let table = task_info(pid)?.pbsd.pbi_nfiles as usize;
    let fds = libproc::proc_pid::listpidinfo::<libproc::file_info::ListFDs>(pid as i32, table)
        .map_err(|e| io::Error::other(format!("pid {pid}: {e}")))?;
    Ok(fds.len())
}

/// `lsof`, which every macOS has, listing the pid's listening TCP sockets. A
/// missing `lsof` is the `Err`. libproc exposes a socket's state inside a C
/// union, which would need `unsafe` to read, so the one line of `lsof` stays.
pub(super) fn listens_on_tcp(pid: u32) -> io::Result<bool> {
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-a", "-p", &pid.to_string(), "-iTCP", "-sTCP:LISTEN"])
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.contains("LISTEN")))
}

// ── Host readings, through sysinfo ───────────────────────────────────────────
//
// `sysinfo` with `default-features = false` and the `system` and `network`
// features alone: no `multithread` feature, so no thread pool, which
// `tests/platform_contract.rs` holds. Disk throughput, pressure stall
// averages and thermal zones have no reading without IOKit, so they are
// absent here, and absent is an empty list or `None`, never a row of zeros.

pub(super) fn net_devices() -> Vec<NetDevice> {
    let nets = sysinfo::Networks::new_with_refreshed_list();
    let mut out: Vec<NetDevice> = nets
        .iter()
        .map(|(name, data)| NetDevice {
            name: name.clone(),
            rx_bytes: data.total_received(),
            rx_packets: data.total_packets_received(),
            rx_errs: data.total_errors_on_received(),
            // Drop counters are not exposed by the kernel's interface table.
            rx_drop: 0,
            tx_bytes: data.total_transmitted(),
            tx_packets: data.total_packets_transmitted(),
            tx_errs: data.total_errors_on_transmitted(),
            tx_drop: 0,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub(super) fn disk_io() -> Vec<DiskIo> {
    Vec::new()
}

pub(super) fn pressure(_resource: &str) -> Option<Pressure> {
    None
}

pub(super) fn tcp_health() -> Option<TcpHealth> {
    None
}

pub(super) fn load_average() -> Option<LoadAverage> {
    let l = sysinfo::System::load_average();
    Some(LoadAverage {
        one: l.one,
        five: l.five,
        fifteen: l.fifteen,
        // The kernel's load average carries no runnable or total task count.
        running: 0,
        total: 0,
    })
}

/// Total memory, and available as total less used (active, wired and
/// compressed pages). `sysinfo`'s own available figure saturates to zero
/// under memory pressure, which would read as a host with no memory.
pub(super) fn memory() -> Option<Memory> {
    let mut s = sysinfo::System::new();
    s.refresh_memory();
    let total = s.total_memory();
    if total == 0 {
        return None;
    }
    Some(Memory {
        total_bytes: total,
        available_bytes: total.saturating_sub(s.used_memory()),
        source: MemorySource::Host,
    })
}

pub(super) fn thermal_zones() -> Vec<ThermalZone> {
    Vec::new()
}

pub(super) fn uptime() -> Option<std::time::Duration> {
    Some(std::time::Duration::from_secs(sysinfo::System::uptime()))
}
