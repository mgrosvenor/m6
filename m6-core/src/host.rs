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
    }
}

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
