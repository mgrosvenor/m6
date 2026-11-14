//! The Linux implementation of the platform layer. Selected by OS family in
//! `platform.rs`, so it is the same code on x86_64 and arm64.
//!
//! Every `pub(super)` item here has a twin of the same name in `macos.rs`, and
//! `tests/platform_gate.rs` holds the two lists equal. No tests live here: the
//! layer's tests are shared and run unchanged on every target.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use super::host::{
    parse_cgroup_memory, parse_diskstats, parse_loadavg, parse_meminfo, parse_net_dev,
    parse_netstat, parse_pressure, parse_sockstat_into, parse_thermal_millidegrees, parse_uptime,
    DiskIo, LoadAverage, Memory, NetDevice, Pressure, TcpHealth, ThermalZone,
};
use super::Signal;

// ── Signals ──────────────────────────────────────────────────────────────────

/// The three signals as a `signalfd`, non-blocking and close-on-exec.
pub(super) struct Signals {
    fd: nix::sys::signalfd::SignalFd,
}

impl Signals {
    pub(super) fn new() -> io::Result<Signals> {
        use nix::sys::signalfd::{SfdFlags, SignalFd};
        let fd = SignalFd::with_flags(
            &super::managed(),
            SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC,
        )?;
        Ok(Signals { fd })
    }

    pub(super) fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub(super) fn read(&mut self) -> Option<Signal> {
        let info = self.fd.read_signal().ok()??;
        super::signal_from_signo(info.ssi_signo as i32)
    }
}

// ── A child's exit ───────────────────────────────────────────────────────────

/// The child's pid as a `pidfd`, readable when the child exits, at once when
/// it already has.
pub(super) struct ChildExit {
    fd: OwnedFd,
}

impl ChildExit {
    pub(super) fn new(pid: u32) -> io::Result<ChildExit> {
        Ok(ChildExit { fd: pidfd_of(pid)? })
    }

    pub(super) fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

fn pidfd_of(pid: u32) -> io::Result<OwnedFd> {
    let pid = rustix::process::Pid::from_raw(pid as i32)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pid 0 is not a child"))?;
    let fd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())?;
    Ok(fd)
}

/// `pidfd_open` needs Linux 5.3. Probed on this process's own pid.
pub(super) fn child_exit_supported() -> io::Result<()> {
    pidfd_of(std::process::id()).map(drop).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("a child's exit is read from a pidfd, so pidfd_open must work (Linux 5.3 or newer): {e}"),
        )
    })
}

// ── Sockets, paths and limits ────────────────────────────────────────────────

/// `sockaddr_un::sun_path` is 108 bytes on Linux.
pub(super) const MAX_SOCKET_PATH: usize = 108;

/// Nothing to set: the runtime ignores `SIGPIPE`, so a write to a closed peer
/// is already an error.
pub(super) fn prepare_socket(_sock: &socket2::Socket) -> io::Result<()> {
    Ok(())
}

/// The kernel enforces the hard limit as read, so there is no cap below it.
pub(super) fn max_open_files() -> io::Result<Option<u64>> {
    Ok(None)
}

// ── Process facts ────────────────────────────────────────────────────────────

pub(super) fn thread_count(pid: u32) -> io::Result<usize> {
    Ok(std::fs::read_dir(format!("/proc/{pid}/task"))?.count())
}

/// The entries of `/proc/<pid>/fd`. For this process the listing holds its
/// own directory handle, which is not one of the process's descriptors once
/// the count is returned, so it is left out.
pub(super) fn open_descriptors(pid: u32) -> io::Result<usize> {
    let count = std::fs::read_dir(format!("/proc/{pid}/fd"))?.count();
    Ok(if pid == std::process::id() {
        count.saturating_sub(1)
    } else {
        count
    })
}

/// The socket inodes of `/proc/<pid>/fd` against the listening rows (state
/// `0A`) of `/proc/net/tcp` and `tcp6`.
pub(super) fn listens_on_tcp(pid: u32) -> io::Result<bool> {
    let inodes: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))?
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter_map(|l| {
            let l = l.to_string_lossy().into_owned();
            Some(l.strip_prefix("socket:[")?.strip_suffix(']')?.to_string())
        })
        .collect();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for line in std::fs::read_to_string(table)
            .unwrap_or_default()
            .lines()
            .skip(1)
        {
            let f: Vec<&str> = line.split_whitespace().collect();
            // Field 3 is the state (0A is LISTEN) and field 9 the socket inode.
            if f.len() > 9 && f[3] == "0A" && inodes.iter().any(|i| i == f[9]) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

// ── Host readings: the /proc and /sys readers, moved from host.rs ────────────

pub(super) fn net_devices() -> Vec<NetDevice> {
    std::fs::read_to_string("/proc/net/dev")
        .map(|s| parse_net_dev(&s))
        .unwrap_or_default()
}

pub(super) fn disk_io() -> Vec<DiskIo> {
    std::fs::read_to_string("/proc/diskstats")
        .map(|s| parse_diskstats(&s))
        .unwrap_or_default()
}

pub(super) fn pressure(resource: &str) -> Option<Pressure> {
    parse_pressure(&std::fs::read_to_string(format!("/proc/pressure/{resource}")).ok()?)
}

pub(super) fn tcp_health() -> Option<TcpHealth> {
    let mut t = parse_netstat(&std::fs::read_to_string("/proc/net/netstat").ok()?);
    if let Ok(s) = std::fs::read_to_string("/proc/net/sockstat") {
        parse_sockstat_into(&s, &mut t);
    }
    Some(t)
}

pub(super) fn load_average() -> Option<LoadAverage> {
    parse_loadavg(&std::fs::read_to_string("/proc/loadavg").ok()?)
}

/// Memory, preferring the limit that actually applies to this process.
pub(super) fn memory() -> Option<Memory> {
    if let (Ok(max), Ok(cur)) = (
        std::fs::read_to_string("/sys/fs/cgroup/memory.max"),
        std::fs::read_to_string("/sys/fs/cgroup/memory.current"),
    ) {
        if let Some(m) = parse_cgroup_memory(&max, &cur) {
            return Some(m);
        }
    }
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

pub(super) fn thermal_zones() -> Vec<ThermalZone> {
    let mut out = Vec::new();
    let dir = match std::fs::read_dir("/sys/class/thermal") {
        Ok(d) => d,
        Err(_) => return out,
    };
    for entry in dir.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // cooling_device* live here too and have no `temp`; a VPS often has
        // those and no thermal_zone at all.
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let celsius = match std::fs::read_to_string(path.join("temp"))
            .ok()
            .and_then(|s| parse_thermal_millidegrees(&s))
        {
            Some(c) => c,
            None => continue,
        };
        let label = std::fs::read_to_string(path.join("type"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| name.clone());
        out.push(ThermalZone {
            name: label,
            celsius,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub(super) fn uptime() -> Option<std::time::Duration> {
    parse_uptime(&std::fs::read_to_string("/proc/uptime").ok()?)
}
