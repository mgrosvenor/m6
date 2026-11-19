#!/bin/bash
# dev-home.sh — source this; do not run it.
#
# Sets and exports M6_DEV_HOME, the private per-user directory that holds the
# development keys, the auth databases, the tokens and the rendered configs.
# It defaults to ${XDG_RUNTIME_DIR:-${TMPDIR:-$HOME/.cache}}/m6. Nothing secret
# goes under a path another local user can create.
#
# The directory is created with mode 700. If it already exists it must be a
# real directory (not a symlink), owned by this user, with mode 700, or the
# calling script exits: another user's pre-created directory is never adopted.
#
# The configs in each example name this directory as @M6_DEV_HOME@, because m6
# reads absolute paths and expands no variables. m6_conf <file> writes a copy
# with the placeholder replaced and prints the path of the copy.

_m6_base="${XDG_RUNTIME_DIR:-${TMPDIR:-${HOME:?HOME is not set}/.cache}}"
export M6_DEV_HOME="${M6_DEV_HOME:-${_m6_base%/}/m6}"
unset _m6_base

# m6_private_dir <dir>: create <dir> with mode 700 and fail closed unless it is
# a real directory owned by this user with exactly that mode.
m6_private_dir() {
    local d="$1" mode
    mkdir -p -m 700 "$d" 2>/dev/null || true
    mode=$(stat -c %a "$d" 2>/dev/null || stat -f %Lp "$d" 2>/dev/null || echo unknown)
    if [ -L "$d" ] || [ ! -d "$d" ] || [ ! -O "$d" ] || [ "$mode" != 700 ]; then
        echo "ERROR: $d must be a directory owned by this user with mode 700 (found mode $mode)." >&2
        echo "       Refusing to keep keys there. Set M6_DEV_HOME to a directory you own." >&2
        return 1
    fi
}

case "$M6_DEV_HOME" in
    "" | /) echo "ERROR: M6_DEV_HOME is empty or /." >&2; exit 1 ;;
    /*[[:space:]]* ) echo "ERROR: M6_DEV_HOME must not contain whitespace: $M6_DEV_HOME" >&2; exit 1 ;;
    /*) ;;
    *) echo "ERROR: M6_DEV_HOME must be an absolute path: $M6_DEV_HOME" >&2; exit 1 ;;
esac
m6_private_dir "$M6_DEV_HOME" || exit 1

_M6_EXAMPLES="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# m6_conf <file>: print the path of <file> with @M6_DEV_HOME@ replaced. The
# copy is named after the example and directory it came from.
m6_conf() {
    local in dir out
    in="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
    dir="${in#"$_M6_EXAMPLES"/}"
    out="$M6_DEV_HOME/conf/${dir//\//-}"
    m6_private_dir "$M6_DEV_HOME/conf" || return 1
    ( umask 077; sed "s#@M6_DEV_HOME@#$M6_DEV_HOME#g" "$in" > "$out" ) || return 1
    echo "$out"
}
