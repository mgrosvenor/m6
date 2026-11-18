# The platform layer

`m6-platform` is the one crate in m6 that knows which operating system it runs on.
m6-core re-exports it as `m6_core::platform`, so a service links m6-core alone.
This page states each contract the layer makes, how Linux and macOS meet it, what is normalised, how a port is added, and which tests hold it all.
It opens with the rule, then the contracts, then the ports and the tests.

## Contents

1. The rule
2. The contracts
3. Service managers
4. Future ports
5. Adding a port
6. The tests

## 1. The rule

Platform-specific code lives in `m6-platform/src/` and nowhere else.
`src/lib.rs` holds the API and the shared code, `src/linux.rs` and `src/macos.rs` hold the two implementations, selected once by OS family and never by CPU.
A family with no file fails to build with a message naming the file a port adds.
The gate test `m6-platform/tests/platform_gate.rs` refuses, in every other `.rs` file and manifest of the workspace, tests included: a `cfg` that names a platform, the crates `mio`, `nix`, `rustix` and `libc`, a direct `Instant::now()` or `SystemTime::now()` outside tests, a read of a systemd-only variable, and a read of a host fact.
That last rule covers a `/proc`, `/sys` or os-release path, `uname`, the host name, the interface table, an rlimit and a `sysctl`.
The one call site for the start-up facts is `m6_core::platform::host_facts()`, and the gate refuses an OS read anywhere else.
Supported means built and tested in CI: macOS arm64, Linux x86_64 and Linux arm64.
The rule is the whole design, and the contracts below are what it protects.

## 2. The contracts

Each item is stated in the service's terms, with the OS mechanism beside it.
Table 1 lists them.

| item | the contract | Linux | macOS |
|---|---|---|---|
| `Poll` | an event for every new readiness edge on a registered interest, one event per token per wait, `reregister` always reaches the kernel so a ready source is reported again, a peer close is readable then EOF, edge-triggered so a turn reads and writes until `WouldBlock` | epoll, edge-triggered, through `mio` | kqueue with `EV_CLEAR`, through `mio` |
| `now`, `wall` | thin wrappers over the monotonic and wall clocks, read once per loop wake and passed down the turn | `CLOCK_MONOTONIC`, `CLOCK_REALTIME` | `CLOCK_UPTIME_RAW`, `gettimeofday` |
| `Signals` | SIGTERM and SIGINT are one `Shutdown` request, read once, a burst coalesces and none is lost, the signals stay blocked for the life of the process | `signalfd`, whose read dequeues the signal | kqueue `EVFILT_SIGNAL`, which reports and does not dequeue, so the two are also `SIG_IGN` and discarded once reported |
| `process::spawn` | a child starts with no signal blocked or ignored and inherits no descriptor above stderr, and stays in the parent's process group so a terminal's interrupt reaches it | `pre_exec` clears the mask and restores the two dispositions, every descriptor above stderr is marked close-on-exec first | the same |
| `net::accept_error` | an accept loop drains to `Drained`, passes over `SkipOne` (a connection gone before it was accepted, an interrupted call) and on `Stop` (any other error) logs it once and stops accepting for the turn | `EAGAIN`, `ECONNABORTED` and `EINTR` | the same |
| `ChildExit` | a readable descriptor when the child exits, at once when it already has, `reap` never blocks | `pidfd_open` | kqueue `EVFILT_PROC` with `NOTE_EXIT`, or one triggered user event when the kernel answers `ESRCH` for an exited child |
| `net::connect_unix` | a refused connect is a listener that is gone, a missing file is `NotFound`, and a full queue never connects and never hangs, so the caller tries the next member and answers 503 only when every attempt was a full queue | `EAGAIN` for a full queue: busy, the next member for this request | `ECONNREFUSED` for a full queue too, so a full queue reads the same as a listener that is gone and the caller tries the next member |
| `net::connect_tcp` | `Connected` or `InProgress`, never `EINPROGRESS` as an error | `WouldBlock` | the raw errno |
| `net::listen_shared` | a TCP listener and a UDP socket bound to one address with `SO_REUSEPORT`, so several processes serve one port, every connection is answered, and stopping one leaves the port served | `SO_REUSEPORT` set before bind, and a hash of the 4-tuple chooses the socket, or with a group size a socket filter (`SO_ATTACH_REUSEPORT_CBPF`) chooses by the source address alone | `SO_REUSEPORT` set before bind, and a UDP datagram goes to one socket |
| sockets | every socket the layer makes gives `BrokenPipe` on a write to a closed peer, never a signal | the runtime ignores `SIGPIPE` | the same, and `SO_NOSIGPIPE` on every socket |
| `MAX_SOCKET_PATH` | a path too long to bind fails at start naming the path and the limit | 108 | 104 |
| `process::raise_fd_limit` | the soft limit becomes the highest the kernel accepts, the number returned is what connections may use, a second call returns the same | the hard limit | `kern.maxfilesperproc`, because the hard limit reads as unlimited |
| `process::lock_memory` | every page the process has faulted, and every page it faults later, is pinned in RAM, or an error names the limit that refused and the fix, and a service that asked for pinning stops rather than runs unpinned | `mlockall(MCL_CURRENT \| MCL_FUTURE \| MCL_ONFAULT)`, with `RLIMIT_MEMLOCK` unlimited and a kernel of 4.4 or newer; under systemd `LimitMEMLOCK=infinity` | nothing to ask for, returns `Ok` |
| `process::huge_page_bytes` | the bytes of the process on huge pages now, zero when none, read once to log which kind of page the heap got | `AnonHugePages` and `Hugetlb` of `/proc/self/smaps_rollup` | 0 |
| `process::virtual_bytes` and `process::resident_bytes` | the process's address space and its resident set, in bytes, or `None` where the platform keeps no such report | `VmSize` and `VmRSS` of `/proc/self/status` | `None` |
| `process::page_size` | the size of one memory page, a power of two of at least 4 KiB, the unit `m6-core`'s bounded buffers are sized in | 4 KiB on x86_64 | 16 KiB on arm64 |
| `host` | memory, load, uptime, disk space and interface counters are real values, an unreadable source is `None` or an empty list, never a row of zeros | `/proc` and `/sys` | `sysinfo` with its default features off, so no thread starts |
| `facts::host_facts` | one reading at start of the host name, the FQDN, every interface address with its name and scope, the OS and kernel, the CPU model and count, total memory, page size and huge-page setting, the rlimits, `somaxconn` and cgroup limits, the time zone, clock source and boot time, and the virtualisation type. Never fails and never panics. A fact the host will not give is `Unknown` with a reason, so a reader can tell it from an empty value | `/etc/os-release`, `/proc/meminfo`, `/proc/cpuinfo`, `/proc/stat`, `/proc/sys/net/core/somaxconn`, `/sys/kernel/mm/transparent_hugepage/enabled`, `/sys/fs/cgroup`, `/sys/class/dmi/id`, `/sys/devices/system/clocksource`, `uname`, `gethostname`, `getifaddrs`, `getrlimit` | `SystemVersion.plist`, `kern.boottime`, `kern.ipc.somaxconn`, `machdep.cpu.brand_string`, `hw.memsize`, `hw.model`, `kern.hv_vmm_present`, `uname`, `gethostname`, `getifaddrs`, `getrlimit` |
| `process` facts | thread count, open descriptors, alive, listening on TCP, root, another process's resident, virtual and locked memory and its minor page faults, and the worker range reading the host's certificate store leaves. A count that cannot be read is an error, never a zero | `/proc/<pid>`; the store is a file read and leaves no thread | `proc_pidinfo`, `lsof`; no memory or fault report for another process, so those are an error; the Security framework leaves three or four |
| directories | `runtime_dir` is `/run/m6`, `temp_dir` honours `TMPDIR`, `socket_scratch_root` is the short root a socket scratch directory goes under whatever `TMPDIR` is, `scratch_dir` permits exec, `create_private` gives 0700 and 0600 | the same | the same |

Table 1: each item of the layer, its contract, and the mechanism behind it on each OS.

`facts::host_facts` is the one reading of a host a service is not polling, and it is called once at start.
Its groups are `Fact` values all the way down, so a host that will say how many CPUs it has but not which model does not lose the count.
A fact that could not be read serialises as `{"unknown": "reason"}` rather than being left out, so a reader can tell an unknown fact from an absent key.
The boot time is RFC 3339 in UTC from `write_rfc3339_utc`, the one date text m6 writes.
The address scope is a documented heuristic: a `wg` name or a `utun` name is WireGuard, a private range is private, and the rest is public.
The one read that can block is the resolver lookup for the FQDN, and it is skipped when the host name already carries its domain.

Three things are stated and not normalised.
A writable report may repeat on macOS after the peer acknowledges data, because kqueue re-arms the filter and epoll does not. A loop that writes until `WouldBlock` pays one failed write for it.
On macOS a stale socket file and a full queue are the same `ECONNREFUSED`, and the kernel's answer is passed through, so a full queue there costs the request one connect and it moves to the next member.
Both monotonic clocks stop while the machine sleeps, so a deadline set before a laptop sleeps fires after it wakes, late by the sleep's length.
Known limitation: when a process runs out of file descriptors, connections already queued wait until the next new connection arrives. m6 does not handle descriptor exhaustion.
Known limitation: a full listen queue is reported differently by the two kernels. macOS refuses the next connection at once, so the client sees `ECONNREFUSED` and can retry or give up. Linux leaves the connection pending, so the client waits for a timeout it did not choose. The macOS answer is the better one, and m6 does not equalise the two, because equalising would mean accepting connections it cannot serve. At this scale the queue does not fill: the accept loop drains it every turn, and a slow process still drains it, so a full queue means the process is not running.
Three readings have no macOS source without IOKit and are absent there: disk throughput, pressure stall averages and thermal zones. Interface drop counters read 0 on macOS.
m6 sets `SO_REUSEPORT` only on a listener a service opens through `net::listen_shared`, which m6-http's `[server] reuse_port` asks for.
Linux spreads a new TCP connection across the sockets that share a port by a hash of its 4-tuple and gives every UDP datagram to one socket. With a group size, `listen_shared` attaches a classic BPF program to both sockets that returns (a hash of the client's source address) mod the group size, so one address always reaches one socket; it reads the address with the `SKF_NET_OFF` offsets and allocates nothing. macOS spreads neither and attaches nothing: the socket bound last to the port receives every connection and every datagram, so there a second process is a takeover rather than a scale-out.
A port of 0 gives the TCP listener and the UDP socket one kernel-chosen port, so a test can bind both halves without naming one.
Nothing may assume an even spread on either target, and on Linux a QUIC client that changes address may land on another process.
m6-http opens it only for the public 443 port: the TLS listener and its QUIC UDP socket on `[server].bind`. The redirect listener, the internal h2c listener and the backend sockets never go through it. The health endpoints are served on that shared listener, and with `[server] status_bind` each process also serves them on a plain TLS listener of its own, which never goes through `listen_shared`.
The default socket buffers differ (about 128 KiB on macOS, about 208 KiB on Linux), so a test that depends on a size asks for it through `net::set_buffer_sizes`.
Everything in Table 1 is held by the tests in section 6.

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

Table 2: what a service needs from its manager and how each one provides it.

m6 never talks to a service manager and never relies on one to create anything.
No binary reads `CREDENTIALS_DIRECTORY`, `NOTIFY_SOCKET`, `LISTEN_FDS`, `WATCHDOG_USEC`, `INVOCATION_ID`, `STATE_DIRECTORY` or `RUNTIME_DIRECTORY`, and the gate refuses a read of any of them.
`JOURNAL_STREAM` is the one exception, read here and nowhere else: `journal_stream()` answers whether systemd wired stdout to the journal, and m6-log writes journald's native socket when it did.
A path a service needs comes from its config, and the rule above is what makes the same binary run under either manager.

## 4. Future ports

Each port is one file beside `linux.rs` and `macos.rs`.
What would map and what would not, in a few lines each:

- **FreeBSD and OpenBSD.** The macOS file, nearly as it is: kqueue for `Poll`, `Signals` and `ChildExit`, `SO_NOSIGPIPE`, `sun_path` of 104. `sysinfo` supports FreeBSD and not OpenBSD, which would take a small `sysctl` reader for memory and load. `listens_on_tcp` needs `sysctl` in place of `lsof`.
- **Linux on arm64 and macOS on x86_64.** Nothing changes: selection is by family and no file reads the CPU. CI builds and tests Linux arm64.
- **Windows.** `mio`'s backend polls sockets only, and the layer would keep its own ready set to give edge semantics. `Signals` would be a console control handler written into a socket pair. `ChildExit` would be a process handle waited on by a helper and written into a socket pair. Unix socket paths, file modes, `raise_fd_limit` and the load average have no equivalent, and `host` would come from `sysinfo`, which supports Windows.

## 5. Adding a port

Copy `m6-platform/src/macos.rs` to `m6-platform/src/<family>.rs`, implement every `pub(super)` item it lists, and select the file at the top of `m6-platform/src/lib.rs`.
The gate's `the_two_arms_define_the_same_items` test compares the item lists, so a missing item fails on every target.
Then add the target to CI beside the three jobs, and run the suites in section 6 on it.
A port that passes them is supported.

## 6. The tests

Every test of the layer is written once against its public API and runs unchanged on every supported target, with no `cfg` and no per-OS branch.
Table 3 maps them.

| suite | file | holds |
|---|---|---|
| the contract | `m6-platform/tests/platform_contract.rs` | readiness (17 cases), child exit (8), what a spawned child inherits and an interrupted harness (2), sockets (13), clock (2), limits (2), host and process facts (14, including the start-up facts), directories (8) |
| signals | `m6-platform/tests/platform_signals.rs` | the 8 signal cases, in its own `main` so the mask is set before any thread |
| the gate | `m6-platform/tests/platform_gate.rs` | the rule in section 1 |
| the reference app | `m6-core/examples/echo.rs`, `echo_tls.rs`, `m6-core/tests/echo_example.rs` | the loop every service follows, built on the layer alone, with 17 behavioural checks including zero allocations, a shutdown that drains and queued clients served once descriptors free |

Table 3: where each contract is held.

Each test names, in a comment, the wrong implementation that must fail it, and every existing service suite passes unchanged on every target above them.
The layer is small because the tests, and the gate, are what keep it that way.
