#!/bin/bash
# own.sh — stop only processes this script started.
#
# Sourced by the examples' dev.sh / test.sh next to dev-keys.sh:
#
#   . "$SITE/../own.sh"
#   own_init "$SITE" stack          # pid file: <site>/logs/.own-stack.pids
#   own_reap                        # stop what a previous run recorded
#   own_refuse_busy 8443 "example 05"   # refuse, naming port+pid, if a stranger holds it
#   ... start services ...
#   own_track "$HTML_PID"           # record each pid the script just started
#   trap 'own_stop_records; own_forget; wait' EXIT
#
# The only evidence of ownership is the pid file this script wrote. Nothing
# here greps a process name or scans a port and kills the result: on a machine
# where several worktrees run the same example, a name or port match is another
# session's stack as easily as this one's. A holder that this script cannot
# prove it started is reported and left alone.
#
# Each record is "<pid>\t<start time>". Where the platform exposes a process
# start time (ps), a pid reused by an unrelated process after a crash no longer
# matches and is left alone. Where it does not (ps is hidden in some sandboxes),
# the recorded pid itself is the proof, as the requirement allows.

OWN_FILE=""
OWN_SITE=""

own_init() {
    OWN_SITE="${1:?own_init needs the site directory}"
    local tag="${2:-stack}"
    mkdir -p "$OWN_SITE/logs"
    OWN_FILE="$OWN_SITE/logs/.own-${tag}.pids"
}

own_start_time() {
    # `|| true`: under `set -o pipefail`, a ps the sandbox denies must read as
    # "no start time", not abort the caller.
    ps -o lstart= -p "$1" 2>/dev/null | sed 's/^[[:space:]]*//;s/[[:space:]]*$//' || true
}

# Record a pid this script just started ($! from a background job).
own_track() {
    local pid="$1" st
    [ -n "$pid" ] || return 0
    [ -n "$OWN_FILE" ] || { echo "own_track: own_init was not called" >&2; return 1; }
    st=$(own_start_time "$pid")
    printf '%s\t%s\n' "$pid" "${st:--}" >> "$OWN_FILE"
}

own_is_recorded() {
    local pid="$1" p st
    [ -f "$OWN_FILE" ] || return 1
    while IFS=$'\t' read -r p st; do
        [ "$p" = "$pid" ] && return 0
    done < "$OWN_FILE"
    return 1
}

# Stop exactly the pids in the file. SIGTERM, a short grace, then SIGKILL —
# no other process is ever considered.
own_stop_records() {
    local pid st cur live=()
    [ -f "$OWN_FILE" ] || return 0
    while IFS=$'\t' read -r pid st; do
        case "$pid" in ''|*[!0-9]*) continue ;; esac
        kill -0 "$pid" 2>/dev/null || continue
        cur=$(own_start_time "$pid")
        if [ "$st" != "-" ] && [ -n "$cur" ] && [ "$cur" != "$st" ]; then
            echo "own: pid $pid in $OWN_FILE is a different process now (pid reused); leaving it alone" >&2
            continue
        fi
        live+=("$pid")
    done < "$OWN_FILE"
    [ "${#live[@]}" -eq 0 ] && return 0
    kill "${live[@]}" 2>/dev/null || true
    local i=0 p alive
    while [ "$i" -lt 30 ]; do
        alive=0
        for p in "${live[@]}"; do kill -0 "$p" 2>/dev/null && alive=1; done
        [ "$alive" -eq 0 ] && return 0
        sleep 0.1; i=$((i+1))
    done
    for p in "${live[@]}"; do kill -9 "$p" 2>/dev/null || true; done
    return 0
}

own_reap() {
    own_stop_records
    own_forget
}

own_forget() { rm -f "$OWN_FILE"; }

# The pid listening on a port, for a message. Read-only: this is never piped
# into kill.
own_port_holder() {
    lsof -nP -tiTCP:"${1:?own_port_holder needs a port}" -sTCP:LISTEN 2>/dev/null | head -1 || true
}

own_wait_port_free() {
    local port="$1" tries="${2:-30}" i=0
    while [ "$i" -lt "$tries" ]; do
        [ -z "$(own_port_holder "$port")" ] && return 0
        sleep 0.1; i=$((i+1))
    done
    return 1
}

# Return 1 when a process this script did not record holds the port, after
# printing its port and pid. The caller exits; nothing is killed.
own_refuse_busy() {
    local port="${1:?own_refuse_busy needs a port}" who="${2:-this script}" holder cmd
    holder=$(own_port_holder "$port")
    [ -n "$holder" ] || return 0
    own_is_recorded "$holder" && return 0
    cmd=$(lsof -nP -p "$holder" -a -d txt 2>/dev/null | awk 'NR==2 {print $NF}' || true)
    echo "ERROR: port $port is held by pid $holder${cmd:+ ($cmd)}," >&2
    echo "       which $who did not start, so it will not be killed." >&2
    echo "       Stop that process yourself, or run this on another port." >&2
    return 1
}
