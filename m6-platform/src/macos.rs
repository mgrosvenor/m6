//! The macOS implementation of the platform layer. Selected by OS family in
//! `platform.rs`, so it is the same code on arm64 and x86_64.
//!
//! Every `pub(super)` item here has a twin of the same name in `linux.rs`, and
//! `tests/platform_gate.rs` holds the two lists equal. The layer's tests are
//! shared and run unchanged on every target; the one exception is the test that
//! drives [`test_recv_error`], because the per-datagram receive it holds is the
//! part of the receive this file owns and Linux does not have. A port to
//! another kqueue system (FreeBSD, OpenBSD) starts from this file.

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};

use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

use super::facts::{self, ClockFacts, CpuFacts, Fact, Limits, MemoryFacts, OsFacts};
use super::host::{
    DiskIo, HostSnapshot, LoadAverage, Memory, MemorySource, NetDevice, Pressure, Source,
    TcpHealth, ThermalZone,
};
use super::process::ProcessMemory;
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
        loop {
            let n = self
                .kq
                .kevent(&[], std::slice::from_mut(&mut out), Some(NOW))
                .ok()?;
            if n == 0 {
                return None;
            }
            if let Some(signal) = super::signal_from_signo(out.ident() as i32) {
                return Some(signal);
            }
        }
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

/// macOS has no UDP receive coalescing, so every receive is one datagram.
pub(super) const RECV_COALESCING: bool = false;

/// `SO_NOSIGPIPE`, so a write to a closed peer is an error and never a
/// signal, whatever the process's disposition. An accepted socket inherits it
/// from its listener.
pub(super) fn prepare_socket(sock: &socket2::Socket) -> io::Result<()> {
    sock.set_nosigpipe(true)
}

/// macOS consults the listen queue before the descriptor table, so an `EMFILE`
/// has already taken and dropped the queued connection, and an empty queue
/// answers `WouldBlock`.
pub(super) const EMFILE_TAKES_QUEUED: bool = true;

/// macOS has no `/proc`, and kqueue reports a listener as readable without
/// saying how many connections are behind it, so there is no queue depth to
/// return.
pub(super) fn accept_queue(_local: SocketAddr) -> io::Result<Option<usize>> {
    Ok(None)
}

/// macOS has no `TCP_NOTSENT_LOWAT`. The kernel takes what the socket gives it
/// and its own order stands, so there is nothing to set and nothing to undo:
/// the caller's frames still leave in the order they were written.
pub(super) fn limit_send_queue(_fd: RawFd, _bytes: usize) -> io::Result<()> {
    Ok(())
}

/// macOS has no reuseport socket filter. The socket bound last to a port gets
/// every connection and datagram, so there is nothing to attach and one client
/// already reaches one process.
pub(super) fn steer_reuseport(_fd: RawFd, _group_size: u32) -> io::Result<()> {
    Ok(())
}

/// macOS has no segmentation offload, so a batch is never one segmented send.
pub(super) fn udp_gso_supported(_socket: &impl AsFd) -> bool {
    false
}

/// macOS has no UDP receive coalescing. `recvfrom` returns one datagram per
/// call, so there is nothing to enable and nothing to report: every receive is
/// one datagram, which is the path the socket already takes.
pub(super) fn udp_gro_supported(_socket: &impl AsFd) -> bool {
    false
}

/// Send `segments` one datagram at a time, in order: macOS has no
/// segmentation offload, so the shared fallback is the whole path here.
pub(super) fn udp_send_batch(
    socket: &impl AsFd,
    segments: &[u8],
    segment_size: usize,
    dest: SocketAddr,
    _gso: bool,
) -> io::Result<usize> {
    super::net::udp_send_each(socket, segments, segment_size, dest)
}

/// Receive up to `metas.len()` datagrams, one `recvfrom` each.
pub(super) fn udp_recv_batch(
    socket: &impl AsFd,
    buf: &mut [u8],
    slot: usize,
    metas: &mut [super::net::RecvMeta],
) -> io::Result<usize> {
    use nix::errno::Errno;
    use nix::sys::socket::{recvfrom, SockaddrStorage};

    // `UdpBatch::recv` trims both slices to one count and never calls this with
    // an empty one, so the metas it hands over are exactly what the call takes
    // in.
    let fd = socket.as_fd().as_raw_fd();
    let mut received = 0;
    for (i, meta) in metas.iter_mut().enumerate() {
        let start = i * slot;
        #[cfg(test)]
        let read = if test_recv_error::due(received) {
            // The error a connected socket reports for a peer that is gone.
            Err(Errno::ECONNREFUSED)
        } else {
            recvfrom::<SockaddrStorage>(fd, &mut buf[start..start + slot])
        };
        #[cfg(not(test))]
        let read = recvfrom::<SockaddrStorage>(fd, &mut buf[start..start + slot]);
        match read {
            Ok((len, from)) => {
                let from = from.as_ref().and_then(super::net::socket_addr_of);
                // One `recvfrom` is one datagram, so every receive has no
                // stride to split it by.
                *meta = super::net::RecvMeta {
                    len,
                    from,
                    stride: 0,
                };
                received += 1;
            }
            Err(Errno::EAGAIN) if received == 0 => return Err(Errno::EAGAIN.into()),
            Err(Errno::EAGAIN) => break,
            // An error after one or more datagrams were read keeps them: the
            // count is what this call took in, and returning the error here
            // would drop them. macOS clears a socket error once `recvfrom`
            // reports it, so the error is consumed here and the next call is
            // not told it again.
            Err(_) if received > 0 => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(received)
}

/// A socket error a test asks the receive below to meet after `after`
/// datagrams.
///
/// The real error cannot be produced to order: the kernel reports it at the
/// next receive, not between two dequeues, so a test that waits for one is
/// testing the network. This module is `cfg(test)`, and a release build
/// contains neither it nor its call.
#[cfg(test)]
mod test_recv_error {
    use std::cell::Cell;

    thread_local! {
        /// Fail this thread's next batch receive after this many datagrams.
        static AFTER: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Fail this thread's next batch receive after `after` datagrams.
    pub(crate) fn fail_after(after: usize) {
        AFTER.with(|a| a.set(Some(after)));
    }

    /// Whether this receive is the one to fail, taking the request off.
    pub(crate) fn due(received: usize) -> bool {
        AFTER.with(|a| match a.get() {
            Some(after) if received >= after => {
                a.set(None);
                true
            }
            _ => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket error that arrives after a datagram was read keeps that
    /// datagram, and the error is not reported again: macOS clears it once
    /// `recvfrom` reports it, so the call that met it returns the datagrams.
    ///
    /// Break: a receive that returns the error after it has read a datagram,
    /// and so drops what it read.
    #[test]
    fn a_socket_error_after_a_datagram_keeps_it() {
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        receiver.set_nonblocking(true).expect("nonblocking");
        let to = receiver.local_addr().expect("receiver address");
        sender.send_to(b"one", to).expect("send one");
        sender.send_to(b"two", to).expect("send two");
        // The datagrams have to be in the receiver's queue before the seam can
        // stop the receive between them. Wait for the first rather than assume
        // the send has landed: a loaded machine queues it a moment later.
        assert!(
            crate::wait_readable(receiver.as_fd(), std::time::Duration::from_secs(5))
                .expect("wait for the receiver"),
            "the datagrams did not arrive"
        );
        let mut buf = [0u8; 2 * 64];
        let mut metas = [super::super::net::RecvMeta::default(); 2];

        test_recv_error::fail_after(1);
        let got = udp_recv_batch(&receiver, &mut buf, 64, &mut metas)
            .expect("the datagram read before the error is kept");
        assert_eq!(got, 1, "the datagram the batch read was dropped");
        assert_eq!(&buf[..3], b"one", "the bytes the batch read");

        // The failing call reported the error and cleared it, so the next call
        // takes the next datagram rather than reporting the error again. The
        // two datagrams cross loopback one at a time, so wait for the second
        // rather than assume it is already queued.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let got = loop {
            match udp_recv_batch(&receiver, &mut buf, 64, &mut metas) {
                Ok(n) => break n,
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    assert!(crate::wait_readable(
                        receiver.as_fd(),
                        std::time::Duration::from_millis(50)
                    )
                    .expect("wait for the receiver"));
                }
                Err(e) => panic!("the next call: {e}"),
            }
        };
        assert_eq!(got, 1, "the datagram behind the error");
        assert_eq!(&buf[..3], b"two", "the bytes the next call read");
        assert_eq!(
            udp_recv_batch(&receiver, &mut buf, 64, &mut metas)
                .expect_err("nothing is waiting")
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }
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

/// There is no call that pins a whole process on macOS, so there is nothing to
/// do and nothing to refuse.
pub(super) fn lock_memory() -> io::Result<()> {
    Ok(())
}

/// macOS has no `/proc`, so this process's address space and resident set are
/// not read here. `None` says so rather than reporting a number from elsewhere.
pub(super) fn virtual_bytes() -> Option<usize> {
    None
}

/// macOS has no `/proc`, so this process's resident set is not read here.
pub(super) fn resident_bytes() -> Option<usize> {
    None
}

/// macOS has no transparent huge pages to ask for.
pub(super) fn advise_huge_pages(_start: *mut u8, _len: usize) -> io::Result<()> {
    Ok(())
}

/// macOS reports no huge-page use to a process.
pub(super) fn huge_page_bytes() -> usize {
    0
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

/// Reading the host's certificate store goes through the Security framework,
/// whose idle libdispatch workers stay behind in the process: three or four,
/// swinging between starts.
pub(super) fn certificate_store_workers() -> (usize, usize) {
    (3, 4)
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

/// macOS reports its own process's memory through `task_info`, and keeps no
/// report for another process. `Err` says so rather than reporting zeros.
pub(super) fn memory_usage(pid: u32) -> io::Result<ProcessMemory> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("macOS keeps no memory report for pid {pid}"),
    ))
}

/// The kernel's page fault counts are not exposed for another process on
/// macOS. `Err` says so rather than reporting zero faults.
pub(super) fn page_faults(pid: u32) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("macOS keeps no page fault count for pid {pid}"),
    ))
}

// ── Host readings, through sysinfo ───────────────────────────────────────────
//
// `sysinfo` with `default-features = false` and the `system` and `network`
// features alone: no `multithread` feature, so no thread pool, which
// `tests/platform_contract.rs` holds. Disk throughput, pressure stall
// averages and thermal zones have no reading without IOKit, so they are
// absent here, and absent is an empty list or `None`, never a row of zeros.

pub(super) fn net_devices() -> Vec<NetDevice> {
    interface_rows(&sysinfo::Networks::new_with_refreshed_list())
}

/// The rows of `sysinfo`'s interface table, sorted by name.
fn interface_rows(nets: &sysinfo::Networks) -> Vec<NetDevice> {
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
    memory_of(&mut s)
}

/// Memory from a `sysinfo` view the caller keeps, refreshed here.
fn memory_of(s: &mut sysinfo::System) -> Option<Memory> {
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

/// What a [`super::host::HostReader`] keeps between steps on macOS: the
/// `sysinfo` views, made once, so a step refreshes one instead of building it.
/// The interface list is fixed at start, as the Linux reader fixes its thermal
/// zones.
pub(super) struct HostState {
    system: sysinfo::System,
    networks: sysinfo::Networks,
}

impl HostState {
    pub(super) fn new() -> HostState {
        HostState {
            system: sysinfo::System::new(),
            networks: sysinfo::Networks::new_with_refreshed_list(),
        }
    }

    /// No thermal reading without IOKit, so the list is empty.
    pub(super) fn thermal_zones(&mut self) -> Vec<ThermalZone> {
        Vec::new()
    }

    /// Read one source into `snapshot`. The sources macOS has no reading for
    /// stay absent.
    pub(super) fn read(&mut self, source: Source, snapshot: &mut HostSnapshot) {
        match source {
            Source::Load => snapshot.load = load_average(),
            Source::Memory => snapshot.memory = memory_of(&mut self.system),
            Source::Uptime => snapshot.uptime_s = uptime().map(|d| d.as_secs()),
            Source::Net => {
                self.networks.refresh();
                snapshot.net = interface_rows(&self.networks);
            }
            Source::DiskIo
            | Source::CpuPressure
            | Source::IoPressure
            | Source::MemoryPressure
            | Source::Netstat
            | Source::Sockstat
            | Source::Thermal(_)
            | Source::Disk
            | Source::Descriptors => {}
        }
    }
}

// ── Host facts ───────────────────────────────────────────────────────────────
//
// The start-up facts that differ from Linux. The reads that are the same on
// both supported families, and every parser, are in `facts.rs`. A fact that
// cannot be read is `Fact::Unknown` with a reason naming the `sysctl` or file
// that did not answer.

/// The OS product name and version, from Apple's `SystemVersion.plist`.
pub(super) fn host_os_facts() -> Fact<OsFacts> {
    const PLIST: &str = "/System/Library/CoreServices/SystemVersion.plist";
    facts::parse(
        facts::read_file(PLIST, "cannot read SystemVersion.plist"),
        facts::parse_system_version,
        "SystemVersion.plist has no ProductName",
    )
}

/// The CPU model, from `machdep.cpu.brand_string`, and the number of CPUs this
/// process may run on.
pub(super) fn host_cpu() -> CpuFacts {
    CpuFacts {
        model: match sysctl_string("machdep.cpu.brand_string") {
            Some(model) if !model.trim().is_empty() => Fact::Known(model.trim().to_string()),
            Some(_) => Fact::Unknown("machdep.cpu.brand_string is empty"),
            None => Fact::Unknown("cannot read machdep.cpu.brand_string"),
        },
        count: facts::cpu_count(),
    }
}

/// Total memory, the page size and the huge-page setting.
///
/// macOS has no transparent huge pages, so that fact is unknown rather than
/// zero, which would read as a host with none left.
pub(super) fn host_memory() -> MemoryFacts {
    MemoryFacts {
        total_bytes: match sysctl_u64("hw.memsize") {
            Some(bytes) if bytes > 0 => Fact::Known(bytes),
            Some(_) => Fact::Unknown("hw.memsize is zero"),
            None => Fact::Unknown("cannot read hw.memsize"),
        },
        page_size: facts::page_size(),
        huge_pages: Fact::Unknown("macOS has no transparent huge pages"),
    }
}

/// The open-file limit, `kern.ipc.somaxconn`, and the cgroup limits.
///
/// macOS has no `RLIMIT_MEMLOCK` and no cgroups, so those facts are unknown
/// rather than zero or unlimited.
pub(super) fn host_limits() -> Limits {
    Limits {
        memlock: Fact::Unknown("macOS has no RLIMIT_MEMLOCK"),
        nofile: facts::nofile(),
        somaxconn: match sysctl_i32("kern.ipc.somaxconn") {
            Some(backlog) if backlog > 0 => Fact::Known(backlog as u32),
            Some(_) => Fact::Unknown("kern.ipc.somaxconn is not positive"),
            None => Fact::Unknown("cannot read kern.ipc.somaxconn"),
        },
        cgroup_memory: Fact::Unknown("macOS has no cgroups"),
        cgroup_cpu: Fact::Unknown("macOS has no cgroups"),
    }
}

/// The time zone, the clock source and the boot time.
///
/// macOS does not expose which hardware counter the kernel uses, so that one
/// fact is unknown. The boot time is `kern.boottime`.
pub(super) fn host_clock() -> ClockFacts {
    ClockFacts {
        timezone: facts::timezone(),
        source: Fact::Unknown("macOS does not report its clock source"),
        boot_time: match sysctl_timeval("kern.boottime") {
            Some(seconds) => {
                Fact::Known(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds))
            }
            None => Fact::Unknown("cannot read kern.boottime"),
        },
    }
}

/// Whether a hypervisor is running this kernel, and, when the hardware model
/// says so, which one.
pub(super) fn host_virtualization() -> Fact<String> {
    match sysctl_i32("kern.hv_vmm_present") {
        Some(1) => {
            let model = sysctl_string("hw.model").unwrap_or_default();
            if model.starts_with("VirtualMac") || model.contains("Virtual") {
                Fact::Known("virtualization.framework".to_string())
            } else {
                Fact::Known("hypervisor".to_string())
            }
        }
        Some(_) => Fact::Known("none".to_string()),
        None => Fact::Unknown("cannot read kern.hv_vmm_present"),
    }
}

/// The value at `name`, through the `sysctl` crate.
fn sysctl_value(name: &str) -> Option<sysctl::CtlValue> {
    use sysctl::Sysctl;
    sysctl::Ctl::new(name).and_then(|c| c.value()).ok()
}

/// A `sysctl` string value at `name`.
fn sysctl_string(name: &str) -> Option<String> {
    match sysctl_value(name)? {
        sysctl::CtlValue::String(s) => Some(s.trim_end_matches('\0').to_string()),
        _ => None,
    }
}

/// A `sysctl` unsigned value at `name`, whatever width the OID has.
fn sysctl_u64(name: &str) -> Option<u64> {
    match sysctl_value(name)? {
        sysctl::CtlValue::U64(n) | sysctl::CtlValue::Ulong(n) => Some(n),
        sysctl::CtlValue::Uint(n) => Some(u64::from(n)),
        sysctl::CtlValue::S64(n) | sysctl::CtlValue::Long(n) => u64::try_from(n).ok(),
        sysctl::CtlValue::Int(n) => u64::try_from(n).ok(),
        _ => None,
    }
}

/// A `sysctl` `i32` value at `name`.
///
/// A raw `sysctlbyname`, where the others go through the `sysctl` crate. The
/// crate checks the OID's flags before it reads, and it refuses
/// `kern.hv_vmm_present` with `NoReadAccess` on macOS 26 although the kernel
/// answers the read.
fn sysctl_i32(name: &str) -> Option<i32> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut value: i32 = 0;
    let mut size = std::mem::size_of::<i32>();
    // SAFETY: `value` is a live `i32` and `size` is its size in bytes.
    let rc = unsafe {
        nix::libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut i32).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(value)
}

/// The seconds since the epoch of a `struct timeval` sysctl, such as
/// `kern.boottime`.
fn sysctl_timeval(name: &str) -> Option<u64> {
    use sysctl::Sysctl;
    let value = sysctl::Ctl::new(name)
        .and_then(|c| c.value_as::<nix::libc::timeval>())
        .ok()?;
    u64::try_from(value.tv_sec).ok()
}

// Two file actions in libSystem since macOS 10.7 and 10.15 that the `libc`
// crate does not declare.
extern "C" {
    fn posix_spawn_file_actions_addinherit_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        fd: libc::c_int,
    ) -> libc::c_int;
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> libc::c_int;
}

/// A `posix_spawn` return code as an error.
fn spawn_check(code: libc::c_int) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

/// A string for the C side, refusing an interior NUL as `std` does.
fn c_string(bytes: &[u8]) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "an argument holds a NUL byte"))
}

/// The file actions and attributes of one spawn, destroyed on every path out.
struct SpawnSetup {
    actions: libc::posix_spawn_file_actions_t,
    attrs: libc::posix_spawnattr_t,
}

impl Drop for SpawnSetup {
    fn drop(&mut self) {
        // SAFETY: both were initialised by `SpawnSetup::new`, and this is their
        // one destroy.
        unsafe {
            libc::posix_spawn_file_actions_destroy(&mut self.actions);
            libc::posix_spawnattr_destroy(&mut self.attrs);
        }
    }
}

impl SpawnSetup {
    fn new() -> io::Result<Box<SpawnSetup>> {
        // Boxed, so the two objects never move after `init`.
        let mut setup = Box::new(SpawnSetup {
            actions: std::ptr::null_mut(),
            attrs: std::ptr::null_mut(),
        });
        // SAFETY: each pointer is to a field of the box, valid for the call.
        // A failed first init leaves nothing to destroy, so the box is forgotten.
        unsafe {
            let code = libc::posix_spawn_file_actions_init(&mut setup.actions);
            if code != 0 {
                std::mem::forget(setup);
                return Err(io::Error::from_raw_os_error(code));
            }
            let code = libc::posix_spawnattr_init(&mut setup.attrs);
            if code != 0 {
                libc::posix_spawn_file_actions_destroy(&mut setup.actions);
                std::mem::forget(setup);
                return Err(io::Error::from_raw_os_error(code));
            }
        }
        Ok(setup)
    }
}

/// [`crate::process::spawn`] on macOS: `posix_spawnp` with
/// `POSIX_SPAWN_CLOEXEC_DEFAULT`, so the child holds the three streams named
/// here and no other descriptor of this process, whatever its close-on-exec
/// flag. The mask is emptied and every managed signal and SIGPIPE is set to
/// its default disposition by the spawn's own attributes.
pub(super) fn spawn(
    cmd: &mut std::process::Command,
    streams: crate::process::Streams,
) -> io::Result<(u32, [Option<std::fs::File>; 3])> {
    use crate::process::Io;
    use std::os::unix::ffi::OsStrExt;

    let program = c_string(cmd.get_program().as_bytes())?;
    let mut argv = vec![program.clone()];
    for arg in cmd.get_args() {
        argv.push(c_string(arg.as_bytes())?);
    }
    let mut env: std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString> =
        std::env::vars_os().collect();
    for (key, value) in cmd.get_envs() {
        match value {
            Some(value) => env.insert(key.to_os_string(), value.to_os_string()),
            None => env.remove(key),
        };
    }
    let mut envp = Vec::with_capacity(env.len());
    for (key, value) in &env {
        let mut pair = key.as_bytes().to_vec();
        pair.push(b'=');
        pair.extend_from_slice(value.as_bytes());
        envp.push(c_string(&pair)?);
    }
    let cwd = cmd
        .get_current_dir()
        .map(|dir| c_string(dir.as_os_str().as_bytes()))
        .transpose()?;
    let null = c_string(b"/dev/null")?;

    let mut setup = SpawnSetup::new()?;
    // The parent's end of each piped stream, and the child's end, which this
    // process closes once the child holds its copy.
    let mut parent_ends: [Option<std::fs::File>; 3] = [None, None, None];
    let mut child_ends: Vec<OwnedFd> = Vec::new();
    for (fd, io) in [streams.stdin, streams.stdout, streams.stderr]
        .into_iter()
        .enumerate()
    {
        let fd = fd as libc::c_int;
        // SAFETY: `setup.actions` is initialised, and every pointer passed is
        // valid for the call.
        unsafe {
            match io {
                Io::Inherit => spawn_check(posix_spawn_file_actions_addinherit_np(
                    &mut setup.actions,
                    fd,
                ))?,
                Io::Null => {
                    let flags = if fd == 0 {
                        libc::O_RDONLY
                    } else {
                        libc::O_WRONLY
                    };
                    spawn_check(libc::posix_spawn_file_actions_addopen(
                        &mut setup.actions,
                        fd,
                        null.as_ptr(),
                        flags,
                        0,
                    ))?;
                }
                Io::Piped => {
                    let (read, write) = nix::unistd::pipe()?;
                    let (parent, child) = if fd == 0 {
                        (write, read)
                    } else {
                        (read, write)
                    };
                    nix::fcntl::fcntl(
                        &parent,
                        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
                    )?;
                    spawn_check(libc::posix_spawn_file_actions_adddup2(
                        &mut setup.actions,
                        child.as_raw_fd(),
                        fd,
                    ))?;
                    parent_ends[fd as usize] = Some(parent.into());
                    child_ends.push(child);
                }
            }
        }
    }
    if let Some(dir) = &cwd {
        // SAFETY: as above.
        spawn_check(unsafe {
            posix_spawn_file_actions_addchdir_np(&mut setup.actions, dir.as_ptr())
        })?;
    }

    let mut defaults = super::managed();
    defaults.add(nix::sys::signal::Signal::SIGPIPE);
    let empty = nix::sys::signal::SigSet::empty();
    let flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT
        | libc::POSIX_SPAWN_SETSIGMASK
        | libc::POSIX_SPAWN_SETSIGDEF;
    // SAFETY: `setup.attrs` is initialised, and the two sets outlive the calls,
    // which copy them.
    unsafe {
        spawn_check(libc::posix_spawnattr_setflags(
            &mut setup.attrs,
            flags as libc::c_short,
        ))?;
        spawn_check(libc::posix_spawnattr_setsigmask(
            &mut setup.attrs,
            empty.as_ref(),
        ))?;
        spawn_check(libc::posix_spawnattr_setsigdefault(
            &mut setup.attrs,
            defaults.as_ref(),
        ))?;
    }

    let mut argv_ptrs: Vec<*mut libc::c_char> =
        argv.iter().map(|a| a.as_ptr().cast_mut()).collect();
    argv_ptrs.push(std::ptr::null_mut());
    let mut envp_ptrs: Vec<*mut libc::c_char> =
        envp.iter().map(|e| e.as_ptr().cast_mut()).collect();
    envp_ptrs.push(std::ptr::null_mut());
    let mut pid: libc::pid_t = 0;
    // SAFETY: every pointer is to a live, NUL-terminated string or a
    // null-terminated array of them, and `setup` holds initialised actions and
    // attributes. `posix_spawnp` reads them and writes the pid.
    spawn_check(unsafe {
        libc::posix_spawnp(
            &mut pid,
            program.as_ptr(),
            &setup.actions,
            &setup.attrs,
            argv_ptrs.as_ptr(),
            envp_ptrs.as_ptr(),
        )
    })?;
    drop(child_ends);
    Ok((pid as u32, parent_ends))
}
