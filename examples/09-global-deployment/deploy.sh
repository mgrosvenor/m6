#!/usr/bin/env bash
# deploy.sh — deploy site content to origin and optionally all cache nodes.
#
# Usage:
#   ./deploy.sh [config.toml] [flags]
#
#   config.toml defaults to deploy.toml in this directory.
#   Use deploy.local.toml to deploy against the local dev stack.
#
# Flags:
#   --all         also push (limited) content to cache nodes in parallel
#   --binary      rebuild render-cms and push to origin; restart the service
#   --invalidate  touch site.toml on each deployed node to flush the HTTP cache
#
# Examples:
#   ./deploy.sh                               # production deploy to origin
#   ./deploy.sh --all --invalidate            # production, all nodes + flush
#   ./deploy.sh deploy.local.toml --invalidate  # local dev flush
#
# Git workflow:
#   1. Edit content locally (content/posts/, templates/, assets/)
#   2. git commit && git push
#   3. ./deploy.sh --all --invalidate         (or trigger from CI on main push)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# ── Parse argv ────────────────────────────────────────────────────────────────
CONFIG=""
DEPLOY_ALL=false
PUSH_BINARY=false
INVALIDATE=false

for arg in "$@"; do
    case "$arg" in
        --all)        DEPLOY_ALL=true ;;
        --binary)     PUSH_BINARY=true ;;
        --invalidate) INVALIDATE=true ;;
        --*)          echo "Unknown flag: $arg" >&2; exit 1 ;;
        *)            CONFIG="$arg" ;;
    esac
done

[[ -z "$CONFIG" ]] && CONFIG="$SCRIPT_DIR/deploy.toml"

if [[ ! -f "$CONFIG" ]]; then
    echo "ERROR: config not found: $CONFIG" >&2
    echo "Copy deploy.toml.example to deploy.toml and fill in your node addresses." >&2
    exit 1
fi

# ── TOML parser ───────────────────────────────────────────────────────────────
# Reads [section] key = "value" pairs and exports them as TOML_section_key.
# Handles quoted and unquoted values; ignores comments and blank lines.
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
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", val)  # trim
            gsub(/^"|"$/, "", val)                         # strip quotes
            gsub(/#.*$/, "", val)                          # strip inline comment
            gsub(/[[:space:]]+$/, "", val)                 # trim again
            printf "TOML_%s_%s=%s\n", section, key, val
        }
    ' "$file"
}

# Load the config into environment variables.
eval "$(parse_toml "$CONFIG")"

# Resolve config values (with defaults).
ORIGIN="${TOML_deploy_origin:-}"
SITE_DIR="${TOML_deploy_site_dir:-/var/www/my-blog}"
SSH_OPTS="${TOML_deploy_ssh_opts:--o StrictHostKeyChecking=accept-new -o ConnectTimeout=15}"

if [[ -z "$ORIGIN" ]]; then
    echo "ERROR: [deploy] origin not set in $CONFIG" >&2
    exit 1
fi

# Collect cache node names and targets from [cache_nodes] section.
# parse_toml outputs TOML_cache_nodes_NAME=HOST for each entry.
declare -A CACHE_NODES
while IFS='=' read -r var host; do
    [[ "$var" == TOML_cache_nodes_* && -n "$host" ]] || continue
    name="${var#TOML_cache_nodes_}"
    CACHE_NODES["$name"]="$host"
done < <(parse_toml "$CONFIG")

echo "Config: $CONFIG"
echo "Origin: $ORIGIN  site_dir: $SITE_DIR"
[[ ${#CACHE_NODES[@]} -gt 0 ]] && \
    echo "Cache nodes: $(printf '%s ' "${!CACHE_NODES[@]}")"
echo ""

# ── Deploy to origin ──────────────────────────────────────────────────────────
echo "==> Syncing to origin ($ORIGIN)..."
# shellcheck disable=SC2086
rsync -az --delete \
    $SSH_OPTS \
    --exclude 'content/drafts/' \
    --exclude 'data/posts.json' \
    --exclude 'keys/' \
    --exclude '*.pem' \
    --exclude '*.pub' \
    --exclude 'target/' \
    --exclude '.git/' \
    --exclude 'logs/' \
    "$SCRIPT_DIR/../07-dev-to-production/" \
    "$ORIGIN:$SITE_DIR/"

# Set ownership (skip if deploying locally — may not have sudo)
# shellcheck disable=SC2086
ssh $SSH_OPTS "$ORIGIN" "id m6 &>/dev/null && chown -R m6:m6 '$SITE_DIR' || true"

# Rebuild posts.json from markdown on the server
# shellcheck disable=SC2086
ssh $SSH_OPTS "$ORIGIN" "m6-md '$SITE_DIR/content/posts/' --output '$SITE_DIR/data/posts.json'"

if $PUSH_BINARY; then
    echo "==> Building render-cms binary..."
    cargo build --release -p render-cms
    echo "==> Pushing binary to origin..."
    # shellcheck disable=SC2086
    rsync -az $SSH_OPTS \
        "$SCRIPT_DIR/../../target/release/render-cms" \
        "$ORIGIN:$SITE_DIR/bin/"
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$ORIGIN" "systemctl restart render-cms 2>/dev/null || true"
fi

echo "==> Origin deploy complete."

# ── Propagate to cache nodes ──────────────────────────────────────────────────
if $DEPLOY_ALL && [[ ${#CACHE_NODES[@]} -gt 0 ]]; then
    echo ""
    echo "==> Propagating to ${#CACHE_NODES[@]} cache node(s)..."

    PIDS=()
    for name in "${!CACHE_NODES[@]}"; do
        target="${CACHE_NODES[$name]}"
        (
            echo "  → $name ($target): syncing..."
            # Cache nodes only need templates + assets for error pages.
            # Content is always fetched live from origin over H2C.
            # shellcheck disable=SC2086
            rsync -az --delete \
                $SSH_OPTS \
                --exclude 'content/' \
                --exclude 'data/' \
                --exclude 'keys/' \
                --exclude '*.pem' \
                --exclude '*.pub' \
                --exclude 'target/' \
                --exclude '.git/' \
                --exclude 'logs/' \
                "$SCRIPT_DIR/../07-dev-to-production/" \
                "$target:$SITE_DIR/"
            # shellcheck disable=SC2086
            ssh $SSH_OPTS "$target" "id m6 &>/dev/null && chown -R m6:m6 '$SITE_DIR' || true"
            echo "  ✓ $name done"
        ) &
        PIDS+=($!)
    done

    FAILED=0
    for pid in "${PIDS[@]}"; do wait "$pid" || { echo "WARNING: a cache node failed"; FAILED=1; }; done
    [[ $FAILED -eq 0 ]] && echo "==> All cache nodes synced." \
                        || { echo "==> Some cache nodes failed." >&2; exit 1; }
fi

# ── Invalidate caches ─────────────────────────────────────────────────────────
if $INVALIDATE; then
    echo ""
    echo "==> Invalidating HTTP caches (touch site.toml)..."
    PIDS=()

    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$ORIGIN" "touch '$SITE_DIR/site.toml'" &
    PIDS+=($!)

    if $DEPLOY_ALL; then
        for name in "${!CACHE_NODES[@]}"; do
            target="${CACHE_NODES[$name]}"
            # shellcheck disable=SC2086
            ssh $SSH_OPTS "$target" "touch '$SITE_DIR/site.toml'" &
            PIDS+=($!)
        done
    fi

    for pid in "${PIDS[@]}"; do wait "$pid" || true; done
    echo "==> Caches invalidated."
fi

echo ""
echo "Deploy complete."
