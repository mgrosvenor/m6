#!/usr/bin/env bash
# tag.sh — run the full check suite (including benchmarks) then create and
# push a release tag.
#
# Usage:
#   ./tag.sh v0.3.0          # full check → tag → push
#   ./tag.sh v0.3.0 --dry-run  # full check only, no tag/push
#
# The pre-push hook only runs build + tests (~30s).  This script is the right
# place to gate tags on benchmark results because it runs locally before
# creating the tag, avoiding SSH-timeout issues that occur when benchmarks run
# inside the network-push hook.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; RESET='\033[0m'

# ── Args ──────────────────────────────────────────────────────────────────────
TAG=""
DRY_RUN=false
for arg in "$@"; do
  case "$arg" in
    v[0-9]*)    TAG="$arg" ;;
    --dry-run)  DRY_RUN=true ;;
    *) echo "Usage: $0 <tag> [--dry-run]"; exit 1 ;;
  esac
done

if [[ -z "$TAG" ]]; then
  echo "Usage: $0 <tag> [--dry-run]"
  exit 1
fi

# ── Guard: must be on clean main ──────────────────────────────────────────────
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$BRANCH" != "main" ]]; then
  echo -e "${RED}ERROR${RESET}: must be on main branch (currently on '$BRANCH')"
  exit 1
fi

if [[ -n "$(git status --porcelain)" ]]; then
  echo -e "${RED}ERROR${RESET}: working tree is dirty — commit or stash changes first"
  exit 1
fi

echo -e "${YELLOW}----${RESET} Running full check suite for tag $TAG..."
echo ""
"$SCRIPT_DIR/check.sh"

echo ""
if [[ "$DRY_RUN" == "true" ]]; then
  echo -e "${YELLOW}----${RESET} Dry run — skipping tag and push"
  echo -e "${GREEN}Full check passed.${RESET} Run without --dry-run to tag and push."
  exit 0
fi

# ── Tag and push ──────────────────────────────────────────────────────────────
echo -e "${YELLOW}----${RESET} Tagging $TAG..."
git tag -f "$TAG"

echo -e "${YELLOW}----${RESET} Pushing $TAG..."
# Push the tag directly (pre-push hook runs --no-bench; benchmarks already done above).
git push origin "$TAG"

echo ""
echo -e "${GREEN}Released $TAG.${RESET}"
echo "  https://github.com/mgrosvenor/m6/releases/tag/$TAG"
