//! The Linux implementation of the platform layer. Selected by OS family in
//! `platform.rs`, so it is the same code on x86_64 and arm64.
//!
//! Every `pub(super)` item here has a twin of the same name in `macos.rs`, and
//! `tests/platform_gate.rs` holds the two lists equal. No tests live here: the
//! layer's tests are shared and run unchanged on every target.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::Path;

use super::facts::{self, ClockFacts, CpuFacts, CpuLimit, Fact, Limits, MemoryFacts, OsFacts};
use super::host::{
    parse_cgroup_memory, parse_diskstats, parse_loadavg, parse_meminfo, parse_net_dev,
    parse_netstat, parse_pressure, parse_sockstat_into, parse_thermal_millidegrees, parse_uptime,
    DiskIo, LoadAverage, Memory, NetDevice, Pressure, TcpHealth, ThermalZone,
};
use super::process::ProcessMemory;
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
        loop {
            let info = self.fd.read_signal().ok()??;
            if let Some(signal) = super::signal_from_signo(info.ssi_signo as i32) {
                return Some(signal);
            }
        }
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

/// Ask the kernel to report the socket writable only while its send queue holds
/// fewer than `bytes`, so one response's body is not all handed to the kernel
/// at once and the order m6 frames it in is the order it leaves.
pub(super) fn limit_send_queue(fd: RawFd, bytes: usize) -> io::Result<()> {
    let lowat = bytes as nix::libc::c_int;
    // SAFETY: `fd` is the caller's live socket, and the option reads exactly
    // the one `c_int` it is given.
    let rc = unsafe {
        nix::libc::setsockopt(
            fd,
            nix::libc::IPPROTO_TCP,
            nix::libc::TCP_NOTSENT_LOWAT,
            std::ptr::from_ref(&lowat).cast(),
            std::mem::size_of::<nix::libc::c_int>() as nix::libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The classic BPF program [`steer_reuseport`] attaches: 20 instructions.
///
/// It reads one thing, the client's source address, from the packet the
/// kernel is choosing a socket for, and returns (a hash of that address) mod
/// `group_size`, the index of the socket in the reuseport group. The packet is
/// a TCP SYN or a UDP datagram, and the program does not care which: it reads
/// from the start of the IP header through the `SKF_NET_OFF` negative offsets,
/// which the kernel resolves against the packet's network header whatever
/// transport header follows. The top nibble of byte 0 says IPv4 (version 4:
/// the source is the word at byte 12) or IPv6 (the source is the four words at
/// bytes 8, 12, 16 and 20, XORed into one). The word is then folded
/// (`a ^ (a >> 16)`) so the low bits, which the modulus keeps, see the whole
/// address. The same address gives the same index every time, so every
/// connection and every datagram from one client reaches one socket.
///
/// An index at or past the number of sockets in the group is ignored by the
/// kernel, which then picks by its own hash.
fn steer_program(group_size: u32) -> [nix::libc::sock_filter; 20] {
    // Offsets from the start of the network header.
    const NET: u32 = 0xfff0_0000; // SKF_NET_OFF, -0x100000 as the u32 the kernel reads
    const LD_B: u16 = 0x30; // BPF_LD | BPF_B | BPF_ABS
    const LD_W: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const RSH: u16 = 0x74; // BPF_ALU | BPF_RSH | BPF_K
    const XOR_X: u16 = 0xac; // BPF_ALU | BPF_XOR | BPF_X
    const MOD: u16 = 0x94; // BPF_ALU | BPF_MOD | BPF_K
    const JEQ: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const JA: u16 = 0x05; // BPF_JMP | BPF_JA
    const TAX: u16 = 0x07; // BPF_MISC | BPF_TAX
    const RET_A: u16 = 0x16; // BPF_RET | BPF_A
    let i = |code, jt, jf, k| nix::libc::sock_filter { code, jt, jf, k };
    [
        i(LD_B, 0, 0, NET),       // 0: A = first byte of the IP header
        i(RSH, 0, 0, 4),          // 1: A = IP version
        i(JEQ, 0, 2, 4),          // 2: version 4 falls through, else go to 5
        i(LD_W, 0, 0, NET + 12),  // 3: IPv4 source address
        i(JA, 0, 0, 10),          // 4: go to 15
        i(LD_W, 0, 0, NET + 8),   // 5: IPv6 source, word 0
        i(TAX, 0, 0, 0),          // 6
        i(LD_W, 0, 0, NET + 12),  // 7: word 1
        i(XOR_X, 0, 0, 0),        // 8
        i(TAX, 0, 0, 0),          // 9
        i(LD_W, 0, 0, NET + 16),  // 10: word 2
        i(XOR_X, 0, 0, 0),        // 11
        i(TAX, 0, 0, 0),          // 12
        i(LD_W, 0, 0, NET + 20),  // 13: word 3
        i(XOR_X, 0, 0, 0),        // 14
        i(TAX, 0, 0, 0),          // 15: fold the word into its low bits
        i(RSH, 0, 0, 16),         // 16
        i(XOR_X, 0, 0, 0),        // 17
        i(MOD, 0, 0, group_size), // 18: index = hash mod the group size
        i(RET_A, 0, 0, 0),        // 19
    ]
}

/// Make every connection and datagram from one client address reach one socket
/// of the reuseport group `fd` is in, by attaching [`steer_program`]
/// (`SO_ATTACH_REUSEPORT_CBPF`). The program belongs to the group, so one
/// attach by any member covers all of them, and each process that joins
/// attaches the same program again. `fd` must already be bound (UDP) or
/// listening (TCP), which is when it joins the group. The program lives on the
/// stack: nothing is allocated.
pub(super) fn steer_reuseport(fd: RawFd, group_size: u32) -> io::Result<()> {
    let mut program = steer_program(group_size);
    let fprog = nix::libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_mut_ptr(),
    };
    // SAFETY: `fd` is the caller's live socket, and the option reads the one
    // `sock_fprog` it is given, whose `filter` points at `len` instructions
    // that outlive the call. The kernel copies them.
    let rc = unsafe {
        nix::libc::setsockopt(
            fd,
            nix::libc::SOL_SOCKET,
            nix::libc::SO_ATTACH_REUSEPORT_CBPF,
            std::ptr::from_ref(&fprog).cast(),
            std::mem::size_of::<nix::libc::sock_fprog>() as nix::libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The kernel enforces the hard limit as read, so there is no cap below it.
pub(super) fn max_open_files() -> io::Result<Option<u64>> {
    Ok(None)
}

/// The error a refused pin returns: it names the limit and both ways to fix it.
fn lock_memory_refused(reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{reason}. Pinning memory needs RLIMIT_MEMLOCK unlimited. \
             Under systemd set LimitMEMLOCK=infinity in the unit, otherwise \
             `ulimit -l unlimited` as root. Or set [server] lock_memory = false to run unpinned"
        ),
    )
}

/// Pin every page this process has faulted, and every page it faults later:
/// `mlockall(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT)`.
///
/// `MCL_ONFAULT` is the difference between pinning the pages a process holds and
/// pinning every page it has a mapping for. Without it, `MCL_CURRENT` faults in
/// every current VMA and `MCL_FUTURE` marks each mapping made later as one to be
/// populated. mimalloc reserves 1 GiB of address space by default
/// (`MI_DEFAULT_ARENA_RESERVE`, 1024*1024 KiB, in its `arena.c`), and a service's
/// own `[limits.heap_reserve]` is another 192 MiB, so a process that had mapped
/// 1.2 GiB and touched 96 MB of it became 1.2 GiB resident, on transparent huge
/// pages, in one `mlockall` call: the pages were touched by the lock, not by the
/// program. With `MCL_ONFAULT` a VMA is locked and not populated, and each page
/// is pulled in and locked when the program first touches it, so memory that is
/// reserved and never used is never resident, and after warm-up the hot path is
/// resident and locked and takes no fault (the same zero it took before).
///
/// `MCL_ONFAULT` is Linux 4.4 (2016), and availability is checked by asking the
/// kernel: a kernel that does not know the flag fails the call with `EINVAL`,
/// which is answered here as an error naming the version and the fix, and never
/// by falling back to pinning every mapping. The value is `libc`'s, and nix's
/// `MlockAllFlags` has no constant for it, so the call is made through `libc`.
///
/// The limit must be unlimited, and that is the owner's rule for how a service
/// starts. The kernel tests `RLIMIT_MEMLOCK` itself: `mlockall` compares
/// `mm->total_vm` with the soft limit before it applies any flag, so a finite
/// limit below this process's address space fails the call with `ENOMEM`
/// whether or not `MCL_ONFAULT` is set (`mm/mlock.c`). Under `MCL_FUTURE` every
/// later `mmap` and `brk` checks `mlock_future_ok`, which refuses a mapping
/// that would take `mm->locked_vm` past the same soft limit (`mm/mmap.c`), so
/// the kernel never creates a locked mapping unlocked. It cannot half-pin this
/// process.
///
/// The limit is checked here anyway, so a finite one is one error at start with
/// the same text for every limit and every process, including a privileged
/// process the kernel would have allowed. That is the policy.
/// `LimitMEMLOCK=infinity` is what the unit sets and what the error names.
pub(super) fn lock_memory() -> io::Result<()> {
    use nix::sys::resource::{getrlimit, Resource};
    // The soft limit is the one the kernel enforces. `mlockall` compares
    // `mm->total_vm` with `rlimit(RLIMIT_MEMLOCK)`, and every later `mmap` and
    // `brk` compares `mm->locked_vm` with the same soft limit (`mlock_future_ok`
    // in `mm/mmap.c`), so a hard limit of infinity with a finite soft limit
    // would pass a hard-only check and be refused by the kernel.
    let (soft, _hard) = getrlimit(Resource::RLIMIT_MEMLOCK)?;
    if soft != nix::libc::RLIM_INFINITY {
        return Err(lock_memory_refused(&format!(
            "pinning pages as they are faulted needs an unlimited RLIMIT_MEMLOCK, and the soft \
             limit is {soft} bytes"
        )));
    }
    const FLAGS: nix::libc::c_int =
        nix::libc::MCL_CURRENT | nix::libc::MCL_FUTURE | nix::libc::MCL_ONFAULT;
    // SAFETY: `mlockall` takes no pointers, reads no memory of the caller's, and
    // changes only how this process's own pages are backed. Every flag is a
    // constant from the platform's headers.
    let rc = unsafe { nix::libc::mlockall(FLAGS) };
    if rc == 0 {
        return Ok(());
    }
    let errno = io::Error::last_os_error();
    match errno.raw_os_error() {
        Some(nix::libc::EINVAL) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "mlockall with MCL_ONFAULT failed (EINVAL): locking pages as they are touched is \
             Linux 4.4 or newer, and this kernel does not know the flag. Set [server] \
             lock_memory = false to run unpinned on this kernel",
        )),
        Some(nix::libc::ENOMEM) | Some(nix::libc::EPERM) => {
            Err(lock_memory_refused(&format!("mlockall failed ({errno})")))
        }
        _ => Err(errno),
    }
}

/// `/proc/self/status`'s `field`, in bytes. `None` where the file or the line is
/// missing, so a caller logs nothing rather than a wrong number.
fn status_bytes(field: &str) -> Option<usize> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix(field))?
        .split_whitespace()
        .next()?;
    value.parse::<usize>().ok().map(|kib| kib * 1024)
}

/// The size of this process's address space, from `/proc/self/status` (`VmSize`).
pub(super) fn virtual_bytes() -> Option<usize> {
    status_bytes("VmSize:")
}

/// This process's resident set, from `/proc/self/status` (`VmRSS`).
pub(super) fn resident_bytes() -> Option<usize> {
    status_bytes("VmRSS:")
}

/// `madvise(MADV_HUGEPAGE)` on the range.
#[allow(unsafe_code)] // the caller owns the range, and the advice changes no byte of it
pub(super) fn advise_huge_pages(start: *mut u8, len: usize) -> io::Result<()> {
    use nix::sys::mman::{madvise, MmapAdvise};
    let Some(start) = std::ptr::NonNull::new(start.cast::<std::ffi::c_void>()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "null range"));
    };
    // SAFETY: MADV_HUGEPAGE only changes how the kernel backs the range.
    unsafe { madvise(start, len, MmapAdvise::MADV_HUGEPAGE) }.map_err(io::Error::from)
}

/// `AnonHugePages` (transparent) and `Hugetlb` (explicit) of this process, from
/// `/proc/self/smaps_rollup`, in bytes.
pub(super) fn huge_page_bytes() -> usize {
    let Ok(text) = std::fs::read_to_string("/proc/self/smaps_rollup") else {
        return 0;
    };
    text.lines()
        .filter_map(|line| {
            let rest = line
                .strip_prefix("AnonHugePages:")
                .or_else(|| line.strip_prefix("Hugetlb:"))?;
            rest.trim().strip_suffix("kB")?.trim().parse::<usize>().ok()
        })
        .sum::<usize>()
        * 1024
}

// ── Process facts ────────────────────────────────────────────────────────────

pub(super) fn thread_count(pid: u32) -> io::Result<usize> {
    Ok(std::fs::read_dir(format!("/proc/{pid}/task"))?.count())
}

/// Reading the host's certificate store is a file read on this platform, so it
/// leaves no thread behind.
pub(super) fn certificate_store_workers() -> (usize, usize) {
    (0, 0)
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

/// `VmRSS`, `VmSize` and `VmLck` of `/proc/<pid>/status`, in bytes. The file
/// writes them in KiB and the kernel's label says `kB`, which is KiB.
pub(super) fn memory_usage(pid: u32) -> io::Result<ProcessMemory> {
    let path = format!("/proc/{pid}/status");
    let text = std::fs::read_to_string(&path)?;
    let field = |name: &str| -> io::Result<u64> {
        let value = text
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .ok_or_else(|| io::Error::other(format!("{path} has no {name}")))?;
        let kib: u64 = value
            .split_whitespace()
            .next()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| io::Error::other(format!("{path}: {name} is not a number")))?;
        Ok(kib.saturating_mul(1024))
    };
    Ok(ProcessMemory {
        resident_bytes: field("VmRSS:")?,
        virtual_bytes: field("VmSize:")?,
        locked_bytes: field("VmLck:")?,
    })
}

/// Field 10 of `/proc/<pid>/stat`, the minor page faults. The command in field
/// 2 is in parentheses and may hold spaces, so the fields start after the last
/// `)`, where the first is field 3.
pub(super) fn page_faults(pid: u32) -> io::Result<u64> {
    let path = format!("/proc/{pid}/stat");
    let stat = std::fs::read_to_string(&path)?;
    let command_end = stat
        .rfind(')')
        .ok_or_else(|| io::Error::other(format!("{path} has no command")))?;
    stat[command_end + 1..]
        .split_whitespace()
        .nth(7)
        .and_then(|faults| faults.parse().ok())
        .ok_or_else(|| io::Error::other(format!("{path} has no minor fault count")))
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
///
/// The cgroup files are read at the group `/proc/self/cgroup` names and walk
/// to the nearest parent that has them, so a container's own limit wins over
/// the root group's `max`, which would otherwise report the machine's memory.
pub(super) fn memory() -> Option<Memory> {
    let group = self_cgroup();
    let root = Path::new(CGROUP_V2);
    let group = group.as_deref().unwrap_or("/");
    if let (Some(max), Some(current)) = (
        facts::read_cgroup_file(root, group, "memory.max"),
        facts::read_cgroup_file(root, group, "memory.current"),
    ) {
        if let Some(m) = parse_cgroup_memory(&max, &current) {
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

// ── Host facts ───────────────────────────────────────────────────────────────
//
// The start-up facts that differ from macOS. The reads that are the same on
// both supported families, and every parser, are in `facts.rs`. A fact that
// cannot be read is `Fact::Unknown` with a reason naming the file or call that
// did not answer.

/// The OS product name and version, from `/etc/os-release`. A system that has
/// moved the file to `/usr/lib/os-release` still answers.
pub(super) fn host_os_facts() -> Fact<OsFacts> {
    const RELEASE: &str = "/etc/os-release";
    const FALLBACK: &str = "/usr/lib/os-release";
    let text = match facts::read_file(RELEASE, "cannot read /etc/os-release") {
        Fact::Known(text) => Fact::Known(text),
        Fact::Unknown(_) => facts::read_file(
            FALLBACK,
            "cannot read /etc/os-release or /usr/lib/os-release",
        ),
    };
    facts::parse(text, facts::parse_os_release, "/etc/os-release has no NAME")
}

/// The CPU model, from `/proc/cpuinfo`, and the number of CPUs this process
/// may run on.
pub(super) fn host_cpu() -> CpuFacts {
    CpuFacts {
        model: facts::parse(
            facts::read_file("/proc/cpuinfo", "cannot read /proc/cpuinfo"),
            facts::parse_cpuinfo_model,
            "/proc/cpuinfo has no CPU model",
        ),
        count: facts::cpu_count(),
    }
}

/// Total memory, the page size and the huge-page setting.
pub(super) fn host_memory() -> MemoryFacts {
    MemoryFacts {
        total_bytes: facts::parse(
            facts::read_file("/proc/meminfo", "cannot read /proc/meminfo"),
            facts::parse_meminfo_total,
            "/proc/meminfo has no MemTotal",
        ),
        page_size: facts::page_size(),
        huge_pages: facts::parse(
            facts::read_file(
                "/sys/kernel/mm/transparent_hugepage/enabled",
                "cannot read /sys/kernel/mm/transparent_hugepage/enabled",
            ),
            facts::parse_hugepage_setting,
            "/sys/kernel/mm/transparent_hugepage/enabled names no setting",
        ),
    }
}

/// The rlimits, the listen backlog cap and the cgroup limits.
pub(super) fn host_limits() -> Limits {
    use nix::sys::resource::{getrlimit, Resource};
    Limits {
        memlock: match getrlimit(Resource::RLIMIT_MEMLOCK) {
            Ok((soft, _hard)) => Fact::Known(soft),
            Err(_) => Fact::Unknown("getrlimit(RLIMIT_MEMLOCK) failed"),
        },
        nofile: facts::nofile(),
        somaxconn: facts::parse(
            facts::read_file(
                "/proc/sys/net/core/somaxconn",
                "cannot read /proc/sys/net/core/somaxconn",
            ),
            |text| text.trim().parse::<u32>().ok(),
            "/proc/sys/net/core/somaxconn is not a number",
        ),
        cgroup_memory: cgroup_memory(),
        cgroup_cpu: cgroup_cpu(),
    }
}

/// The mount point of the cgroup v2 hierarchy.
const CGROUP_V2: &str = "/sys/fs/cgroup";

/// This process's directory in the cgroup v2 hierarchy, from
/// `/proc/self/cgroup`.
///
/// `None` on a host with no v2 hierarchy, and on one that will not show the
/// file. Both callers then walk from the mount point itself, which is the
/// answer this code gave every host before the walk existed.
fn self_cgroup() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    facts::parse_self_cgroup_v2(&text)
}

/// The cgroup memory limit, from cgroup v2's `memory.max` or cgroup v1's
/// `memory.limit_in_bytes`.
///
/// The v2 file is read at the group `/proc/self/cgroup` names, and at the
/// nearest parent when the group has no file of its own. Reading it at the
/// mount point answers the root group's setting, which is the host's and not
/// this process's.
fn cgroup_memory() -> Fact<Option<u64>> {
    const V1: &str = "/sys/fs/cgroup/memory/memory.limit_in_bytes";
    let group = self_cgroup();
    let v2 = facts::read_cgroup_memory(Path::new(CGROUP_V2), group.as_deref().unwrap_or("/"));
    if v2.is_known() {
        return v2;
    }
    let v1 = facts::parse(
        facts::read_file(V1, "cannot read the cgroup memory limit"),
        facts::parse_cgroup_memory_limit_in_bytes,
        "the cgroup memory limit is not a number",
    );
    // A v2 host answers above. Where neither hierarchy has a limit, the v2
    // answer names the file and the walk, so it is the one kept.
    if v1.is_known() {
        v1
    } else {
        v2
    }
}

/// The cgroup CPU quota, from cgroup v2's `cpu.max` or cgroup v1's
/// `cpu.cfs_quota_us` and `cpu.cfs_period_us`.
///
/// The v2 file is read at the group `/proc/self/cgroup` names and walks to the
/// nearest parent that has one, as [`cgroup_memory`] does.
fn cgroup_cpu() -> Fact<Option<CpuLimit>> {
    const QUOTA: &str = "/sys/fs/cgroup/cpu/cpu.cfs_quota_us";
    const PERIOD: &str = "/sys/fs/cgroup/cpu/cpu.cfs_period_us";
    let group = self_cgroup();
    let v2 = facts::read_cgroup_cpu(Path::new(CGROUP_V2), group.as_deref().unwrap_or("/"));
    if v2.is_known() {
        return v2;
    }
    let quota = facts::read_file(QUOTA, "cannot read the cgroup CPU quota");
    let period = facts::read_file(PERIOD, "cannot read the cgroup CPU quota");
    let v1 = match (quota, period) {
        (Fact::Known(quota), Fact::Known(period)) => {
            match facts::parse_cgroup_cpu_v1(&quota, &period) {
                Some(limit) => Fact::Known(limit),
                None => Fact::Unknown("the cgroup v1 CPU quota is not a quota and period"),
            }
        }
        (Fact::Unknown(reason), _) | (_, Fact::Unknown(reason)) => Fact::Unknown(reason),
    };
    if v1.is_known() {
        v1
    } else {
        v2
    }
}

/// The time zone, the kernel's clock source and the boot time.
pub(super) fn host_clock() -> ClockFacts {
    const SOURCE: &str = "/sys/devices/system/clocksource/clocksource0/current_clocksource";
    ClockFacts {
        timezone: facts::timezone(),
        source: facts::parse(
            facts::read_file(SOURCE, "cannot read the kernel clock source"),
            |text| match text.trim() {
                "" => None,
                source => Some(source.to_string()),
            },
            "the kernel clock source file is empty",
        ),
        boot_time: facts::parse(
            facts::read_file("/proc/stat", "cannot read /proc/stat"),
            facts::parse_btime,
            "/proc/stat has no btime",
        ),
    }
}

/// The virtualisation type, from the DMI strings and the CPU's `hypervisor`
/// flag.
pub(super) fn host_virtualization() -> Fact<String> {
    const VENDOR: &str = "/sys/class/dmi/id/sys_vendor";
    const PRODUCT: &str = "/sys/class/dmi/id/product_name";
    let vendor = facts::read_file(VENDOR, "cannot read /sys/class/dmi/id/sys_vendor");
    let product = facts::read_file(PRODUCT, "cannot read /sys/class/dmi/id/product_name");
    let cpuinfo = facts::read_file("/proc/cpuinfo", "cannot read /proc/cpuinfo");
    if vendor.reason().is_some() && product.reason().is_some() && cpuinfo.reason().is_some() {
        return Fact::Unknown("cannot read /sys/class/dmi/id or /proc/cpuinfo");
    }
    let hypervisor = cpuinfo
        .known()
        .is_some_and(|text| facts::parse_hypervisor_flag(text));
    match facts::parse_virtualization(
        vendor.known().map(String::as_str),
        product.known().map(String::as_str),
        hypervisor,
    ) {
        Some(kind) => Fact::Known(kind),
        None => Fact::Unknown("the DMI strings name no virtualisation"),
    }
}

#[cfg(test)]
mod tests {
    //! The reuseport steering, on real sockets. Linux only: this module is part
    //! of the Linux arm, so on macOS it is not compiled.

    use std::io;
    use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
    use std::num::NonZeroU32;

    use crate::net::listen_shared;

    const GROUP: Option<NonZeroU32> = NonZeroU32::new(2);
    /// More connections than two sockets would split evenly by chance: with
    /// the kernel's own 4-tuple hash, 64 from one address reach both sockets.
    const MANY: usize = 64;

    /// Two members of one group on a loopback port, in join order: the first
    /// socket is index 0 and the second index 1.
    fn pair(group: Option<NonZeroU32>) -> (SocketAddr, [(TcpListener, UdpSocket); 2]) {
        let first = listen_shared(SocketAddr::from(([127, 0, 0, 1], 0)), group).expect("first");
        let addr = first.0.local_addr().expect("addr");
        let second = listen_shared(addr, group).expect("second");
        (addr, [first, second])
    }

    /// Which listener took the connection a client just made: loopback queues
    /// it before `connect` returns.
    fn tcp_owner(members: &[(TcpListener, UdpSocket); 2]) -> usize {
        let hits: Vec<usize> = (0..2)
            .filter(|&i| match members[i].0.accept() {
                Ok(_) => true,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
                Err(e) => panic!("accept: {e}"),
            })
            .collect();
        assert_eq!(hits.len(), 1, "one connection reaches exactly one listener");
        hits[0]
    }

    /// The socket that holds the datagram a client just sent.
    fn udp_owner(members: &[(TcpListener, UdpSocket); 2]) -> usize {
        let mut buf = [0u8; 8];
        let hits: Vec<usize> = (0..2)
            .filter(|&i| match members[i].1.recv_from(&mut buf) {
                Ok(_) => true,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
                Err(e) => panic!("recv: {e}"),
            })
            .collect();
        assert_eq!(hits.len(), 1, "one datagram reaches exactly one socket");
        hits[0]
    }

    /// `MANY` connections, each from a new source port of `src`, and the set of
    /// listeners that took them.
    fn tcp_owners(
        addr: SocketAddr,
        src: IpAddr,
        members: &[(TcpListener, UdpSocket); 2],
    ) -> [bool; 2] {
        let mut seen = [false; 2];
        for _ in 0..MANY {
            let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
                .expect("client socket");
            sock.bind(&SocketAddr::new(src, 0).into())
                .expect("bind the client address");
            sock.connect(&addr.into()).expect("connect");
            let _client = TcpStream::from(std::os::fd::OwnedFd::from(sock));
            seen[tcp_owner(members)] = true;
        }
        seen
    }

    /// `MANY` datagrams, each from a new source port of `src`.
    fn udp_owners(
        addr: SocketAddr,
        src: IpAddr,
        members: &[(TcpListener, UdpSocket); 2],
    ) -> [bool; 2] {
        let mut seen = [false; 2];
        for _ in 0..MANY {
            let client = UdpSocket::bind(SocketAddr::new(src, 0)).expect("bind the client address");
            client.send_to(b"x", addr).expect("send");
            seen[udp_owner(members)] = true;
        }
        seen
    }

    /// Many TCP connections from one address, each from its own source port,
    /// all reach one listener, and another address reaches the other. Break:
    /// no filter attached, so the kernel hashes the 4-tuple and one address
    /// reaches both listeners.
    ///
    /// The indices are exact: the hash of 127.0.0.1 is odd and of 127.0.0.2 is
    /// even, and the second listener to join is index 1.
    #[test]
    fn tcp_connections_from_one_address_reach_one_listener() {
        let (addr, members) = pair(GROUP);
        assert_eq!(
            tcp_owners(addr, [127, 0, 0, 1].into(), &members),
            [false, true]
        );
        assert_eq!(
            tcp_owners(addr, [127, 0, 0, 2].into(), &members),
            [true, false]
        );
    }

    /// A port of 0 gives the TCP listener and the UDP socket one port. The
    /// steering tests below depend on it: two UDP sockets on different ports
    /// are not a reuseport group, and every datagram goes to the one bound to
    /// the port it is sent to. Break: bind the UDP socket to `addr` as it was
    /// asked for, and the first member's UDP port differs from its TCP port.
    #[test]
    fn a_port_of_zero_gives_both_halves_one_port() {
        let first = listen_shared(SocketAddr::from(([127, 0, 0, 1], 0)), None).expect("first");
        assert_eq!(
            first.0.local_addr().expect("tcp addr").port(),
            first.1.local_addr().expect("udp addr").port()
        );
    }

    /// The same for UDP, which is what QUIC runs on. Break: as above.
    #[test]
    fn udp_datagrams_from_one_address_reach_one_socket() {
        let (addr, members) = pair(GROUP);
        assert_eq!(
            udp_owners(addr, [127, 0, 0, 1].into(), &members),
            [false, true]
        );
        assert_eq!(
            udp_owners(addr, [127, 0, 0, 2].into(), &members),
            [true, false]
        );
    }

    /// IPv6 takes the other branch of the program. `::1` is the one local
    /// address a host has, its four words XOR to 1, and 1 mod 2 is index 1.
    /// A host with no IPv6 loopback has nothing to test and says so.
    #[test]
    fn ipv6_datagrams_from_one_address_reach_one_socket() {
        let first = match listen_shared(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 0)), GROUP) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::AddrNotAvailable => {
                eprintln!("no IPv6 loopback on this host: {e}");
                return;
            }
            Err(e) => panic!("bind: {e}"),
        };
        let addr = first.1.local_addr().expect("addr");
        let second = listen_shared(addr, GROUP).expect("second");
        let members = [first, second];
        let mut seen = [false; 2];
        for _ in 0..MANY {
            let client = UdpSocket::bind("[::1]:0").expect("bind the client address");
            client.send_to(b"x", addr).expect("send");
            seen[udp_owner(&members)] = true;
        }
        assert_eq!(seen, [false, true]);
    }

    /// With no group size nothing is attached, and the kernel spreads one
    /// address's connections over both listeners by its own hash. This is what
    /// the other tests would see without the filter.
    #[test]
    fn without_a_group_size_one_address_reaches_both_listeners() {
        let (addr, members) = pair(None);
        assert_eq!(
            tcp_owners(addr, [127, 0, 0, 1].into(), &members),
            [true, true]
        );
    }
}
