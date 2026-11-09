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

# Escape a path for use as a pgrep -f extended regex.
escape_regex() {
  printf '%s\n' "$1" | sed 's:[][\.^$*+?(){}|]:\\&:g'
}

# Return 0 when either the base or candidate binary is still running. The
# pattern is anchored to the start of the command line, so a process whose argv
# merely carries the path as an argument (perfcheck.sh's own `--base-bin`, for
# example) does not match; only a process that was started AS that binary does.
stray_alive() { # stray_alive <base-bin> <cand-bin>
  pgrep -f "^$(escape_regex "$1")" >/dev/null 2>&1 && return 0
  [[ "$2" != "$1" ]] && pgrep -f "^$(escape_regex "$2")" >/dev/null 2>&1 && return 0
  return 1
}
