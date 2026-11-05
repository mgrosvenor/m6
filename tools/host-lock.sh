# shellcheck shell=bash
# host-lock.sh: how gates share one build host (#344). Sourced by check.sh,
# tools/perfcheck.sh and both build-host halves of tools/build-host-tests.sh, so
# the protocol is written once.
#
# Builds and lints run side by side: each run holds the host lock shared. The
# steps that bind fixed loopback ports take the port lock, one run at a time. A
# measurement needs the host to itself, because a wall-clock number taken beside
# a build measures the build: it takes the host lock exclusive. It queues through
# the entry lock, which every run passes on its way in, so runs that arrive while
# a measurement waits queue behind it instead of starving it.
#
# The lock files are opened read only, so an account other than the one that
# created them can still lock them. The host and entry locks are held on fds 8
# and 6; the commands this file runs for a caller close them, so a process that
# outlives its run cannot keep the host from a measurement. Where flock is
# missing (macOS) a run has the machine to itself and nothing is locked.

M6_HOST_LOCK="${M6_HOST_LOCK:-/var/tmp/m6-host.lock}"
M6_HOST_ENTRY_LOCK="${M6_HOST_ENTRY_LOCK:-/var/tmp/m6-host-entry.lock}"
M6_TEST_LOCK="${M6_TEST_LOCK:-/var/tmp/m6-test.lock}"
# How long a measurement waits for an idle host before it reports NOT MEASURED,
# rather than measuring under load or waiting for ever.
M6_HOST_IDLE_WAIT="${M6_HOST_IDLE_WAIT:-7200}"

_have_flock() { command -v flock >/dev/null 2>&1; }

# Create a lock file readable by every account, once.
_lock_file() {
    [[ -e "$1" ]] || (umask 000 && : >> "$1") 2>/dev/null || true
}

# Hold the host lock shared for the rest of the run.
host_enter_shared() {
    _have_flock || return 0
    _lock_file "$M6_HOST_ENTRY_LOCK"
    _lock_file "$M6_HOST_LOCK"
    exec 6<"$M6_HOST_ENTRY_LOCK"
    flock 6
    exec 8<"$M6_HOST_LOCK"
    flock -s 8
    exec 6<&-
}

# Run a command that binds fixed loopback ports, one run at a time. The command
# and anything it starts do not inherit the host lock.
with_port_lock() {
    if _have_flock; then
        _lock_file "$M6_TEST_LOCK"
        flock -o "$M6_TEST_LOCK" "$@" 8<&-
    else
        "$@"
    fi
}

# Hold the port lock on fd 7 for the rest of the run, for a run whose remaining
# steps all bind ports.
port_hold() {
    _have_flock || return 0
    _lock_file "$M6_TEST_LOCK"
    exec 7<"$M6_TEST_LOCK"
    flock 7
}

# Take the host for a measurement. Returns 0 holding it exclusive, or prints
# NOT MEASURED and returns 2. The shared hold is dropped first: two runs that
# each kept theirs while queuing for the exclusive one would wait on each other.
host_alone() {
    _have_flock || return 0
    [[ "${M6_HOST_LOCK_HELD:-}" == exclusive ]] && return 0
    _lock_file "$M6_HOST_ENTRY_LOCK"
    _lock_file "$M6_HOST_LOCK"
    if { true <&8; } 2>/dev/null; then
        flock -u 8
    else
        exec 8<"$M6_HOST_LOCK"
    fi
    exec 6<"$M6_HOST_ENTRY_LOCK"
    if flock -w "$M6_HOST_IDLE_WAIT" 6 && flock -w "$M6_HOST_IDLE_WAIT" -x 8; then
        export M6_HOST_LOCK_HELD=exclusive
        return 0
    fi
    echo "NOT MEASURED: the host was not idle within ${M6_HOST_IDLE_WAIT}s (holders: $(fuser "$M6_HOST_LOCK" 2>/dev/null | xargs))" >&2
    return 2
}
