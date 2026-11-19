# shellcheck shell=bash
# memcalls-site.sh: what tools/memcalls.sh and tools/hwcounters.sh share. Source
# it; it runs nothing but its own setup.
#
# It makes one small site, starts m6-html, m6-file and m6-http on it from a
# directory of built binaries, and stops them. m6-http answers `/` from its
# cache after the first request (the hot hit the scripts measure), `/render`
# from m6-html with no cache (the render path), and `/assets/...` from m6-file
# (what fills the cache).
#
# Needs from the caller: HERE (the tools directory), WORK, PORT.

PIDS=()
HTML_SOCK="$WORK/html.sock"
FILE_SOCK="$WORK/file.sock"
SITE="$WORK/site"
CLIENT="$WORK/perf_client"
HTTP_PID=""
HTML_PID=""

stop_all() {
  local pid
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; done
  PIDS=()
  rm -f "$HTML_SOCK" "$FILE_SOCK"
}

# curl reads its URL list from a file, so one connection carries every request.
url_file() { # url_file <count> <url> <file>
  local i
  : > "$3"
  for ((i = 0; i < $1; i++)); do printf 'url = "%s"\noutput = "/dev/null"\n' "$2" >> "$3"; done
}

wait_for() { # wait_for <test...>: up to 10 s
  local i
  for ((i = 0; i < 100; i++)); do "$@" 2>/dev/null && return 0; sleep 0.1; done
  return 1
}

minflt() { awk '{ print $10 }' "/proc/$1/stat"; }
status_kb() { awk -v key="$2" '$1 == key { print $2 }' "/proc/$1/status"; }
rss_kb() { status_kb "$1" "VmRSS:"; }
mapped_kb() { status_kb "$1" "VmSize:"; }
locked_kb() { status_kb "$1" "VmLck:"; }
huge_kb() { awk '/^AnonHugePages:/ { print $2 }' "/proc/$1/smaps_rollup"; }

# make_site <fill-mib>: the site, with that many MiB of static files for the
# cache to hold. Zero still makes one small file.
make_site() {
  local fill_mib="$1" i
  mkdir -p "$WORK"
  cc -O2 -std=c11 -Wall -Wextra -Werror -o "$CLIENT" "$HERE/perf_client.c" || return 1
  rm -rf "$SITE"
  mkdir -p "$SITE/templates" "$SITE/data" "$SITE/configs" "$SITE/assets"
  # A throwaway self-signed certificate, so the run needs no fixture.
  # ECDSA P-256, with a named curve: LibreSSL's `req -newkey ec` writes explicit
  # parameters, which rustls refuses.
  openssl ecparam -name prime256v1 -genkey -noout -out "$SITE/key.pem" 2>/dev/null \
    && openssl req -x509 -new -key "$SITE/key.pem" -days 2 \
    -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost" \
    -out "$SITE/cert.pem" 2>/dev/null \
    || { echo "memcalls: openssl could not make a certificate" >&2; return 1; }
  cat > "$SITE/site.toml" <<TOML
[site]
name   = "memcalls"
domain = "localhost"

[errors]
mode = "internal"

# The load is thousands of requests from one address in a minute, and m6-http's
# default rate limit is 300: with it on, the warm-up and every request after it
# is answered 429 and the counters measure the refusal path, not the hit path.
[rate_limit]
enabled = false

[log]
level = "error"

[[backend]]
name    = "m6-html"
sockets = "$HTML_SOCK"

[[backend]]
name    = "m6-file"
sockets = "$FILE_SOCK"

[[route]]
path    = "/"
backend = "m6-html"

[[route]]
path    = "/render"
backend = "m6-html"

[[route_group]]
glob    = "assets/**/*"
path    = "/assets/{relpath}"
backend = "m6-file"
TOML
  cat > "$WORK/system.toml" <<TOML
[node]
name = "memcalls"

[server]
bind     = "127.0.0.1:$PORT"
tls_cert = "$SITE/cert.pem"
tls_key  = "$SITE/key.pem"
TOML
  # `/` is cacheable, so m6-http answers it from its cache after the first
  # request. `/render` is no-store, so m6-html renders it every time.
  cat > "$SITE/configs/m6-html.conf" <<TOML
global_params = ["data/site.json"]

[[route]]
path     = "/"
template = "templates/home.html"

[[route]]
path     = "/render"
template = "templates/home.html"
cache    = "no-store"
TOML
  cat > "$SITE/configs/m6-file.conf" <<TOML
[[route]]
path    = "/assets/{relpath}"
handler = "files"
root    = "assets/"
TOML
  cat > "$SITE/templates/home.html" <<'HTML'
<!doctype html>
<html><head><meta charset="utf-8"><title>memcalls</title></head><body><h1>memcalls</h1><p>ok</p></body></html>
HTML
  echo '{"site_name":"memcalls"}' > "$SITE/data/site.json"
  echo ok > "$SITE/assets/small.txt"
  # 256 KiB each, so the cache fills with many entries, as a real one does.
  for ((i = 0; i < fill_mib * 4; i++)); do
    head -c 262144 /dev/urandom > "$SITE/assets/blob-$i.bin"
  done
}

# start_stack <dir> <label>: m6-html, m6-file and m6-http from <dir>, waiting
# until m6-http answers. Sets HTML_PID and HTTP_PID.
start_stack() {
  local dir="$1" label="$2"
  M6_SOCKET_OVERRIDE="$HTML_SOCK" "$dir/m6-html" "$SITE" "$SITE/configs/m6-html.conf" \
    --log-level error > "$WORK/html-$label.log" 2>&1 &
  HTML_PID=$!
  PIDS+=("$HTML_PID")
  M6_SOCKET_OVERRIDE="$FILE_SOCK" "$dir/m6-file" "$SITE" "$SITE/configs/m6-file.conf" \
    --log-level error > "$WORK/file-$label.log" 2>&1 &
  PIDS+=("$!")
  wait_for test -S "$HTML_SOCK" -a -S "$FILE_SOCK" \
    || { echo "memcalls: the backends never bound their sockets, see $WORK/html-$label.log" >&2; return 1; }
  "$dir/m6-http" "$SITE" "$WORK/system.toml" --log-level info > "$WORK/http-$label.log" 2>&1 &
  HTTP_PID=$!
  PIDS+=("$HTTP_PID")
  wait_for curl -sk -o /dev/null --fail "https://127.0.0.1:$PORT/" \
    || { echo "memcalls: m6-http never answered, see $WORK/http-$label.log" >&2; return 1; }
}
