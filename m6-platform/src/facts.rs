//! What the host says it is, read once when a service starts.
//!
//! [`host_facts`] answers the static facts a start-up record needs: the host
//! name, the interface addresses, the OS and kernel, the CPU and memory, the
//! limits the process depends on, the clock and the virtualisation type. Every
//! one of them is read here and nowhere else, and every one is read once.
//! No value from this module is on a request path, and nothing in it takes a
//! threshold, decides what is healthy, or fails.
//!
//! # A fact that cannot be read is unknown, not an error
//!
//! The host is allowed to say nothing. A container can hide `/proc` and
//! `/sys`, `/etc/os-release` can be absent, a kernel can decline to name a
//! clock source. None of that is a reason for a service not to start, so
//! [`host_facts`] cannot fail and does not panic. Each answer is a [`Fact`]:
//! either [`Fact::Known`] with the value, or [`Fact::Unknown`] with a short
//! reason, so a reader can say *why* it does not know rather than leaving the
//! key out.
//!
//! A group of facts read from more than one source keeps its members honest.
//! [`CpuFacts::model`] and [`CpuFacts::count`] are two reads, so each is a
//! [`Fact`] of its own. The same is true of [`MemoryFacts`], [`ClockFacts`],
//! [`Limits`] and [`OsFacts::version`]: a host that will say how much memory it
//! has but not how much is free must not lose the first answer to the second.
//!
//! # Where the answers come from
//!
//! The reads that are the same on both supported families are here: the host
//! name and the canonical name, `uname`, the interface table, the page size,
//! the time zone, the open-file limit, and a file reader that turns a missing
//! file into an unknown fact. The reads that differ are one function each in
//! `linux.rs` and `macos.rs`: `os_facts`, `cpu`, `memory`, `limits`, `clock`
//! and `virtualization`. Table 1 of `docs/m6-platform.md` states each one.
//!
//! Linux reads `/etc/os-release`, `/proc` and `/sys`. macOS reads
//! `SystemVersion.plist` and the kernel through `sysctlbyname`, because it has
//! none of the three. Nothing here parses JSON, starts a process or reads the
//! network, so one call is a fixed number of small file reads and system calls.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use super::os;

// ── A fact ───────────────────────────────────────────────────────────────────

/// A fact that was either read or could not be. `Unknown` carries the reason
/// and is never an error: a start-up must not fail because the host will not
/// say.
///
/// The reason is a short static phrase naming what was tried, for example
/// `"cannot read /proc/meminfo"`. A monitoring service prints it beside the
/// fact's name, so an operator can tell a host that hides `/proc` from a host
/// that reports nothing.
///
/// Serialises as `{"known": value}` or `{"unknown": "reason"}`, so every key
/// is present in a start-up record and an unknown fact is never an omitted
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "a fact is an answer, and dropping it loses the reason it is unknown"]
pub enum Fact<T> {
    /// Read from the host.
    Known(T),
    /// Not read, with the reason.
    Unknown(&'static str),
}

impl<T> Fact<T> {
    /// The value, or `None` when the fact is unknown.
    pub fn known(&self) -> Option<&T> {
        match self {
            Fact::Known(value) => Some(value),
            Fact::Unknown(_) => None,
        }
    }

    /// The reason the fact is unknown, or `None` when it is known.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Fact::Known(_) => None,
            Fact::Unknown(reason) => Some(reason),
        }
    }

    /// Whether the fact was read.
    pub fn is_known(&self) -> bool {
        matches!(self, Fact::Known(_))
    }

    /// Apply `f` to a known value, leaving an unknown fact alone.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Fact<U> {
        match self {
            Fact::Known(value) => Fact::Known(f(value)),
            Fact::Unknown(reason) => Fact::Unknown(reason),
        }
    }
}

impl<T: Serialize> Serialize for Fact<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fact = serializer.serialize_struct("Fact", 1)?;
        match self {
            Fact::Known(value) => fact.serialize_field("known", value)?,
            Fact::Unknown(reason) => fact.serialize_field("unknown", reason)?,
        }
        fact.end()
    }
}

// ── The snapshot ─────────────────────────────────────────────────────────────

/// Everything a service learns about its host once, at start.
///
/// Every field is a fact or a list. The addresses are an ordinary `Vec`
/// because an empty list is a complete answer: a host with no configured
/// interface is unusual, and a host whose interface table cannot be read looks
/// the same through this API, which is why nothing calls that a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostFacts {
    /// The system host name.
    pub hostname: Fact<String>,
    /// The fully qualified name, when the host has one.
    pub fqdn: Fact<String>,
    /// Every address assigned to an interface, with the interface's name.
    pub addresses: Vec<InterfaceAddress>,
    /// The operating system's product name and version.
    pub os: Fact<OsFacts>,
    /// The kernel's release and version, from `uname -r` and `uname -v`.
    pub kernel: Fact<KernelFacts>,
    /// The CPU model and the number of CPUs this process may run on.
    pub cpu: CpuFacts,
    /// Total memory, the page size and whether huge pages are available.
    pub memory: MemoryFacts,
    /// The limits the process depends on.
    pub limits: Limits,
    /// The time zone, the clock source and the boot time.
    pub clock: ClockFacts,
    /// The virtualisation type, when the host can name one.
    pub virtualization: Fact<String>,
}

/// One address on one interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InterfaceAddress {
    /// The interface's name, such as `eth0`, `en0`, `wg0` or `utun3`.
    pub name: String,
    /// The address assigned to it.
    pub address: IpAddr,
    /// What kind of address this is, from the address and the name.
    pub scope: AddressScope,
}

/// What an interface address is for, as far as this layer can tell.
///
/// A name that begins `wg` and a number, or `utun`, is a WireGuard tunnel on
/// the hosts m6 is deployed on. The kernel has no stable flag that says
/// "WireGuard", so the name is the one signal available and it is documented
/// here rather than guessed silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressScope {
    /// The loopback interface or a loopback address.
    Loopback,
    /// A link-local address, `169.254.0.0/16` or `fe80::/10`.
    LinkLocal,
    /// A private address, RFC 1918 or a unique local IPv6 address.
    Private,
    /// Any other address, reachable from outside the host's own networks.
    Public,
    /// A WireGuard tunnel address, whatever its range.
    WireGuard,
}

/// The operating system's product name and version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OsFacts {
    /// `/etc/os-release`'s `NAME` on Linux, `ProductName` on macOS.
    pub name: String,
    /// `VERSION_ID` on Linux, `ProductVersion` on macOS. A rolling release
    /// has no `VERSION_ID`, and that is the only reason it is a [`Fact`].
    pub version: Fact<String>,
}

/// The kernel, from `uname`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KernelFacts {
    /// `uname -r`, for example `6.8.0-45-generic` or `25.5.0`.
    pub release: String,
    /// `uname -v`, the build string.
    pub version: String,
}

/// The CPUs this process may run on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CpuFacts {
    /// The model's name, for example `Apple M4` or `AMD EPYC 7B13`.
    pub model: Fact<String>,
    /// The number of logical CPUs this process may run on, which respects CPU
    /// affinity and a cgroup quota. It is not the number of CPUs in the box.
    pub count: Fact<usize>,
}

/// What the host has for memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryFacts {
    /// The machine's total memory in bytes. Inside a container this is the
    /// container's remembered host total, not the limit, which is in
    /// [`Limits::cgroup_memory`].
    pub total_bytes: Fact<u64>,
    /// The size of one memory page in bytes: 4 KiB on Linux x86_64, 16 KiB on
    /// macOS arm64. The unit m6-core's bounded buffers are sized in.
    pub page_size: Fact<u64>,
    /// Whether the host hands out huge pages, and whether that takes a hint.
    pub huge_pages: Fact<HugePages>,
}

/// How the kernel gives a process huge pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HugePages {
    /// Every mapping that can use one gets one, with no hint from the process.
    Always,
    /// Only a range the process advises gets them.
    MAdvise,
    /// The host has the support and has turned it off.
    Never,
}

/// The static limits a service depends on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Limits {
    /// The soft `RLIMIT_MEMLOCK` in bytes. `u64::MAX` is unlimited. macOS has
    /// no such limit, so it is unknown there.
    pub memlock: Fact<u64>,
    /// The soft `RLIMIT_NOFILE`.
    pub nofile: Fact<u64>,
    /// `net.core.somaxconn`, the kernel's cap on a listen backlog.
    pub somaxconn: Fact<u32>,
    /// The cgroup memory limit in bytes. [`Fact::Known`] with `None` means the
    /// cgroup sets no limit, which is a complete answer. [`Fact::Unknown`]
    /// means the cgroup files could not be read.
    pub cgroup_memory: Fact<Option<u64>>,
    /// The cgroup CPU quota. [`Fact::Known`] with `None` means the cgroup sets
    /// no quota. [`Fact::Unknown`] means the cgroup files could not be read.
    pub cgroup_cpu: Fact<Option<CpuLimit>>,
}

/// A cgroup CPU quota, in the kernel's microsecond units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CpuLimit {
    /// The quota the group may use in one period.
    pub quota_micros: u64,
    /// The period the quota applies to.
    pub period_micros: u64,
}

/// The clock, as the host describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockFacts {
    /// The time zone's name, for example `Australia/Sydney`, from `TZ`,
    /// `/etc/localtime` or `/etc/timezone`.
    pub timezone: Fact<String>,
    /// The kernel's clock source, from `/sys` on Linux. macOS does not report
    /// one, so it is unknown there.
    pub source: Fact<String>,
    /// When the machine booted, from `/proc/stat` on Linux and
    /// `kern.boottime` on macOS.
    pub boot_time: Fact<SystemTime>,
}

/// Serialise the boot time as RFC 3339 in UTC, so a reader of the record does
/// not have to know when the epoch was. [`super::write_rfc3339_utc`] is the
/// formatter the log line uses, so there is one date text in m6 rather than
/// two. An unknown boot time still serialises as an unknown fact with its
/// reason.
impl Serialize for ClockFacts {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut clock = serializer.serialize_struct("ClockFacts", 3)?;
        clock.serialize_field("timezone", &self.timezone)?;
        clock.serialize_field("source", &self.source)?;
        clock.serialize_field("boot_time", &BootTime(&self.boot_time))?;
        clock.end()
    }
}

struct BootTime<'a>(&'a Fact<SystemTime>);

impl Serialize for BootTime<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fact = serializer.serialize_struct("Fact", 1)?;
        match self.0 {
            Fact::Known(time) => {
                let seconds = time
                    .duration_since(UNIX_EPOCH)
                    .map(|since| since.as_secs())
                    .unwrap_or(0);
                let mut text = [0u8; 20];
                super::write_rfc3339_utc(seconds, &mut text);
                let text = std::str::from_utf8(&text[..19]).unwrap_or_default();
                fact.serialize_field("known", &format!("{text}Z"))?;
            }
            Fact::Unknown(reason) => fact.serialize_field("unknown", reason)?,
        }
        fact.end()
    }
}

// ── The one call ─────────────────────────────────────────────────────────────

/// Read every host fact once.
///
/// Never fails and never panics. Each answer is [`Fact::Known`] or
/// [`Fact::Unknown`] with a reason. Call it once, at start, and pass the result
/// down: it is not on a request path and it must not become one.
///
/// The cost is a fixed set of small reads: about a dozen files and four system
/// calls on Linux, and six `sysctl` calls plus three system calls on macOS,
/// plus one resolver lookup when the host name has no domain part. Measured on
/// an idle macOS arm64 host it is 150 to 200 microseconds once warm and 300 to
/// 700 microseconds on the first call, and the interface table is the largest
/// single read at 55 to 85 microseconds. The one read that can block is that
/// resolver lookup, and it is skipped whenever the host name already contains
/// a dot.
pub fn host_facts() -> HostFacts {
    let hostname = hostname();
    HostFacts {
        fqdn: fqdn(&hostname),
        hostname,
        addresses: interfaces(),
        os: os::host_os_facts(),
        kernel: kernel(),
        cpu: os::host_cpu(),
        memory: os::host_memory(),
        limits: os::host_limits(),
        clock: os::host_clock(),
        virtualization: os::host_virtualization(),
    }
}

// ── The reads both families share ────────────────────────────────────────────

/// The system host name, through `gethostname`.
fn hostname() -> Fact<String> {
    match nix::unistd::gethostname() {
        Ok(name) => match name.into_string() {
            Ok(name) if !name.is_empty() => Fact::Known(name),
            Ok(_) => Fact::Unknown("the host name is empty"),
            Err(_) => Fact::Unknown("the host name is not UTF-8"),
        },
        Err(_) => Fact::Unknown("gethostname failed"),
    }
}

/// The fully qualified name.
///
/// A host name that already contains a dot is its own FQDN, and that is the
/// common case on the hosts m6 is deployed on, so no lookup is made. Otherwise
/// the resolver is asked for the canonical name, which is one lookup at start.
/// A name with no domain part is an ordinary answer for a host that has none.
fn fqdn(hostname: &Fact<String>) -> Fact<String> {
    let Some(name) = hostname.known() else {
        return Fact::Unknown("the host name could not be read");
    };
    if name.contains('.') {
        return Fact::Known(name.clone());
    }
    let Ok(name) = std::ffi::CString::new(name.as_str()) else {
        return Fact::Unknown("the host name holds a NUL byte");
    };
    let mut hints: nix::libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_family = nix::libc::AF_UNSPEC;
    hints.ai_socktype = nix::libc::SOCK_STREAM;
    hints.ai_flags = nix::libc::AI_CANONNAME;
    let mut result: *mut nix::libc::addrinfo = std::ptr::null_mut();
    // SAFETY: every pointer is either null or points to a live value of the
    // type the call expects, and the hints are fully initialised above. The
    // list the call returns is freed once it has been read.
    let rc =
        unsafe { nix::libc::getaddrinfo(name.as_ptr(), std::ptr::null(), &hints, &mut result) };
    if rc != 0 || result.is_null() {
        return Fact::Unknown("the host name did not resolve to a canonical name");
    }
    let mut canonical = None;
    let mut node = result;
    while !node.is_null() {
        // SAFETY: `node` is a node of the list `getaddrinfo` returned.
        let info = unsafe { &*node };
        if !info.ai_canonname.is_null() {
            // SAFETY: `ai_canonname` is a NUL terminated string when set.
            let name = unsafe { std::ffi::CStr::from_ptr(info.ai_canonname) };
            let name = name.to_string_lossy().into_owned();
            if name.contains('.') {
                canonical = Some(name);
                break;
            }
        }
        node = info.ai_next;
    }
    // SAFETY: `result` is the list `getaddrinfo` returned and has not been
    // freed. Every node was only read.
    unsafe { nix::libc::freeaddrinfo(result) };
    match canonical {
        Some(name) => Fact::Known(name),
        None => Fact::Unknown("the host has no domain name"),
    }
}

/// The kernel's release and version, through `uname`.
fn kernel() -> Fact<KernelFacts> {
    let uts = match nix::sys::utsname::uname() {
        Ok(uts) => uts,
        Err(_) => return Fact::Unknown("uname failed"),
    };
    let release = uts.release().to_string_lossy().into_owned();
    let version = uts.version().to_string_lossy().into_owned();
    if release.is_empty() {
        return Fact::Unknown("uname reported no kernel release");
    }
    Fact::Known(KernelFacts { release, version })
}

/// Every address on every interface, through `getifaddrs`.
///
/// Empty when the call fails, because the contract for the list is a list. The
/// rows are sorted by interface name and then by address, so two runs on one
/// host agree.
fn interfaces() -> Vec<InterfaceAddress> {
    let mut list: *mut nix::libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` writes a list of nodes into `list` on success, and
    // the list is freed below. A failure returns non-zero and leaves it null.
    if unsafe { nix::libc::getifaddrs(&mut list) } != 0 {
        return Vec::new();
    }
    let mut addresses = Vec::new();
    let mut node = list;
    while !node.is_null() {
        // SAFETY: `node` is a node of the list `getifaddrs` returned, and
        // `ifa_next` is valid until the null terminator.
        let info = unsafe { &*node };
        if !info.ifa_addr.is_null() {
            // SAFETY: `ifa_addr` is a live `sockaddr` whose family says how
            // much of it the kernel filled in.
            let family = unsafe { (*info.ifa_addr).sa_family as i32 };
            let address = match family {
                // SAFETY: the family is `AF_INET`, so the storage is a
                // `sockaddr_in`.
                nix::libc::AF_INET => unsafe {
                    let sin = &*(info.ifa_addr as *const nix::libc::sockaddr_in);
                    Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                        sin.sin_addr.s_addr,
                    ))))
                },
                // SAFETY: the family is `AF_INET6`, so the storage is a
                // `sockaddr_in6`.
                nix::libc::AF_INET6 => unsafe {
                    let sin6 = &*(info.ifa_addr as *const nix::libc::sockaddr_in6);
                    Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
                },
                _ => None,
            };
            if let Some(address) = address {
                let name = if info.ifa_name.is_null() {
                    String::new()
                } else {
                    // SAFETY: `ifa_name` is a NUL terminated string when set.
                    unsafe { std::ffi::CStr::from_ptr(info.ifa_name) }
                        .to_string_lossy()
                        .into_owned()
                };
                let loopback = info.ifa_flags & (nix::libc::IFF_LOOPBACK as u32) != 0;
                let scope = scope_of(&name, address, loopback);
                addresses.push(InterfaceAddress {
                    name,
                    address,
                    scope,
                });
            }
        }
        node = info.ifa_next;
    }
    // SAFETY: `list` is the head of the list `getifaddrs` returned.
    unsafe { nix::libc::freeifaddrs(list) };
    addresses.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.address.cmp(&b.address)));
    addresses.dedup();
    addresses
}

/// What an address is for, from its name and its range.
fn scope_of(name: &str, address: IpAddr, loopback_flag: bool) -> AddressScope {
    if loopback_flag || address.is_loopback() {
        return AddressScope::Loopback;
    }
    if is_tunnel(name) {
        return AddressScope::WireGuard;
    }
    match address {
        IpAddr::V4(v4) if v4.is_private() => AddressScope::Private,
        IpAddr::V4(v4) if v4.is_link_local() => AddressScope::LinkLocal,
        IpAddr::V6(v6) if v6.is_unique_local() => AddressScope::Private,
        IpAddr::V6(v6) if v6.is_unicast_link_local() => AddressScope::LinkLocal,
        _ => AddressScope::Public,
    }
}

/// Whether an interface's name is the one WireGuard uses on the hosts m6 is
/// deployed on: `wg0` on Linux, `utun3` on macOS through wireguard-go.
fn is_tunnel(name: &str) -> bool {
    match name.strip_prefix("wg") {
        Some(number) => !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()),
        None => name.starts_with("utun"),
    }
}

/// The size of one memory page, through `sysconf`.
pub(super) fn page_size() -> Fact<u64> {
    match nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE) {
        Ok(Some(size)) if size > 0 => Fact::Known(size as u64),
        Ok(_) => Fact::Unknown("sysconf reported no page size"),
        Err(_) => Fact::Unknown("sysconf(PAGE_SIZE) failed"),
    }
}

/// The number of logical CPUs this process may run on.
pub(super) fn cpu_count() -> Fact<usize> {
    match std::thread::available_parallelism() {
        Ok(count) => Fact::Known(count.get()),
        Err(_) => Fact::Unknown("available_parallelism failed"),
    }
}

/// The soft open-file limit. The same call on both supported families.
pub(super) fn nofile() -> Fact<u64> {
    match nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE) {
        Ok((soft, _hard)) => Fact::Known(soft),
        Err(_) => Fact::Unknown("getrlimit(RLIMIT_NOFILE) failed"),
    }
}

/// The time zone, from `TZ`, then the `/etc/localtime` link, then
/// `/etc/timezone`.
///
/// `TZ` wins because it is the setting the process would actually use. A value
/// with a leading colon is the POSIX form that names a zone file. A zone name
/// is accepted from any of the three, and the two hosts m6 is deployed on put
/// the link under different prefixes, so both are stripped.
pub(super) fn timezone() -> Fact<String> {
    if let Some(tz) = std::env::var_os("TZ") {
        let tz = tz.to_string_lossy();
        let tz = tz.strip_prefix(':').unwrap_or(&tz);
        if is_zone_name(tz) {
            return Fact::Known(tz.to_string());
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let target = target.to_string_lossy();
        for prefix in [
            "/var/db/timezone/zoneinfo/",
            "/usr/share/zoneinfo/",
            "/usr/lib/zoneinfo/",
        ] {
            if let Some(zone) = target.strip_prefix(prefix) {
                if is_zone_name(zone) {
                    return Fact::Known(zone.to_string());
                }
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string("/etc/timezone") {
        let zone = text.trim();
        if is_zone_name(zone) {
            return Fact::Known(zone.to_string());
        }
    }
    Fact::Unknown("no time zone is set")
}

/// Whether a string looks like an IANA zone name: letters, digits, and the
/// separators a zone name uses. `UTC` and `Australia/Sydney` both pass, and a
/// POSIX rule such as `GMT0BST` passes as well, which is correct because it is
/// still the zone the process would use.
fn is_zone_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 128
        && !name.starts_with('/')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'))
}

// ── Turning a read into a fact ───────────────────────────────────────────────

/// Read one file once. A file that cannot be read is an unknown fact whose
/// reason is `unreadable`, which the caller writes for the file it asked for.
///
/// This is the fake `/proc` path a container has, and it is the one every
/// reader above goes through, so a host that hides a file is a named unknown
/// rather than a panic.
pub(super) fn read_file(path: &str, unreadable: &'static str) -> Fact<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Fact::Known(text),
        Err(_) => Fact::Unknown(unreadable),
    }
}

/// Parse what a file said into a fact. A parse that finds nothing is an
/// unknown fact whose reason is `unparsable`, and an unreadable file keeps its
/// own reason.
pub(super) fn parse<T>(
    text: Fact<String>,
    parser: fn(&str) -> Option<T>,
    unparsable: &'static str,
) -> Fact<T> {
    match text {
        Fact::Known(text) => match parser(&text) {
            Some(value) => Fact::Known(value),
            None => Fact::Unknown(unparsable),
        },
        Fact::Unknown(reason) => Fact::Unknown(reason),
    }
}

// ── The parsers both families share, and their tests ─────────────────────────
//
// Every parser is a pure function over text, so it is held on any host with
// the text a real host produces. A parser returns `None` when the text does
// not contain the fact, and the reader above turns that into `Unknown` with a
// reason. A wrong parser therefore shows up as an unknown fact, never as a
// zero.

/// `/etc/os-release`'s `NAME` and `VERSION_ID`. The values are usually
/// double-quoted and may be single-quoted. A line without the two keys is not
/// the fact.
pub fn parse_os_release(text: &str) -> Option<OsFacts> {
    let mut name = None;
    let mut version = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "NAME" if !value.is_empty() => name = Some(value.to_string()),
            "VERSION_ID" if !value.is_empty() => version = Some(value.to_string()),
            _ => {}
        }
    }
    Some(OsFacts {
        name: name?,
        version: match version {
            Some(version) => Fact::Known(version),
            None => Fact::Unknown("no VERSION_ID in the OS release file"),
        },
    })
}

/// A plist's string value for `key`, from the XML `SystemVersion.plist` uses.
/// The scanner is deliberately small: the file is Apple's, its shape is fixed,
/// and the two keys wanted are plain sibling entries.
pub fn parse_system_version(text: &str) -> Option<OsFacts> {
    let name = plist_string(text, "ProductName")?;
    Some(OsFacts {
        name,
        version: match plist_string(text, "ProductVersion") {
            Some(version) => Fact::Known(version),
            None => Fact::Unknown("no ProductVersion in SystemVersion.plist"),
        },
    })
}

fn plist_string(text: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let after = &text[text.find(&needle)? + needle.len()..];
    let open = after.find("<string>")? + "<string>".len();
    let close = after[open..].find("</string>")? + open;
    let value = after[open..close].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The CPU model from `/proc/cpuinfo`. x86 names it `model name`, arm64 uses
/// `model name`, `Model`, `Hardware` or `Processor` depending on the kernel,
/// so the keys are tried in the order that names a model rather than a
/// revision.
pub fn parse_cpuinfo_model(text: &str) -> Option<String> {
    const KEYS: [&str; 5] = ["model name", "Model", "Hardware", "Processor", "cpu model"];
    for key in KEYS {
        for line in text.lines() {
            let Some((found, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            if found.trim() == key && !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// `MemTotal` from `/proc/meminfo`, in bytes. The kernel's `kB` label means
/// KiB, so the number is multiplied by 1024.
pub fn parse_meminfo_total(text: &str) -> Option<u64> {
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if key.trim() != "MemTotal" {
            continue;
        }
        let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
        return Some(kib.saturating_mul(1024));
    }
    None
}

/// The active setting of `/sys/kernel/mm/transparent_hugepage/enabled`, which
/// writes all three settings with the active one in square brackets:
/// `always [madvise] never`.
pub fn parse_hugepage_setting(text: &str) -> Option<HugePages> {
    let text = text.trim();
    let active = match (text.find('['), text.find(']')) {
        (Some(open), Some(close)) if open < close => &text[open + 1..close],
        _ => text,
    };
    match active.trim() {
        "always" => Some(HugePages::Always),
        "madvise" => Some(HugePages::MAdvise),
        "never" => Some(HugePages::Never),
        _ => None,
    }
}

/// `btime` from `/proc/stat`, in seconds since the epoch.
pub fn parse_btime(text: &str) -> Option<SystemTime> {
    let line = text.lines().find(|line| line.starts_with("btime "))?;
    let seconds: u64 = line.strip_prefix("btime ")?.trim().parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

/// cgroup v2's `memory.max`. `max` means no limit applies, which is a complete
/// answer and not an unknown one.
pub fn parse_cgroup_memory_max(text: &str) -> Option<Option<u64>> {
    let text = text.trim();
    if text == "max" {
        return Some(None);
    }
    text.parse::<u64>().ok().map(Some)
}

/// cgroup v1's `memory.limit_in_bytes`. The kernel writes a number near 2^63
/// for "no limit", so a value above 2^60 is read as no limit rather than as an
/// 8 EiB machine.
pub fn parse_cgroup_memory_limit_in_bytes(text: &str) -> Option<Option<u64>> {
    const UNLIMITED: u64 = 1 << 60;
    let bytes: u64 = text.trim().parse().ok()?;
    Some(if bytes >= UNLIMITED {
        None
    } else {
        Some(bytes)
    })
}

/// cgroup v2's `cpu.max`, which is `<quota> <period>` or `max <period>`.
pub fn parse_cgroup_cpu_max(text: &str) -> Option<Option<CpuLimit>> {
    let mut fields = text.split_whitespace();
    let quota = fields.next()?;
    if quota == "max" {
        return Some(None);
    }
    let quota: u64 = quota.parse().ok()?;
    let period: u64 = fields.next()?.parse().ok()?;
    if period == 0 {
        return None;
    }
    Some(Some(CpuLimit {
        quota_micros: quota,
        period_micros: period,
    }))
}

/// cgroup v1's `cpu.cfs_quota_us` and `cpu.cfs_period_us`. A quota of -1 is
/// the v1 spelling of "no limit".
pub fn parse_cgroup_cpu_v1(quota: &str, period: &str) -> Option<Option<CpuLimit>> {
    let quota: i64 = quota.trim().parse().ok()?;
    let period: u64 = period.trim().parse().ok()?;
    if period == 0 {
        return None;
    }
    if quota <= 0 {
        return Some(None);
    }
    Some(Some(CpuLimit {
        quota_micros: quota as u64,
        period_micros: period,
    }))
}

/// The virtualisation type from the DMI strings and the CPU's `hypervisor`
/// flag.
///
/// The DMI vendor is the name the hypervisor publishes, so it is matched
/// case-insensitively against the ones m6 is deployed under. A flag with no
/// matching name still says a hypervisor is there, and a host whose DMI could
/// be read with no match is a physical machine. `None` means nothing was
/// readable, which the caller reports as unknown.
pub fn parse_virtualization(
    vendor: Option<&str>,
    product: Option<&str>,
    hypervisor_flag: bool,
) -> Option<String> {
    let names = format!("{} {}", vendor.unwrap_or(""), product.unwrap_or(""));
    let names = names.to_ascii_lowercase();
    const KINDS: [(&str, &str); 13] = [
        ("kvm", "kvm"),
        ("qemu", "qemu"),
        ("vmware", "vmware"),
        ("xen", "xen"),
        ("microsoft", "hyperv"),
        ("virtualbox", "virtualbox"),
        ("innotek", "virtualbox"),
        ("parallels", "parallels"),
        ("amazon", "amazon"),
        ("google", "google"),
        ("digitalocean", "digitalocean"),
        ("bochs", "bochs"),
        ("openstack", "openstack"),
    ];
    for (needle, kind) in KINDS {
        if names.contains(needle) {
            return Some(kind.to_string());
        }
    }
    if hypervisor_flag {
        return Some("hypervisor".to_string());
    }
    if vendor.is_some() || product.is_some() {
        return Some("none".to_string());
    }
    None
}

/// Whether `/proc/cpuinfo` carries the `hypervisor` CPU flag.
pub fn parse_hypervisor_flag(cpuinfo: &str) -> bool {
    cpuinfo.lines().any(|line| {
        line.strip_prefix("flags")
            .is_some_and(|flags| flags.split_whitespace().any(|flag| flag == "hypervisor"))
    })
}

/// Strip one pair of matching single or double quotes.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(rest) = value.strip_prefix(quote) {
            if let Some(rest) = rest.strip_suffix(quote) {
                return rest;
            }
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reason a fact is unknown is never empty, because it is what the
    /// monitor prints beside the key.
    #[test]
    fn an_unknown_fact_always_carries_a_reason() {
        let fact: Fact<u64> = Fact::Unknown("cannot read /proc/stat");
        assert_eq!(fact.reason(), Some("cannot read /proc/stat"));
        assert!(!fact.is_known());
        assert_eq!(fact.known(), None);
        assert_eq!(Fact::Known(7u64).known(), Some(&7));
    }

    /// A file the host does not have is an unknown fact naming the file, which
    /// is the case a container with half of `/proc` missing puts every reader
    /// in. This is the fake `/proc` path, spelled as a path no host has.
    #[test]
    fn a_missing_file_is_unknown_with_the_reason_it_was_given() {
        let stat = read_file("/nonexistent/proc/stat", "cannot read /proc/stat");
        assert_eq!(stat.reason(), Some("cannot read /proc/stat"));
        let parsed = parse(stat, parse_btime, "/proc/stat has no btime");
        assert_eq!(parsed.reason(), Some("cannot read /proc/stat"));
    }

    /// A file that exists and does not hold the fact is unknown with the
    /// parse's reason, so a host that answers nonsense is not read as zero.
    #[test]
    fn a_file_without_the_fact_is_unknown_with_the_parse_reason() {
        let empty = read_file("/dev/null", "cannot read /dev/null");
        assert!(
            empty.is_known(),
            "reading /dev/null succeeds on both targets"
        );
        let memory = parse(empty, parse_meminfo_total, "/proc/meminfo has no MemTotal");
        assert_eq!(memory.reason(), Some("/proc/meminfo has no MemTotal"));
    }

    /// The host name is either read or reported, never a panic, and the two
    /// targets both have one.
    #[test]
    fn the_host_name_is_known_or_unknown() {
        match hostname() {
            Fact::Known(name) => assert!(!name.is_empty()),
            Fact::Unknown(reason) => assert!(!reason.is_empty()),
        }
    }

    /// The kernel is either read or reported, never a panic.
    #[test]
    fn the_kernel_is_known_or_unknown() {
        match kernel() {
            Fact::Known(kernel) => assert!(!kernel.release.is_empty()),
            Fact::Unknown(reason) => assert!(!reason.is_empty()),
        }
    }

    /// Address enumeration returns a list, which may legitimately be empty, and
    /// every row it does return is complete. No address is required to exist.
    #[test]
    fn addresses_may_be_empty_and_every_row_is_complete() {
        let addresses = interfaces();
        for address in &addresses {
            assert!(!address.name.is_empty(), "{address:?}");
            assert!(!address.address.is_unspecified(), "{address:?}");
        }
        // The host this runs on has a loopback address, so the reader is
        // exercised. The assertion is not that the list is non-empty: a
        // sandbox can hide the interface table, and the contract allows it.
        if !addresses.is_empty() {
            assert!(
                addresses
                    .iter()
                    .all(|a| a.scope != AddressScope::Public || !a.address.is_loopback()),
                "{addresses:?}"
            );
        }
    }

    /// The scope rules, which are pure and so are held on any host.
    #[test]
    fn a_private_address_is_private_and_a_tunnel_is_wireguard() {
        let v4 = |s: &str| IpAddr::V4(s.parse().expect("an IPv4 address"));
        let v6 = |s: &str| IpAddr::V6(s.parse().expect("an IPv6 address"));
        assert_eq!(
            scope_of("lo0", v4("127.0.0.1"), false),
            AddressScope::Loopback
        );
        assert_eq!(
            scope_of("eth0", v4("10.1.2.3"), false),
            AddressScope::Private
        );
        assert_eq!(
            scope_of("eth0", v4("172.16.4.5"), false),
            AddressScope::Private
        );
        assert_eq!(
            scope_of("eth0", v4("192.168.1.2"), false),
            AddressScope::Private
        );
        assert_eq!(
            scope_of("eth0", v4("169.254.1.2"), false),
            AddressScope::LinkLocal
        );
        assert_eq!(
            scope_of("eth0", v6("fd00::1"), false),
            AddressScope::Private
        );
        assert_eq!(
            scope_of("eth0", v6("fe80::1"), false),
            AddressScope::LinkLocal
        );
        assert_eq!(
            scope_of("eth0", v4("203.0.113.9"), false),
            AddressScope::Public
        );
        assert_eq!(
            scope_of("wg0", v4("10.9.0.1"), false),
            AddressScope::WireGuard
        );
        assert_eq!(
            scope_of("utun3", v4("10.9.0.1"), false),
            AddressScope::WireGuard
        );
        assert_eq!(
            scope_of("en0", v4("10.9.0.1"), false),
            AddressScope::Private
        );
    }

    /// A name that only looks like a tunnel name is not one.
    #[test]
    fn only_the_wireguard_names_are_tunnels() {
        assert!(is_tunnel("wg0"));
        assert!(is_tunnel("wg12"));
        assert!(is_tunnel("utun3"));
        assert!(!is_tunnel("wgbad"));
        assert!(!is_tunnel("wg"));
        assert!(!is_tunnel("tun0"));
        assert!(!is_tunnel("en0"));
    }

    /// A real `/etc/os-release`, with values quoted the way the file has them.
    #[test]
    fn parses_a_real_os_release() {
        let text = "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\n\
                    NAME=\"Ubuntu\"\n\
                    VERSION_ID=\"24.04\"\n\
                    VERSION=\"24.04.1 LTS (Noble Numbat)\"\n\
                    ID=ubuntu\n";
        let os = parse_os_release(text).expect("Ubuntu's os-release");
        assert_eq!(os.name, "Ubuntu");
        assert_eq!(os.version, Fact::Known("24.04".to_string()));
    }

    /// A rolling release has no `VERSION_ID`. Its name is still a fact, and
    /// the version says why it is not one.
    #[test]
    fn a_rolling_release_keeps_its_name_when_it_has_no_version() {
        let os = parse_os_release("NAME=\"Arch Linux\"\nID=arch\n").expect("Arch's os-release");
        assert_eq!(os.name, "Arch Linux");
        assert_eq!(
            os.version.reason(),
            Some("no VERSION_ID in the OS release file")
        );
    }

    /// A file that is not an os-release at all has no name.
    #[test]
    fn an_os_release_without_a_name_is_none() {
        assert!(parse_os_release("").is_none());
        assert!(parse_os_release("VERSION_ID=\"1\"\n").is_none());
    }

    /// A real `SystemVersion.plist`, from the Mac that built this.
    #[test]
    fn parses_a_real_system_version_plist() {
        let text = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                    <plist version=\"1.0\">\n\
                    <dict>\n\
                    \t<key>ProductBuildVersion</key>\n\
                    \t<string>25F84</string>\n\
                    \t<key>ProductName</key>\n\
                    \t<string>macOS</string>\n\
                    \t<key>ProductVersion</key>\n\
                    \t<string>26.5.2</string>\n\
                    </dict>\n\
                    </plist>\n";
        let os = parse_system_version(text).expect("the Mac's SystemVersion.plist");
        assert_eq!(os.name, "macOS");
        assert_eq!(os.version, Fact::Known("26.5.2".to_string()));
    }

    /// Two real `/proc/cpuinfo` shapes, x86 and arm64.
    #[test]
    fn parses_the_cpu_model_from_both_cpuinfo_shapes() {
        let x86 = "processor\t: 0\nmodel name\t: AMD EPYC 7B13\ncpu MHz\t\t: 2249.998\n";
        assert_eq!(parse_cpuinfo_model(x86), Some("AMD EPYC 7B13".to_string()));
        let arm = "processor\t: 0\nBogoMIPS\t: 48.00\nFeatures\t: fp asimd\n\
                   model name\t: Cortex-A72\n";
        assert_eq!(parse_cpuinfo_model(arm), Some("Cortex-A72".to_string()));
        let hardware = "processor\t: 0\nHardware\t: BCM2835\n";
        assert_eq!(parse_cpuinfo_model(hardware), Some("BCM2835".to_string()));
        assert_eq!(parse_cpuinfo_model("processor\t: 0\n"), None);
    }

    /// A real `/proc/meminfo`: `MemTotal` is in KiB whatever the label says.
    #[test]
    fn meminfo_total_is_kib_times_1024() {
        let text = "MemTotal:         973352 kB\nMemFree:           24992 kB\n";
        assert_eq!(parse_meminfo_total(text), Some(973_352 * 1024));
        assert_eq!(parse_meminfo_total("MemFree: 10 kB\n"), None);
    }

    /// The three transparent huge page settings, with the active one marked.
    #[test]
    fn parses_the_active_huge_page_setting() {
        assert_eq!(
            parse_hugepage_setting("always [madvise] never\n"),
            Some(HugePages::MAdvise)
        );
        assert_eq!(
            parse_hugepage_setting("[always] madvise never\n"),
            Some(HugePages::Always)
        );
        assert_eq!(
            parse_hugepage_setting("always madvise [never]\n"),
            Some(HugePages::Never)
        );
        assert_eq!(parse_hugepage_setting("nonsense"), None);
    }

    /// `btime` from `/proc/stat` is seconds since the epoch.
    #[test]
    fn parses_the_boot_time() {
        let text = "cpu  1 2 3\nbtime 1726000000\nprocesses 1234\n";
        let boot = parse_btime(text).expect("btime");
        assert_eq!(
            boot.duration_since(UNIX_EPOCH).expect("after the epoch"),
            Duration::from_secs(1_726_000_000)
        );
        assert!(parse_btime("cpu 0 0 0\n").is_none());
    }

    /// cgroup v2 states no limit as the word `max`, which is a complete answer
    /// and not an unknown one.
    #[test]
    fn a_cgroup_that_sets_no_limit_is_known_none() {
        assert_eq!(parse_cgroup_memory_max("max\n"), Some(None));
        assert_eq!(
            parse_cgroup_memory_max("536870912\n"),
            Some(Some(536_870_912))
        );
        assert_eq!(parse_cgroup_memory_max("lots\n"), None);
    }

    /// cgroup v1's spelling of no limit, and the 8 EiB sentinel.
    #[test]
    fn cgroup_v1_unlimited_reads_as_no_limit() {
        assert_eq!(
            parse_cgroup_memory_limit_in_bytes("9223372036854771712\n"),
            Some(None)
        );
        assert_eq!(
            parse_cgroup_memory_limit_in_bytes("536870912\n"),
            Some(Some(536_870_912))
        );
    }

    /// cgroup v2's CPU quota, and the word `max`.
    #[test]
    fn parses_a_cgroup_cpu_quota() {
        assert_eq!(
            parse_cgroup_cpu_max("200000 100000\n"),
            Some(Some(CpuLimit {
                quota_micros: 200_000,
                period_micros: 100_000,
            }))
        );
        assert_eq!(parse_cgroup_cpu_max("max 100000\n"), Some(None));
        assert_eq!(parse_cgroup_cpu_max("200000\n"), None);
        assert_eq!(parse_cgroup_cpu_max("200000 0\n"), None);
    }

    /// cgroup v1's quota, where -1 means no limit.
    #[test]
    fn parses_a_cgroup_v1_cpu_quota() {
        assert_eq!(parse_cgroup_cpu_v1("-1\n", "100000\n"), Some(None));
        assert_eq!(
            parse_cgroup_cpu_v1("200000\n", "100000\n"),
            Some(Some(CpuLimit {
                quota_micros: 200_000,
                period_micros: 100_000,
            }))
        );
        assert_eq!(parse_cgroup_cpu_v1("200000\n", "0\n"), None);
    }

    /// The DMI names a hypervisor, a bare machine, and an unnamed hypervisor,
    /// and nothing readable is none of those.
    #[test]
    fn names_the_virtualisation_it_can() {
        assert_eq!(
            parse_virtualization(Some("QEMU"), Some("Standard PC"), false),
            Some("qemu".to_string())
        );
        assert_eq!(
            parse_virtualization(Some("VMware, Inc."), Some("VMware7,1"), false),
            Some("vmware".to_string())
        );
        assert_eq!(
            parse_virtualization(
                Some("Microsoft Corporation"),
                Some("Virtual Machine"),
                false
            ),
            Some("hyperv".to_string())
        );
        assert_eq!(
            parse_virtualization(Some("LENOVO"), Some("ThinkPad"), false),
            Some("none".to_string())
        );
        assert_eq!(
            parse_virtualization(Some("LENOVO"), Some("ThinkPad"), true),
            Some("hypervisor".to_string())
        );
        assert_eq!(
            parse_virtualization(None, None, true),
            Some("hypervisor".to_string())
        );
        assert_eq!(parse_virtualization(None, None, false), None);
    }

    /// The `hypervisor` CPU flag, which is how a hypervisor that publishes no
    /// DMI string is still detectable.
    #[test]
    fn the_hypervisor_flag_is_read_from_the_flags_line() {
        assert!(parse_hypervisor_flag("flags\t\t: fpu vme hypervisor lm\n"));
        assert!(!parse_hypervisor_flag("flags\t\t: fpu vme lm\n"));
        assert!(!parse_hypervisor_flag("processor\t: 0\n"));
    }

    /// A zone name is accepted, a POSIX rule is accepted, and a path or an
    /// empty string is not.
    #[test]
    fn a_zone_name_is_not_a_path() {
        assert!(is_zone_name("Australia/Sydney"));
        assert!(is_zone_name("UTC"));
        assert!(is_zone_name("GMT0BST"));
        assert!(!is_zone_name(""));
        assert!(!is_zone_name("/etc/localtime"));
        assert!(!is_zone_name("Australia Sydney"));
    }

    /// The time zone is either read or reported, never a panic, and a host
    /// with `/etc/localtime` has one on both supported targets.
    #[test]
    fn the_time_zone_is_known_or_unknown() {
        match timezone() {
            Fact::Known(zone) => assert!(is_zone_name(&zone), "{zone}"),
            Fact::Unknown(reason) => assert!(!reason.is_empty()),
        }
    }

    /// The page size and the CPU count are either read or reported.
    #[test]
    fn the_page_size_and_cpu_count_are_known_or_unknown() {
        if let Fact::Known(size) = page_size() {
            assert!(size >= 4096 && size.is_power_of_two(), "{size}");
        }
        if let Fact::Known(count) = cpu_count() {
            assert!(count >= 1, "{count}");
        }
    }

    /// The open-file limit is read on both targets.
    #[test]
    fn the_open_file_limit_is_a_positive_number() {
        match nofile() {
            Fact::Known(limit) => assert!(limit > 0, "{limit}"),
            Fact::Unknown(reason) => assert!(!reason.is_empty()),
        }
    }

    /// The boot time is RFC 3339 in UTC, so the line a person reads does not
    /// ask them to know when the epoch was. An unknown boot time keeps its
    /// reason.
    #[test]
    fn the_boot_time_serialises_as_rfc3339() {
        let clock = ClockFacts {
            timezone: Fact::Unknown("no time zone is set"),
            source: Fact::Unknown("macOS does not report its clock source"),
            boot_time: Fact::Known(UNIX_EPOCH + Duration::from_secs(1_767_225_600)),
        };
        let json = serde_json::to_string(&clock).expect("serialise");
        assert!(
            json.contains(r#""boot_time":{"known":"2026-01-01T00:00:00Z"}"#),
            "{json}"
        );

        let unknown = ClockFacts {
            timezone: Fact::Unknown("no time zone is set"),
            source: Fact::Unknown("macOS does not report its clock source"),
            boot_time: Fact::Unknown("cannot read /proc/stat"),
        };
        let json = serde_json::to_string(&unknown).expect("serialise");
        assert!(
            json.contains(r#""boot_time":{"unknown":"cannot read /proc/stat"}"#),
            "{json}"
        );
    }

    /// The serialised form keeps the key and carries the reason, so a reader
    /// can tell an unknown fact from an absent one.
    #[test]
    fn an_unknown_fact_serialises_with_its_reason() {
        let known = serde_json::to_string(&Fact::Known(7u64)).expect("serialise");
        assert_eq!(known, "{\"known\":7}");
        let unknown: Fact<u64> = Fact::Unknown("cannot read /proc/stat");
        let unknown = serde_json::to_string(&unknown).expect("serialise");
        assert_eq!(unknown, "{\"unknown\":\"cannot read /proc/stat\"}");
    }
}
