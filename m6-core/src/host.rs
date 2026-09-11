//! What the machine underneath a service is doing.
//!
//! Load, memory, disk and temperature. Every m6 deployment runs on a host and
//! every operator wants to know the same things about it, so the *reading*
//! lives here. The *judging* does not: there is no threshold in this module,
//! no notion of "high" or "unhealthy", and nothing that decides when to
//! complain. A monitoring service decides what 80% means; core only says what
//! the number is.
//!
//! Core wants these for itself too, not only to report them. The service loop
//! already sheds load with a 503 when its queue is full; admission that is
//! aware of load average, and a cache sized against the memory limit that
//! actually applies, are runtime concerns rather than reporting ones.
//!
//! # Why this is not three lines of `std::fs::read_to_string`
//!
//! Because the two readings that matter most are the two that are easiest to
//! get subtly wrong, and getting them wrong is invisible:
//!
//! - **Memory inside a container is not the host's memory.** `/proc/meminfo`
//!   reports the machine, not the cgroup limit. A service that sizes a cache
//!   from `MemTotal` inside a container with a 512MB limit will size it for
//!   the host's 64GB, and be OOM-killed at a number that looks healthy in
//!   every log. [`memory`] prefers the cgroup limit when one applies and says
//!   which it used, in [`Memory::source`].
//!
//! - **Free disk is not available disk.** `statvfs` gives both `f_bfree` and
//!   `f_bavail`; the difference is the blocks reserved for root, typically 5%.
//!   Reporting `f_bfree` makes a filesystem that is full for every ordinary
//!   process look like it has room. [`disk`] reports `f_bavail`, which is the
//!   space a service can actually use.
//!
//! # Structure
//!
//! Parsing is separated from reading. `parse_*` are pure functions over the
//! text these files contain and are tested on any platform; the readers are
//! thin and platform-gated. A VPS frequently has no thermal zone at all, so
//! "no reading" is an ordinary answer and is `None`, not an error.

use std::path::Path;

use serde::{Deserialize, Serialize};

// ── Load ─────────────────────────────────────────────────────────────────────

/// Kernel load averages, and how many tasks are behind them.
///
/// Load is a queue length, not a percentage: on an 8 core box a load of 4.0 is
/// half busy, and on a 1 core box it is four deep. Anything comparing this to
/// a threshold needs [`cpu_count`] as well, which is why both are here.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LoadAverage {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
    /// Tasks currently runnable.
    pub running: u32,
    /// Tasks in total.
    pub total: u32,
}

/// Parse `/proc/loadavg`: `0.13 0.13 0.09 1/231 1250464`.
pub fn parse_loadavg(s: &str) -> Option<LoadAverage> {
    let mut it = s.split_whitespace();
    let one = it.next()?.parse().ok()?;
    let five = it.next()?.parse().ok()?;
    let fifteen = it.next()?.parse().ok()?;
    let (running, total) = match it.next() {
        Some(procs) => {
            let mut p = procs.split('/');
            (
                p.next().and_then(|v| v.parse().ok()).unwrap_or(0),
                p.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            )
        }
        None => (0, 0),
    };
    Some(LoadAverage { one, five, fifteen, running, total })
}

/// Logical CPUs available to this process.
///
/// `available_parallelism` respects CPU affinity and cgroup quota where the
/// platform exposes them, which is what a load comparison wants: the number of
/// CPUs this process may actually run on, not the number the machine has.
pub fn cpu_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

// ── Memory ───────────────────────────────────────────────────────────────────

/// Which authority a memory reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemorySource {
    /// `/proc/meminfo`: the machine's memory.
    Host,
    /// A cgroup v2 limit, which is the memory this process may actually use.
    Cgroup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub total_bytes: u64,
    /// Memory obtainable without swapping. On the host this is `MemAvailable`,
    /// which accounts for reclaimable cache; `MemFree` would understate it
    /// badly on any box that has been up long enough to fill its page cache.
    pub available_bytes: u64,
    pub source: MemorySource,
}

impl Memory {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }
    pub fn used_fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        self.used_bytes() as f64 / self.total_bytes as f64
    }
}

/// Parse `/proc/meminfo`. Values are in kB despite the kernel's `kB` label
/// meaning KiB, so they are multiplied by 1024.
pub fn parse_meminfo(s: &str) -> Option<Memory> {
    let mut total = None;
    let mut available = None;
    let mut free = None;
    for line in s.lines() {
        let (key, rest) = line.split_once(':')?;
        let kb: u64 = match rest.split_whitespace().next().and_then(|v| v.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        match key {
            "MemTotal" => total = Some(kb * 1024),
            "MemAvailable" => available = Some(kb * 1024),
            "MemFree" => free = Some(kb * 1024),
            _ => {}
        }
    }
    // MemAvailable has been present since Linux 3.14. Falling back to MemFree
    // understates what is obtainable, so it is a last resort rather than a
    // silent equivalent.
    Some(Memory {
        total_bytes: total?,
        available_bytes: available.or(free)?,
        source: MemorySource::Host,
    })
}

/// Interpret cgroup v2 `memory.max` and `memory.current`.
///
/// `memory.max` is the literal string `max` when no limit applies, which is
/// the case for the root cgroup on an ordinary VM. Returning `None` then is
/// correct and means "ask the host instead", not "failed to read".
pub fn parse_cgroup_memory(max: &str, current: &str) -> Option<Memory> {
    let max = max.trim();
    if max == "max" {
        return None;
    }
    let total: u64 = max.parse().ok()?;
    let used: u64 = current.trim().parse().ok()?;
    Some(Memory {
        total_bytes: total,
        available_bytes: total.saturating_sub(used),
        source: MemorySource::Cgroup,
    })
}

// ── Disk ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    pub total_bytes: u64,
    /// Space an unprivileged process can use: `f_bavail`, not `f_bfree`. The
    /// gap between them is the root reserve, usually 5%, and reporting the
    /// larger number makes a filesystem that is full for every service on the
    /// box look like it has room.
    pub available_bytes: u64,
}

impl Disk {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }
    pub fn used_fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        self.used_bytes() as f64 / self.total_bytes as f64
    }
}

/// Space on the filesystem containing `path`.
pub fn disk(path: &Path) -> Option<Disk> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` writes into a zeroed struct we own, and `c` is a valid
    // NUL-terminated path for the duration of the call.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        // f_frsize is the fragment size and is the unit f_blocks and f_bavail
        // are counted in. f_bsize is the preferred I/O block size and is not
        // the same thing, though they usually match.
        let unit = if st.f_frsize > 0 { st.f_frsize } else { st.f_bsize } as u64;
        Some(Disk {
            total_bytes: st.f_blocks as u64 * unit,
            available_bytes: st.f_bavail as u64 * unit,
        })
    }
}

// ── Temperature ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalZone {
    pub name: String,
    pub celsius: f32,
}

/// Parse a `/sys/class/thermal/thermal_zone*/temp` value, which is
/// millidegrees Celsius.
pub fn parse_thermal_millidegrees(s: &str) -> Option<f32> {
    let milli: i64 = s.trim().parse().ok()?;
    Some(milli as f32 / 1000.0)
}

// ── Uptime ───────────────────────────────────────────────────────────────────

/// Parse `/proc/uptime`: seconds up, seconds idle across all cores.
pub fn parse_uptime(s: &str) -> Option<std::time::Duration> {
    let secs: f64 = s.split_whitespace().next()?.parse().ok()?;
    Some(std::time::Duration::from_secs_f64(secs))
}


// ── Network interfaces ───────────────────────────────────────────────────────

/// One interface's counters, from `/proc/net/dev`.
///
/// `rx_errs`, `rx_drop`, `tx_errs` and `tx_drop` are the reason this is here.
/// Byte counters say how busy the box is, which is interesting; drops say the
/// kernel threw traffic away, which is a cause. Latency that looks like a slow
/// application is sometimes a NIC discarding packets, and that is invisible
/// from inside the request path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetDevice {
    pub name: String,
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errs: u64,
    pub rx_drop: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errs: u64,
    pub tx_drop: u64,
}

impl NetDevice {
    /// Anything the kernel refused to carry, in either direction.
    pub fn discarded(&self) -> u64 {
        self.rx_errs + self.rx_drop + self.tx_errs + self.tx_drop
    }
}

/// Parse `/proc/net/dev`.
///
/// Loopback is skipped: it is always busy, never a fault, and reporting it
/// next to a real interface invites reading its traffic as external.
pub fn parse_net_dev(s: &str) -> Vec<NetDevice> {
    let mut out = Vec::new();
    for line in s.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name == "lo" {
            continue;
        }
        let f: Vec<u64> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        if f.len() < 16 {
            continue;
        }
        out.push(NetDevice {
            name: name.to_string(),
            rx_bytes: f[0], rx_packets: f[1], rx_errs: f[2], rx_drop: f[3],
            tx_bytes: f[8], tx_packets: f[9], tx_errs: f[10], tx_drop: f[11],
        });
    }
    out
}

// ── Disk throughput ──────────────────────────────────────────────────────────

/// One block device's I/O counters, from `/proc/diskstats`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskIo {
    pub name: String,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub reads: u64,
    pub writes: u64,
    /// Milliseconds with at least one I/O in flight. The saturation signal:
    /// against wall-clock time it is a utilisation percentage, and a device
    /// pinned near 100% is a device that is the bottleneck.
    pub io_ms: u64,
    pub in_flight: u64,
}

/// `/proc/diskstats` counts in 512-byte sectors regardless of the device's
/// actual block size. This is a kernel ABI constant, not a property of the
/// disk, and treating it as the latter gives numbers wrong by a factor of 8
/// on a 4K-sector device.
const SECTOR_BYTES: u64 = 512;

/// Parse `/proc/diskstats`, keeping whole devices.
///
/// Partitions (`vda1`) are skipped: their counters are a subset of their
/// parent's, so including both double-counts every byte.
pub fn parse_diskstats(s: &str) -> Vec<DiskIo> {
    let mut out = Vec::new();
    for line in s.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 14 {
            continue;
        }
        let name = f[2];
        // Whole devices only. A trailing digit on a vd*/sd*/nvme partition
        // means a slice of a device already counted.
        let is_partition = name
            .chars()
            .last()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
            && !name.starts_with("nvme");
        if is_partition || name.starts_with("loop") || name.starts_with("ram") {
            continue;
        }
        let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        out.push(DiskIo {
            name: name.to_string(),
            reads: n(3),
            read_bytes: n(5) * SECTOR_BYTES,
            writes: n(7),
            written_bytes: n(9) * SECTOR_BYTES,
            in_flight: n(11),
            io_ms: n(12),
        });
    }
    out
}

// ── Pressure stall information ───────────────────────────────────────────────

/// One PSI resource: the share of time work was stalled waiting for it.
///
/// More honest than load average, and the reason both are here. Load counts
/// runnable tasks and says nothing about why; PSI says "work was blocked for
/// this fraction of the last ten seconds, on this resource". `full` is time
/// when *everything* was stalled, which on a single-purpose box is the number
/// that matters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Pressure {
    pub some_avg10: f32,
    pub some_avg60: f32,
    pub full_avg10: f32,
    pub full_avg60: f32,
}

/// Parse one `/proc/pressure/*` file.
pub fn parse_pressure(s: &str) -> Option<Pressure> {
    let mut p = Pressure::default();
    let mut seen = false;
    for line in s.lines() {
        let full = line.starts_with("full");
        if !full && !line.starts_with("some") {
            continue;
        }
        seen = true;
        for tok in line.split_whitespace() {
            let Some((k, v)) = tok.split_once('=') else { continue };
            let Ok(v) = v.parse::<f32>() else { continue };
            match (full, k) {
                (false, "avg10") => p.some_avg10 = v,
                (false, "avg60") => p.some_avg60 = v,
                (true, "avg10") => p.full_avg10 = v,
                (true, "avg60") => p.full_avg60 = v,
                _ => {}
            }
        }
    }
    seen.then_some(p)
}

// ── TCP health ───────────────────────────────────────────────────────────────

/// Kernel-side connection health, from `/proc/net/netstat` and
/// `/proc/net/sockstat`.
///
/// `listen_overflows` is the one to read first. It counts connections dropped
/// because the accept queue was full, which is backpressure the application
/// never sees: those clients got nothing, and m6's own 503 path does not know
/// they existed. A server can look completely healthy in its own logs while
/// this climbs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TcpHealth {
    pub listen_overflows: u64,
    pub listen_drops: u64,
    pub syn_retrans: u64,
    pub sockets_used: u64,
    pub tcp_inuse: u64,
    pub tcp_time_wait: u64,
    pub tcp_orphan: u64,
}

/// Parse `/proc/net/netstat`, which alternates a header line of names and a
/// line of values under the same prefix.
pub fn parse_netstat(s: &str) -> TcpHealth {
    let mut t = TcpHealth::default();
    let lines: Vec<&str> = s.lines().collect();
    for pair in lines.windows(2) {
        let (names, values) = (pair[0], pair[1]);
        let Some((np, nrest)) = names.split_once(':') else { continue };
        let Some((vp, vrest)) = values.split_once(':') else { continue };
        if np != vp {
            continue;
        }
        for (k, v) in nrest.split_whitespace().zip(vrest.split_whitespace()) {
            let Ok(v) = v.parse::<u64>() else { continue };
            match k {
                "ListenOverflows" => t.listen_overflows = v,
                "ListenDrops" => t.listen_drops = v,
                "TCPSynRetrans" => t.syn_retrans = v,
                _ => {}
            }
        }
    }
    t
}

/// Merge `/proc/net/sockstat` counts into an existing [`TcpHealth`].
pub fn parse_sockstat_into(s: &str, t: &mut TcpHealth) {
    for line in s.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let get = |key: &str| -> Option<u64> {
            f.iter().position(|x| *x == key).and_then(|i| f.get(i + 1)).and_then(|v| v.parse().ok())
        };
        if line.starts_with("sockets:") {
            t.sockets_used = get("used").unwrap_or(0);
        } else if line.starts_with("TCP:") {
            t.tcp_inuse = get("inuse").unwrap_or(0);
            t.tcp_time_wait = get("tw").unwrap_or(0);
            t.tcp_orphan = get("orphan").unwrap_or(0);
        }
    }
}

// ── File descriptors ─────────────────────────────────────────────────────────

/// This process's open file descriptors against its own limit.
///
/// A server that runs out of descriptors stops accepting connections and the
/// reason is not obvious from anywhere else: the failure is `EMFILE` deep in
/// an accept loop, and what the operator sees is a site that stopped
/// answering. Reporting the headroom makes it a number that can be watched
/// instead of an outage to be diagnosed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDescriptors {
    pub open: u64,
    pub soft_limit: u64,
    pub hard_limit: u64,
}

impl FileDescriptors {
    pub fn used_fraction(&self) -> f64 {
        if self.soft_limit == 0 {
            return 0.0;
        }
        self.open as f64 / self.soft_limit as f64
    }
}

/// Parse the `Max open files` row of `/proc/self/limits`.
pub fn parse_fd_limits(s: &str) -> Option<(u64, u64)> {
    for line in s.lines() {
        if !line.starts_with("Max open files") {
            continue;
        }
        let rest = line.trim_start_matches("Max open files").trim();
        let f: Vec<&str> = rest.split_whitespace().collect();
        let parse = |v: &str| -> u64 {
            if v == "unlimited" { u64::MAX } else { v.parse().unwrap_or(0) }
        };
        return Some((parse(f.first().copied().unwrap_or("0")),
                     parse(f.get(1).copied().unwrap_or("0"))));
    }
    None
}

// ── Readers ──────────────────────────────────────────────────────────────────

/// A snapshot of the host, with every field optional because every field can
/// legitimately be unavailable.
///
/// A VPS commonly exposes no thermal zone, a non-Linux host exposes none of
/// `/proc`, and none of that is an error. A monitoring service reports what it
/// got and says nothing about what it did not, rather than reporting a zero
/// that reads as a healthy measurement.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostSnapshot {
    pub load: Option<LoadAverage>,
    pub cpus: usize,
    pub memory: Option<Memory>,
    pub disk: Option<Disk>,
    pub thermal: Vec<ThermalZone>,
    pub uptime_s: Option<u64>,
    /// Interface counters. Empty off Linux.
    #[serde(default)]
    pub net: Vec<NetDevice>,
    /// Block device throughput. Empty off Linux.
    #[serde(default)]
    pub disks: Vec<DiskIo>,
    /// Pressure stall, per resource. Absent on kernels without PSI.
    #[serde(default)]
    pub cpu_pressure: Option<Pressure>,
    #[serde(default)]
    pub io_pressure: Option<Pressure>,
    #[serde(default)]
    pub memory_pressure: Option<Pressure>,
    #[serde(default)]
    pub tcp: Option<TcpHealth>,
    #[serde(default)]
    pub fds: Option<FileDescriptors>,
}

/// Read everything available about the host.
///
/// `disk_path` selects the filesystem to report; pass the one the service
/// actually writes to, which is not necessarily `/`.
pub fn snapshot(disk_path: &Path) -> HostSnapshot {
    HostSnapshot {
        load: load_average(),
        cpus: cpu_count(),
        memory: memory(),
        disk: disk(disk_path),
        thermal: thermal_zones(),
        uptime_s: uptime().map(|d| d.as_secs()),
        net: net_devices(),
        disks: disk_io(),
        cpu_pressure: pressure("cpu"),
        io_pressure: pressure("io"),
        memory_pressure: pressure("memory"),
        tcp: tcp_health(),
        fds: file_descriptors(),
    }
}

#[cfg(target_os = "linux")]
pub fn net_devices() -> Vec<NetDevice> {
    std::fs::read_to_string("/proc/net/dev").map(|s| parse_net_dev(&s)).unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
pub fn net_devices() -> Vec<NetDevice> { Vec::new() }

#[cfg(target_os = "linux")]
pub fn disk_io() -> Vec<DiskIo> {
    std::fs::read_to_string("/proc/diskstats").map(|s| parse_diskstats(&s)).unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
pub fn disk_io() -> Vec<DiskIo> { Vec::new() }

#[cfg(target_os = "linux")]
pub fn pressure(resource: &str) -> Option<Pressure> {
    parse_pressure(&std::fs::read_to_string(format!("/proc/pressure/{resource}")).ok()?)
}

#[cfg(not(target_os = "linux"))]
pub fn pressure(_resource: &str) -> Option<Pressure> { None }

#[cfg(target_os = "linux")]
pub fn tcp_health() -> Option<TcpHealth> {
    let mut t = parse_netstat(&std::fs::read_to_string("/proc/net/netstat").ok()?);
    if let Ok(s) = std::fs::read_to_string("/proc/net/sockstat") {
        parse_sockstat_into(&s, &mut t);
    }
    Some(t)
}

#[cfg(not(target_os = "linux"))]
pub fn tcp_health() -> Option<TcpHealth> { None }

#[cfg(target_os = "linux")]
pub fn file_descriptors() -> Option<FileDescriptors> {
    let open = std::fs::read_dir("/proc/self/fd").ok()?.count() as u64;
    let (soft, hard) = parse_fd_limits(&std::fs::read_to_string("/proc/self/limits").ok()?)?;
    Some(FileDescriptors { open, soft_limit: soft, hard_limit: hard })
}

#[cfg(not(target_os = "linux"))]
pub fn file_descriptors() -> Option<FileDescriptors> { None }

#[cfg(target_os = "linux")]
pub fn load_average() -> Option<LoadAverage> {
    parse_loadavg(&std::fs::read_to_string("/proc/loadavg").ok()?)
}

#[cfg(not(target_os = "linux"))]
pub fn load_average() -> Option<LoadAverage> {
    // SAFETY: getloadavg writes at most 3 doubles into a buffer we own.
    unsafe {
        let mut avg = [0f64; 3];
        if libc::getloadavg(avg.as_mut_ptr(), 3) != 3 {
            return None;
        }
        Some(LoadAverage {
            one: avg[0],
            five: avg[1],
            fifteen: avg[2],
            running: 0,
            total: 0,
        })
    }
}

/// Memory, preferring the limit that actually applies to this process.
#[cfg(target_os = "linux")]
pub fn memory() -> Option<Memory> {
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

#[cfg(not(target_os = "linux"))]
pub fn memory() -> Option<Memory> {
    None
}

#[cfg(target_os = "linux")]
pub fn thermal_zones() -> Vec<ThermalZone> {
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
        out.push(ThermalZone { name: label, celsius });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(not(target_os = "linux"))]
pub fn thermal_zones() -> Vec<ThermalZone> {
    Vec::new()
}

#[cfg(target_os = "linux")]
pub fn uptime() -> Option<std::time::Duration> {
    parse_uptime(&std::fs::read_to_string("/proc/uptime").ok()?)
}

#[cfg(not(target_os = "linux"))]
pub fn uptime() -> Option<std::time::Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real output from syd, 2026-09-11.
    #[test]
    fn parses_a_real_loadavg() {
        let l = parse_loadavg("0.13 0.13 0.09 1/231 1250464\n").unwrap();
        assert_eq!(l.one, 0.13);
        assert_eq!(l.five, 0.13);
        assert_eq!(l.fifteen, 0.09);
        assert_eq!(l.running, 1);
        assert_eq!(l.total, 231);
    }

    /// Real output from syd. These are 950MB boxes, so the numbers matter.
    #[test]
    fn parses_a_real_meminfo_and_prefers_available_over_free() {
        let s = "MemTotal:         973352 kB\n\
                 MemFree:           24992 kB\n\
                 MemAvailable:     627720 kB\n\
                 Buffers:           36416 kB\n\
                 Cached:           559804 kB\n";
        let m = parse_meminfo(s).unwrap();
        assert_eq!(m.total_bytes, 973352 * 1024);
        assert_eq!(
            m.available_bytes,
            627720 * 1024,
            "MemFree would report 24MB available on a box with 613MB obtainable"
        );
        assert_eq!(m.source, MemorySource::Host);
        assert!((m.used_fraction() - 0.355).abs() < 0.01);
    }

    /// A box that has been up long enough to fill its page cache has almost no
    /// MemFree and plenty of MemAvailable. Reading MemFree reports a memory
    /// emergency on a healthy machine.
    #[test]
    fn mem_free_is_only_a_fallback() {
        let with_available = parse_meminfo(
            "MemTotal: 1000 kB\nMemFree: 10 kB\nMemAvailable: 800 kB\n",
        )
        .unwrap();
        assert_eq!(with_available.available_bytes, 800 * 1024);

        let without = parse_meminfo("MemTotal: 1000 kB\nMemFree: 10 kB\n").unwrap();
        assert_eq!(without.available_bytes, 10 * 1024);
    }

    /// The root cgroup on an ordinary VM, which is what syd, lon and chi are.
    /// `max` means no limit, and the answer is "ask the host", not an error.
    #[test]
    fn an_unlimited_cgroup_defers_to_the_host() {
        assert!(parse_cgroup_memory("max\n", "12345\n").is_none());
    }

    /// The case the module exists for: inside a container the host's figure is
    /// the wrong one, and a cache sized from it gets the process OOM-killed at
    /// a number that looks fine.
    #[test]
    fn a_limited_cgroup_wins_and_says_so() {
        let m = parse_cgroup_memory("536870912\n", "134217728\n").unwrap();
        assert_eq!(m.total_bytes, 512 * 1024 * 1024);
        assert_eq!(m.available_bytes, 384 * 1024 * 1024);
        assert_eq!(m.source, MemorySource::Cgroup);
        assert!((m.used_fraction() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn cgroup_usage_above_the_limit_does_not_underflow() {
        let m = parse_cgroup_memory("1000", "1200").unwrap();
        assert_eq!(m.available_bytes, 0);
    }

    #[test]
    fn thermal_is_millidegrees() {
        assert_eq!(parse_thermal_millidegrees("52000\n"), Some(52.0));
        assert_eq!(parse_thermal_millidegrees("-1000"), Some(-1.0));
        assert_eq!(parse_thermal_millidegrees("not a number"), None);
    }

    #[test]
    fn parses_a_real_uptime() {
        let d = parse_uptime("694521.76 661067.40\n").unwrap();
        assert_eq!(d.as_secs(), 694521);
    }

    #[test]
    fn malformed_input_yields_none_not_zero() {
        assert!(parse_loadavg("").is_none());
        assert!(parse_loadavg("nonsense").is_none());
        assert!(parse_meminfo("").is_none());
        assert!(parse_meminfo("MemFree: 10 kB\n").is_none(), "no MemTotal, no answer");
        assert!(parse_uptime("").is_none());
    }

    /// Disk is read through statvfs and works on any unix, so this one runs
    /// against the real filesystem wherever the tests run.
    #[test]
    fn disk_reports_available_not_free() {
        let d = disk(Path::new("/")).expect("statvfs on / should work");
        assert!(d.total_bytes > 0);
        assert!(d.available_bytes <= d.total_bytes);
        assert!(d.used_fraction() >= 0.0 && d.used_fraction() <= 1.0);
        assert!(disk(Path::new("/no/such/path/here")).is_none());
    }

    /// Every field is allowed to be absent, and a snapshot of a machine with
    /// no thermal zone is an ordinary snapshot rather than a failure.
    #[test]
    fn a_snapshot_is_honest_about_what_it_could_not_read() {
        let s = snapshot(Path::new("/"));
        assert!(s.cpus >= 1);
        assert!(s.disk.is_some(), "statvfs works everywhere the tests run");
        // load/memory/thermal/uptime are all legitimately None off Linux.
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"cpus\""));
    }
}

#[cfg(test)]
mod system_tests {
    use super::*;

    /// Real rows from syd, 2026-09-11.
    #[test]
    fn parses_real_net_dev_and_skips_loopback() {
        let s = "Inter-|   Receive                                                |  Transmit\n\
                 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
    lo: 10911902   11248    0    0    0     0          0         0 10911902   11248    0    0    0     0       0          0\n\
enp1s0: 2929071615 3102200    0    0    0     0          0         0 3105995619 2471836    0    0    0     0       0          0\n";
        let d = parse_net_dev(s);
        assert_eq!(d.len(), 1, "loopback is always busy and never a fault");
        assert_eq!(d[0].name, "enp1s0");
        assert_eq!(d[0].rx_bytes, 2_929_071_615);
        assert_eq!(d[0].rx_packets, 3_102_200);
        assert_eq!(d[0].tx_bytes, 3_105_995_619);
        assert_eq!(d[0].discarded(), 0);
    }

    /// The counters that matter are the ones that are usually zero.
    #[test]
    fn drops_and_errors_are_picked_up_from_the_right_columns() {
        let s = "h\nh\n eth0: 100 10 1 2 0 0 0 0 200 20 3 4 0 0 0 0\n";
        let d = parse_net_dev(s);
        assert_eq!((d[0].rx_errs, d[0].rx_drop), (1, 2));
        assert_eq!((d[0].tx_errs, d[0].tx_drop), (3, 4));
        assert_eq!(d[0].discarded(), 10);
    }

    /// A real `vda` row. Sectors are 512 bytes by kernel ABI whatever the
    /// device's real block size, so a 4K-sector disk read as 4096 would be
    /// reported eight times too large.
    #[test]
    fn parses_real_diskstats_in_512_byte_sectors() {
        let s = " 253       0 vda 29480462 17908798 1806624911 7728100 8813910 37472146 512578074 14928994 0 3407769 22701203 92366 0 3092727720 20811 292631 23296\n";
        let d = parse_diskstats(s);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "vda");
        assert_eq!(d[0].reads, 29_480_462);
        assert_eq!(d[0].read_bytes, 1_806_624_911 * 512);
        assert_eq!(d[0].written_bytes, 512_578_074 * 512);
        assert_eq!(d[0].io_ms, 3_407_769);
        assert_eq!(d[0].in_flight, 0);
    }

    /// Partition counters are a subset of their parent's. Counting both
    /// double-counts every byte the device ever moved.
    #[test]
    fn partitions_and_pseudo_devices_are_skipped() {
        let s = " 253 0 vda 1 0 100 0 1 0 200 0 0 5 0 0 0 0 0 0 0\n\
                  253 1 vda1 1 0 100 0 1 0 200 0 0 5 0 0 0 0 0 0 0\n\
                  7 0 loop0 1 0 8 0 0 0 0 0 0 1 0 0 0 0 0 0 0\n";
        let d = parse_diskstats(s);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "vda");
    }

    /// Real PSI output from syd.
    #[test]
    fn parses_real_pressure() {
        let p = parse_pressure(
            "some avg10=0.20 avg60=0.48 avg300=0.31 total=5618265699\n\
             full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .unwrap();
        assert!((p.some_avg10 - 0.20).abs() < 1e-6);
        assert!((p.some_avg60 - 0.48).abs() < 1e-6);
        assert_eq!(p.full_avg10, 0.0);
        // A kernel without PSI has no file at all, which is None, not zero.
        assert!(parse_pressure("").is_none());
    }

    /// `ListenOverflows` is backpressure the application never sees: those
    /// clients were dropped before accept, so nothing in m6's own logs knows
    /// they existed.
    #[test]
    fn parses_netstat_name_value_pairs() {
        let s = "TcpExt: SyncookiesSent ListenOverflows ListenDrops TCPSynRetrans\n\
                 TcpExt: 0 7 9 42\n\
                 IpExt: InOctets OutOctets\n\
                 IpExt: 100 200\n";
        let t = parse_netstat(s);
        assert_eq!(t.listen_overflows, 7);
        assert_eq!(t.listen_drops, 9);
        assert_eq!(t.syn_retrans, 42);
    }

    /// Mismatched header and value prefixes must not be zipped together, or
    /// every number is attributed to the wrong name.
    #[test]
    fn netstat_does_not_zip_across_different_sections() {
        let s = "TcpExt: ListenOverflows\nIpExt: 999\n";
        assert_eq!(parse_netstat(s).listen_overflows, 0);
    }

    #[test]
    fn parses_real_sockstat() {
        let mut t = TcpHealth::default();
        parse_sockstat_into(
            "sockets: used 229\nTCP: inuse 9 orphan 0 tw 0 alloc 10 mem 16\nUDP: inuse 5 mem 0\n",
            &mut t,
        );
        assert_eq!(t.sockets_used, 229);
        assert_eq!(t.tcp_inuse, 9);
        assert_eq!(t.tcp_time_wait, 0);
    }

    /// The real limits row from m6-http on syd: a 1024 soft limit against a
    /// 524288 hard one. The soft limit is what `accept` fails against.
    #[test]
    fn parses_fd_limits_and_keeps_soft_and_hard_apart() {
        let s = "Limit                     Soft Limit           Hard Limit           Units\n\
                 Max open files            1024                 524288               files\n";
        let (soft, hard) = parse_fd_limits(s).unwrap();
        assert_eq!(soft, 1024);
        assert_eq!(hard, 524288);

        let f = FileDescriptors { open: 512, soft_limit: soft, hard_limit: hard };
        assert!((f.used_fraction() - 0.5).abs() < 1e-9, "headroom is against the SOFT limit");
    }

    #[test]
    fn unlimited_fds_do_not_parse_as_zero() {
        let s = "Max open files            unlimited            unlimited            files\n";
        let (soft, _) = parse_fd_limits(s).unwrap();
        assert_eq!(soft, u64::MAX);
        assert_eq!(parse_fd_limits("Max locked memory 8388608 8388608 bytes\n"), None);
    }

    /// Everything is optional and a snapshot on a machine missing any of it
    /// is an ordinary snapshot, not a failure.
    #[test]
    fn a_snapshot_serialises_with_whatever_it_could_read() {
        let s = snapshot(Path::new("/"));
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"cpus\""));
        assert!(json.contains("\"net\""));
        assert!(json.contains("\"disks\""));
    }
}
