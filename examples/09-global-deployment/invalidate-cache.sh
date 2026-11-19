#!/usr/bin/env bash
# invalidate-cache.sh — flush the in-memory HTTP cache on all nodes.
#
# m6-http flushes its cache and reloads routing when site.toml is touched.
# This script touches site.toml on every node simultaneously over SSH.
#
# Usage:
#   ./invalidate-cache.sh [config.toml] [node ...]
#
#   config.toml defaults to deploy.toml in this directory.
#   Optionally pass specific node names to invalidate only those nodes.
#
# Examples:
#   ./invalidate-cache.sh                         # all nodes (production)
#   ./invalidate-cache.sh deploy.local.toml       # local dev
#   ./invalidate-cache.sh deploy.toml edge-a edge-d   # specific nodes only

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# ── Parse argv ────────────────────────────────────────────────────────────────
CONFIG=""
TARGETS=()

for arg in "$@"; do
    case "$arg" in
        *.toml) CONFIG="$arg" ;;
        *)      TARGETS+=("$arg") ;;
    esac
done

[[ -z "$CONFIG" ]] && CONFIG="$SCRIPT_DIR/deploy.toml"

if [[ ! -f "$CONFIG" ]]; then
    echo "ERROR: config not found: $CONFIG" >&2
    exit 1
fi

# ── TOML parser (same as deploy.sh) ──────────────────────────────────────────
parse_toml() {
    local file="$1"
    awk '
        /^[[:space:]]*#/  { next }
        /^[[:space:]]*$/  { next }
        /^\[/ {
            section = substr($0, 2, index($0, "]") - 2)
            gsub(/[^a-zA-Z0-9_]/, "_", section)
            next
        }
        /=/ {
            key = $1
            val = substr($0, index($0, "=") + 1)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", val)
            gsub(/^"|"$/, "", val)
            gsub(/#.*$/, "", val)
            gsub(/[[:space:]]+$/, "", val)
            printf "TOML_%s_%s=%s\n", section, key, val
        }
    ' "$file"
}

eval "$(parse_toml "$CONFIG")"

ORIGIN="${TOML_deploy_origin:-}"
SITE_DIR="${TOML_deploy_site_dir:-/var/www/my-blog}"
SSH_OPTS="${TOML_deploy_ssh_opts:--o StrictHostKeyChecking=accept-new -o ConnectTimeout=10}"

declare -A ALL_NODES
[[ -n "$ORIGIN" ]] && ALL_NODES[origin]="$ORIGIN"
while IFS='=' read -r var host; do
    [[ "$var" == TOML_cache_nodes_* && -n "$host" ]] || continue
    ALL_NODES["${var#TOML_cache_nodes_}"]="$host"
done < <(parse_toml "$CONFIG")

# Default: all nodes
if [[ ${#TARGETS[@]} -eq 0 ]]; then
    TARGETS=("${!ALL_NODES[@]}")
fi

echo "Invalidating: ${TARGETS[*]}"

PIDS=()
for name in "${TARGETS[@]}"; do
    target="${ALL_NODES[$name]:?Unknown node: $name}"
    echo "  → $name ($target)"
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$target" "touch '$SITE_DIR/site.toml'" &
    PIDS+=($!)
done

FAILED=0
for pid in "${PIDS[@]}"; do
    wait "$pid" || { echo "WARNING: $name failed"; FAILED=1; }
done

[[ $FAILED -eq 0 ]] && echo "Done — ${#TARGETS[@]} node(s) invalidated." \
                    || { echo "Completed with errors." >&2; exit 1; }
