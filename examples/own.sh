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

# True when a process is listening on the unix socket at $1.
#
# A socket file is not a listener. A process that died without unlinking its
# socket leaves the file behind, and a path a backend created and then unlinked
# is not a listener either. lsof names the bound socket, which is the fact the
# wait needs. own.sh already needs lsof for own_port_holder.
own_socket_listening() {
    local names
    [ -S "$1" ] || return 1
    names=$(lsof -nP -F n -- "$1" 2>/dev/null) || true
    grep -Fxq "n$1" <<< "$names"
}

# The pids this run recorded that are gone, on one line, empty when all are
# alive. A pid whose recorded start time no longer matches is a different
# process now, which is the same as gone.
own_exited_records() {
    local pid st cur out=""
    [ -f "$OWN_FILE" ] || return 0
    while IFS=$'\t' read -r pid st; do
        case "$pid" in ''|*[!0-9]*) continue ;; esac
        if kill -0 "$pid" 2>/dev/null; then
            cur=$(own_start_time "$pid")
            if [ -z "$st" ] || [ "$st" = "-" ] || [ -z "$cur" ] || [ "$cur" = "$st" ]; then
                continue
            fi
        fi
        out="${out:+$out, }$pid"
    done < "$OWN_FILE"
    printf '%s\n' "$out"
}

# Wait until every named backend socket has a process listening on it.
#
# $1 is the example name, so a failure says which stack stopped. The rest are
# socket paths. Returns 1 as soon as a socket cannot appear, naming the example
# and every path that is not listening.
#
# There is no time bound, on purpose. Each dev.sh used to poll a fixed twenty to
# thirty times, which is four to six seconds, and then start m6-http whether or
# not the sockets were there. A fixed number of seconds is a guess about the
# host, and a stack that starts slower than the guess is the case this replaces:
# while every recorded backend is alive, its socket may still appear, so the
# wait continues. The fact that ends a wait is the backend process itself. Once
# one this run recorded has exited, the path it should have bound will never
# appear, and the stack stops instead of starting m6-http on a half-built
# stack. m6-http validates every backend socket at start and exits before
# binding when one is missing, so the front would answer nothing at all.
#
# A socket counts only while a process is listening on it, and all of them are
# checked in the same pass, so a socket that appeared and then went away is not
# remembered as ready. It is reported as stopped listening rather than as never
# having appeared. A backend that is alive but will never bind is a hang, not a
# fact this can read: the wait continues, because no number of seconds can tell
# that from a slow host.
own_wait_sockets() {
    local who="${1:?own_wait_sockets needs the example name}"; shift
    [ "$#" -gt 0 ] || { echo "ERROR: $who: no backend socket paths were named" >&2; return 1; }
    [ -n "$OWN_FILE" ] || { echo "ERROR: $who: own_init was not called" >&2; return 1; }
    command -v lsof >/dev/null 2>&1 || {
        echo "ERROR: $who: lsof is needed to see whether a backend socket is listening" >&2
        return 1
    }
    local -a missing=() present=() seen=()
    local n=0 i s names dead
    for s in "$@"; do seen+=(""); done
    while :; do
        present=(); n=0
        for s in "$@"; do
            [ -S "$s" ] && present+=("$s")
        done
        names=""
        if [ "${#present[@]}" -gt 0 ]; then
            names=$(lsof -nP -F n -- "${present[@]}" 2>/dev/null) || true
        fi
        missing=()
        i=0
        for s in "$@"; do
            if grep -Fxq "n$s" <<< "$names"; then
                seen[i]=1
            else
                missing+=("$s"); n=$((n + 1))
            fi
            i=$((i + 1))
        done
        [ "$n" -eq 0 ] && return 0
        dead=$(own_exited_records)
        if [ -n "$dead" ]; then
            i=0
            for s in "$@"; do
                if ! own_socket_listening "$s"; then
                    if [ "${seen[i]:-}" = 1 ]; then
                        echo "ERROR: $who: $s stopped listening" >&2
                    else
                        echo "ERROR: $who: $s never appeared" >&2
                    fi
                fi
                i=$((i + 1))
            done
            echo "       a backend this run started has exited (pid $dead), so the stack cannot be complete" >&2
            return 1
        fi
        sleep 0.2
    done
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
