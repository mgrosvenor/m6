# The platform layer

`m6-platform` is the one crate in m6 that knows which operating system (*OS*) it runs on.
m6-core re-exports it as `m6_core::platform`, so a service links m6-core alone.
This page states each contract the layer makes, how Linux and macOS meet it, how a port is added, and which tests hold it all.
It opens with the rule, then the contracts, the service managers, the ports and the tests, and closes with a summary.

## Contents

1. [The rule](#1-the-rule)
2. [The contracts](#2-the-contracts)
3. [Service managers](#3-service-managers)
4. [Future ports](#4-future-ports)
5. [Adding a port](#5-adding-a-port)
6. [The tests](#6-the-tests)
7. [Summary](#7-summary)

## 1. The rule

Platform-specific code lives in `m6-platform/src/` and nowhere else.
`src/lib.rs` holds the API and the shared code, and `src/linux.rs` and `src/macos.rs` hold the two implementations.
The implementation is selected once by OS family and ignores the CPU.
A family with no file fails to build with a message naming the file a port adds.

The gate test `m6-platform/tests/platform_gate.rs` reads every other `.rs` file in the workspace, tests included, and refuses:

- a `cfg` that names a platform (`target_os`, `target_family`, `target_vendor`, `target_arch`, `target_pointer_width`, `target_endian`, `cfg(unix`, `cfg(windows`)
- the crates `mio`, `nix`, `rustix` and `libc`, in code and in any other crate's manifest
- a direct `Instant::now()` or `SystemTime::now()` outside tests, benches and examples (a loop reads the clock through `platform::now`)
- a read of a service manager variable (section 3)
- an OS read: a `/proc`, `/sys`, os-release, `/etc/localtime` or `/etc/timezone` path, `uname`, `gethostname`, `getifaddrs`, `getrlimit`, `sysctlbyname`, an `RLIMIT_` resource and `systemd-detect-virt`

The one call site for the start-up facts is `m6_core::platform::host_facts()`.
Supported means built and tested in CI: macOS arm64, Linux x86_64 and Linux arm64.
Section 2 lists the contracts this rule protects.

## 2. The contracts

Table 1 states each item in the service's terms, with the OS mechanism beside it.
Read a row left to right: the contract is what every target guarantees, and the last two columns are how each OS meets it.

| item | the contract | Linux | macOS |
|---|---|---|---|
| `Poll` | an event for every new readiness edge on a registered interest, one event per token per wait, `reregister` always reaches the kernel so a ready source is reported again, a peer close is readable then EOF, edge-triggered so a turn reads and writes until `WouldBlock` | epoll, edge-triggered, through `mio` | kqueue with `EV_CLEAR`, through `mio` |
| `now`, `wall` | thin wrappers over the monotonic and wall clocks, read once per loop wake and passed down the turn | `CLOCK_MONOTONIC`, `CLOCK_REALTIME` | `CLOCK_UPTIME_RAW`, `gettimeofday` |
| `Signals` | SIGTERM and SIGINT are one `Shutdown` request, read once, a burst coalesces and none is lost, SIGUSR1 is read and discarded, the signals stay blocked for the life of the process | `signalfd`, whose read dequeues the signal | kqueue `EVFILT_SIGNAL`, which reports the signal and leaves it pending, so the three are also `SIG_IGN` and discarded once reported |
| `process::spawn` | a child starts with no signal blocked, SIGTERM, SIGINT, SIGUSR1 and SIGPIPE at their default disposition, and only the three standard streams `Streams` names, whichever thread opened another descriptor and whatever its close-on-exec flag, and it stays in the parent's process group so a terminal's interrupt reaches it | `std`'s spawn, where every descriptor is created close-on-exec in one call, and a `pre_exec` that clears the mask, restores the dispositions and marks every descriptor above stderr close-on-exec with `close_range` | `posix_spawnp` with `POSIX_SPAWN_CLOEXEC_DEFAULT`, because a socket here gets close-on-exec in a second call and a fork by another thread between the two would copy it, with the mask and dispositions set by the spawn's attributes |
| `net::accept_error` | an accept loop drains to `Drained` (`WouldBlock`), continues past `SkipOne` (a connection gone before it was accepted, an interrupted call), refuses the queued connection and keeps draining on `Exhausted` (out of descriptors), and on `Stop` (any other error) logs once, re-registers the listener and stops draining, so a queued connection is reported again | `ECONNABORTED` and `EINTR` are `SkipOne`, `EMFILE` and `ENFILE` are `Exhausted` | the same |
| `net::SpareDescriptor` | one descriptor of the listener's own socket family, held from start, so descriptor exhaustion can refuse the connection already queued: the reserve is closed, the connection is taken in the descriptor it frees and closed at once, and the reserve is opened again. The listener ends the turn with an empty queue, so it is never left edge-triggered with a connection nobody will read | a stream socket, `AF_UNIX` or `AF_INET`/`AF_INET6` from the listener's address | the same |
| `ChildExit` | a readable descriptor when the child exits, at once when it already has, and `reap` returns at once | `pidfd_open`, which needs Linux 5.3 | kqueue `EVFILT_PROC` with `NOTE_EXIT`, or one triggered user event when the kernel answers `ESRCH` for an exited child |
| `net::connect_unix` | a refused connect is a listener that is gone, a missing file is `NotFound`, and a full queue fails at once with `WouldBlock` and does not wait | `EAGAIN` for a full queue | `ECONNREFUSED` for a full queue too, so it reads as a listener that is gone |
| `net::connect_tcp` | `Connected` or `InProgress`, and `EINPROGRESS` is reported as `InProgress` | `WouldBlock` | the raw errno |
| `net::UdpBatch` | one `send` puts a buffer packed as segments of one size on the wire as those datagrams in order, the last at most the segment size, and one `recv` takes in up to `MAX_BATCH` receives, each with its length and source. A receive is one datagram, or a run the kernel coalesced, which `RecvMeta::datagrams` splits into the datagrams that were sent. A socket without segmentation offload sends one datagram per segment, and a socket without coalescing receives one datagram per slot, with the same bytes in the same order | `sendmsg` with a `UDP_SEGMENT` control message and `recvmmsg` with a `UDP_GRO` control buffer, both asked once at setup, the send option by `getsockopt(UDP_SEGMENT)` and the receive option by `getsockopt(UDP_GRO)` | a loop of one `sendto` and one `recvfrom` per datagram |
| `net::listen_shared` | a TCP listener and a UDP socket bound to one address with `SO_REUSEPORT`, so several processes serve one port, every connection is answered, and stopping one process leaves the port served | `SO_REUSEPORT` set before bind, and a hash of the 4-tuple chooses the socket, or with a group size a socket filter (`SO_ATTACH_REUSEPORT_CBPF`) chooses by the source address alone | `SO_REUSEPORT` set before bind, and the socket bound last receives every connection and datagram |
| sockets | every socket the layer makes gives `BrokenPipe` on a write to a closed peer and raises no signal | the runtime ignores `SIGPIPE` | the same, and `SO_NOSIGPIPE` on every socket |
| `MAX_SOCKET_PATH` | a path too long to bind fails at start naming the path and the limit | 108 | 104 |
| `process::raise_fd_limit` | the soft limit becomes the highest the kernel accepts, the call returns that limit less `FD_MARGIN` (16 descriptors for the listener, the signal descriptor, the poll and stdio), and a second call returns the same | the hard limit | the lower of the hard limit and `kern.maxfilesperproc`, because the hard limit reads as unlimited |
| `process::lock_memory` | every page the process has faulted, and every page it faults later, is pinned in RAM, or an error names the limit that refused and the fix, and a service that asked for pinning stops when pinning fails | `mlockall(MCL_CURRENT \| MCL_FUTURE \| MCL_ONFAULT)`, which needs `RLIMIT_MEMLOCK` unlimited (under systemd `LimitMEMLOCK=infinity`) and Linux 4.4 or newer | returns `Ok` and pins nothing |
| `process::huge_page_bytes` | the bytes of the process on huge pages now, zero when none, read once to log which kind of page the heap got | `AnonHugePages` and `Hugetlb` of `/proc/self/smaps_rollup` | 0 |
| `process::virtual_bytes` and `process::resident_bytes` | the process's address space and its resident set, in bytes, or `None` where the platform keeps no such report | `VmSize` and `VmRSS` of `/proc/self/status` | `None` |
| `process::page_size` | the size of one memory page, a power of two of at least 4 KiB, the unit `m6-core`'s bounded buffers are sized in | 4 KiB on x86_64 | 16 KiB on arm64 |
| `host` | memory, load, uptime, disk space and interface counters are real values, and an unreadable source gives `None` or an empty list | `/proc` and `/sys` | `sysinfo` with its default features off, so no thread starts |
| `facts::host_facts` | one reading at start of the host name, the FQDN, every interface address with its name and scope, the OS and kernel, the CPU model and count, total memory, page size and huge-page setting, the rlimits, `somaxconn` and cgroup limits, the time zone, clock source and boot time, and the virtualisation type. It never fails and never panics. A fact the host will not give is `Unknown` with a reason | `/etc/os-release`, `/proc/meminfo`, `/proc/cpuinfo`, `/proc/stat`, `/proc/self/cgroup`, `/proc/sys/net/core/somaxconn`, `/sys/kernel/mm/transparent_hugepage/enabled`, `/sys/fs/cgroup`, `/sys/class/dmi/id`, `/sys/devices/system/clocksource`, `/etc/hosts`, `uname`, `gethostname`, `getifaddrs`, `getrlimit` | `SystemVersion.plist`, `kern.boottime`, `kern.ipc.somaxconn`, `machdep.cpu.brand_string`, `hw.memsize`, `hw.model`, `kern.hv_vmm_present`, `/etc/hosts`, `uname`, `gethostname`, `getifaddrs`, `getrlimit` |
| `process` facts | thread count, open descriptors, alive, listening on TCP, root, another process's resident, virtual and locked memory and its minor page faults, and the worker range that reading the host's certificate store leaves. A count that cannot be read is an error | `/proc/<pid>`, and the certificate store is a file read that leaves no thread | `proc_pidinfo` and `lsof`, no memory or fault report for another process (an error), and the Security framework leaves three or four worker threads |
| directories | `runtime_dir` is `/run/m6`, `temp_dir` honours `TMPDIR`, `socket_scratch_root` is the short root a socket scratch directory goes under whatever `TMPDIR` is, `scratch_dir` permits exec, `create_private` gives 0700 on a parent it creates and 0600 on the file | the same | the same |

**Table 1: each item of the layer, its contract, and the mechanism behind it on each OS.**

`m6-http` meets the `net::connect_unix` contract as follows.
A full queue or a refused connect moves the request to the next member of the backend pool.
When every attempt was a full queue, the answer is 503, which happens on Linux.
On macOS a full queue is `ECONNREFUSED`, so the answer is 502 with the connect error.

`facts::host_facts` is the one reading of a host that a service does not poll, and it is called once at start.
Its groups are `Fact` values all the way down, so a host that will say how many CPUs it has but not which model keeps the count.
An unknown fact serialises as `{"unknown": "reason"}`, so every key is present in the record.
The cgroup limits are read at the group `/proc/self/cgroup` names, and a group with no `memory.max` or `cpu.max` of its own is read at the nearest parent up to the mount point.
`max` is a known value meaning no limit, and it serialises as `{"known": null}`, which a monitor reads as unlimited.
The boot time is RFC 3339 in UTC from `write_rfc3339_utc`, the one date text m6 writes.
The address scope is one of `loopback`, `link_local`, `private`, `public` and `wire_guard`.
It comes from a heuristic: an interface named `wg` and a number, or `utun`, is `wire_guard` whatever its range, a loopback address is `loopback`, 169.254.0.0/16 and fe80::/10 are `link_local`, an RFC 1918 or unique local address is `private`, and the rest is `public`.
No read goes to the network.
The FQDN is the host name when it carries its domain, and otherwise the first name with a domain on the `/etc/hosts` line that lists the host, or unknown with the reason.

Five behaviours differ between the targets.

- **Writable reports.** A writable report may repeat on macOS after the peer acknowledges data, because kqueue re-arms the filter and epoll does not. A loop that writes until `WouldBlock` pays one failed write for it.
- **Clocks.** Both monotonic clocks stop while the machine sleeps, so a deadline set before a sleep fires after the wake, late by the sleep's length.
- **Readings.** Three readings have no macOS source without IOKit and are absent there: disk throughput, pressure stall averages and thermal zones. Interface drop counters read 0 on macOS.
- **Socket buffers.** The default socket buffers are about 128 KiB on macOS and about 208 KiB on Linux, so a test that depends on a size asks for it through `net::set_buffer_sizes`.
- **Listen queue.** A full TCP listen queue shows differently on the two kernels. macOS refuses the next connection at once with `ECONNREFUSED`. Linux leaves it pending and the client waits for its own timeout. m6 passes the kernel's answer through.
- **A shrunk backlog.** After an `accept` takes one connection of several queued, kqueue may report the listener again for the ones left, and epoll reports nothing until a new connection arrives. A loop never relies on the first: it drains to `WouldBlock` in the same turn, which is the only behaviour that is correct on both.

Descriptor exhaustion is refused, never waited out.
`raise_fd_limit` sets the connection ceiling, and one descriptor of the listener's family sits in reserve.
When `accept` fails with `EMFILE` or `ENFILE`, the loop closes the reserve, takes the connection already queued in the descriptor that frees, closes that connection at once, opens the reserve again, writes one rate-limited line at `warn` with the exact count of refusals since the previous line, and keeps draining.
The count matters as much as the line: an operator reads how many connections were refused, not only that some were.
A listener that stops accepting under that pressure cannot recover on its own. An edge-triggered registration has already reported the readiness that woke it, so the only thing that reports it again is another connection, and a full queue lets no other connection complete.
The audit of `1179b991` found exactly that: a backend that stopped accepting for good, silently, with no line at all.
The reserve is `libuv`'s `uv__emfile_trick`, and it is what `nginx`-style servers do for the same reason.

`net::listen_shared` is the only place m6 sets `SO_REUSEPORT`.
Its use in m6-http is the `[server] reuse_port` key, and `[server] reuse_port_processes` gives the group size.
Linux spreads a new TCP connection across the sockets that share a port by a hash of its 4-tuple and gives every UDP datagram to one socket.
With a group size, `listen_shared` attaches a classic BPF program to both sockets that returns (a hash of the client's source address) mod the group size, so one address always reaches one socket.
The program reads the address with the `SKF_NET_OFF` offsets and allocates nothing.
macOS attaches no program: the socket bound last to the port receives every connection and every datagram, so a second process there takes the port over and adds no capacity.
A port of 0 gives the TCP listener and the UDP socket one kernel-chosen port, so a test can bind both halves without naming one.
Nothing may assume an even spread on either target, and on Linux a QUIC client that changes address may land on another process.
m6-http opens a shared listener only for the public TLS port and its QUIC UDP socket on `[server].bind`.
The redirect listener, the internal h2c listener and the backend sockets do not use it.
The health endpoints are served on that shared listener, and with `[server] status_bind` each process also serves them on a TLS listener of its own, which does not use `listen_shared`.
Section 6 lists the tests that hold every item of Table 1.

`net::UdpBatch` is the one place a datagram batch reaches the kernel.
`UdpBatch::for_socket` asks once, at socket setup, whether the kernel segments a batch on send and coalesces one on receive.
A socket that answers no to segmentation takes the same send down the loop of single sends, so the bytes and their order do not depend on the answer.
A socket that answers no to coalescing receives one datagram per call, which is the path it took before, and the batch API reports each of them on its own.
`UdpBatch::without_gso` is the send path on a socket that answered yes, and the shared contract suite runs both paths over loopback and compares the datagrams they deliver.
That comparison runs on a host with segmentation offload, which is how the fallback is tested as a fallback and the Linux segmented path is compared with it on one machine.
On macOS the two send paths are the same loop, and the suite still holds the bytes and the order.

A coalesced receive is one `RecvMeta` whose `len` is the whole run and whose `stride` is the kernel's `gso_size`, zero when the receive is one datagram.
`RecvMeta::datagrams` splits it into `(offset, length)` pairs, the last at most `stride` bytes, in the order the sender sent them.
A receive slot is `RECV_SLOT` bytes, 64 KB, so a coalesced run up to the largest UDP payload arrives whole.
The contract suite sends one segmented batch to a receiver that enabled coalescing, and requires the four datagrams back with their lengths, bytes and order.
On Linux that test requires the socket to accept both options and the kernel to deliver the run coalesced.
On macOS it takes the per-datagram path and still requires the same four datagrams, which is how the split is held on both targets and how the Linux path is held to the fallback's answer.

## 3. Service managers

An m6 service needs six things from whatever starts it, and both systemd and launchd give all six.
Table 2 lists them.

| need | systemd | launchd |
|---|---|---|
| start, stop, restart | `ExecStart`, `KillSignal=SIGTERM`, `Restart=` | `ProgramArguments`, `KeepAlive`, SIGTERM on unload |
| shutdown requests | `systemctl stop` | `launchctl unload` |
| secrets by absolute path | a path in the config, mode 0600 | the same |
| runtime and state directories | paths in the config, created by the unit or the operator | paths in the config, created by the plist or the operator |
| several instances | `M6_SOCKET_OVERRIDE` per unit | `M6_SOCKET_OVERRIDE` per plist |
| logs | stdout to the journal, which rotates | stdout to `StandardOutPath`, which `newsyslog` rotates |

**Table 2: what a service needs from its manager and how each one provides it.**

m6 never talks to a service manager and relies on none to create anything.
No binary reads `CREDENTIALS_DIRECTORY`, `NOTIFY_SOCKET`, `LISTEN_FDS`, `WATCHDOG_USEC`, `INVOCATION_ID`, `STATE_DIRECTORY` or `RUNTIME_DIRECTORY`, and the gate refuses a read of any of them.
`JOURNAL_STREAM` is read in the platform layer only.
`journal_stream()` compares the device and inode systemd wrote to the variable with the ones `fstat` reports for descriptor 1, and answers true only when they match, which is systemd's documented contract for the variable.
m6-log writes journald's native socket when it did.
A path a service needs comes from its config, so the same binary runs under either manager.

## 4. Future ports

Each port is one file beside `linux.rs` and `macos.rs`.
Three cases follow.

- **FreeBSD and OpenBSD.** The macOS file nearly as it is: kqueue for `Poll`, `Signals` and `ChildExit`, `SO_NOSIGPIPE`, and a `sun_path` of 104. `sysinfo` supports FreeBSD and lists no OpenBSD support, so OpenBSD would need a small `sysctl` reader for memory and load. `listens_on_tcp` would need `sysctl` in place of `lsof`.
- **Linux on other CPUs and macOS on x86_64.** Nothing changes, because selection is by family and no file reads the CPU. CI builds and tests Linux arm64.
- **Windows.** Unix socket paths, file modes, `raise_fd_limit` and the load average have no equivalent. `Signals`, `ChildExit` and the edge-triggered `Poll` would be rebuilt on Windows primitives, and `host` would come from `sysinfo`, which has a Windows backend.

## 5. Adding a port

Copy `m6-platform/src/macos.rs` to `m6-platform/src/<family>.rs`, implement every `pub(super)` item it lists, and select the file at the top of `m6-platform/src/lib.rs`.
The gate's `the_two_arms_define_the_same_items` test compares the item lists of the two existing files.
Then add the target to CI beside the existing jobs, and run the suites in section 6 on it.
A port that passes them is supported.

## 6. The tests

Every test of the layer is written once against its public API and runs unchanged on every supported target, with no `cfg` and no per-OS branch.
Table 3 maps them.

| suite | file | holds |
|---|---|---|
| the contract | `m6-platform/tests/platform_contract.rs` | readiness, child exit, what a spawned child inherits, sockets, clock, limits, host and process facts (including the start-up facts), directories |
| signals | `m6-platform/tests/platform_signals.rs` | the 8 signal cases, in a test with its own `main` (`harness = false`) so the mask is set before any thread |
| process facts | `m6-platform/tests/platform_process_facts.rs` | thread and descriptor counts, run in a child process so nothing else moves them |
| child exit | `m6-platform/tests/platform_child_exit_leak.rs` | that a dropped `ChildExit` leaks no descriptor |
| the gate | `m6-platform/tests/platform_gate.rs` | the rule in section 1 |
| the reference app | `m6-core/examples/echo.rs`, `echo_tls.rs`, `m6-core/tests/echo_example.rs` | the loop every service follows, built on the layer alone, with checks that include zero allocations, a shutdown that drains, queued clients refused with no new connection arriving, and a listener serving again as soon as a descriptor frees |

**Table 3: where each contract is held.**

Each test of the contract names, in a comment, the wrong implementation that must fail it.

## 7. Summary

`m6-platform` holds every operating-system read and call in the workspace, and a gate test refuses one anywhere else.
Each contract in Table 1 is stated in a service's terms, met on Linux and macOS, and held by the suites in Table 3, so a port adds one file and passes the same tests.
[`m6-core-reference.md`](m6-core-reference.md) §8.5 and §8.5.1 give the host readings and facts as a service calls them, and [`m6-log.md`](m6-log.md) §9 shows where the facts are written.

