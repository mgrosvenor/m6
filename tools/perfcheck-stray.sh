# shellcheck shell=bash
# perfcheck-stray.sh: the leftover-server check, sourced by perfcheck.sh and by
# its test. A stray m6-html pins the measured CPU and corrupts every reading, so
# a run that leaves one behind fails. The paths are absolute, so the check
# matches only the binaries this run started and not another checkout's servers
# that happen to share a relative `target/release/m6-html`.

# Print the absolute form of a path, resolving a relative path against the
# current directory. Symlinks and `..` are left in place, which is correct
# because the same string is passed to both `exec` and `pgrep`.
resolve_abs() {
  local p="$1"
  if [[ "$p" = /* ]]; then
    printf '%s\n' "$p"
  else
    printf '%s\n' "$(pwd)/$p"
  fi
}

# Return 0 when either the base or candidate binary is still running.
stray_alive() { # stray_alive <base-bin> <cand-bin>
  pgrep -f "$1" >/dev/null 2>&1 && return 0
  [[ "$2" != "$1" ]] && pgrep -f "$2" >/dev/null 2>&1 && return 0
  return 1
}
