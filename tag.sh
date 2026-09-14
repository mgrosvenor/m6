#!/usr/bin/env bash
# tag.sh — tag a release that has already been merged into main, and push it.
#
# Usage:
#   ./tag.sh v1.1.0
#   ./tag.sh v1.1.0 --dry-run   # check everything, tag nothing
#
# ── This no longer runs the checks, and that is the point ─────────────────────
#
# It used to run the whole suite including benchmarks. That made sense when a tag
# was the first moment anything had been verified. It is not any more: a release
# reaches main only by PULL REQUEST, and that pull request has already run the
# full set in CI plus the changelog and version checks. Running it again here
# re-measures what is already known and makes tagging a ten-minute operation
# people put off.
#
# What it does check is what a tag can still get wrong: the wrong branch, a dirty
# tree, a version that disagrees with Cargo.toml, a tag that already exists, and a
# main that is behind the remote.
#
# The original header's warning is worth keeping, because it came true on
# 2026-09-14: slow work inside a push breaks the push. git opens its connection,
# runs the pre-push hook, then sends the pack — and a hook that takes ten minutes
# lets the server close the connection, so git dies with SIGPIPE and no message.
# The hook is milliseconds now. Do not put a suite back into either of them.

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

# ── What a tag can still get wrong ───────────────────────────────────────────
VERSION="${TAG#v}"
CARGO_VERSION="$(grep -m1 '^version' "$SCRIPT_DIR/Cargo.toml" | cut -d'"' -f2)"
if [[ "$VERSION" != "$CARGO_VERSION" ]]; then
  echo -e "${RED}ERROR${RESET}: $TAG does not match Cargo.toml's version ($CARGO_VERSION)."
  echo "  A tag that disagrees with the version it names is worse than no tag."
  exit 1
fi

if git rev-parse "$TAG" >/dev/null 2>&1; then
  echo -e "${RED}ERROR${RESET}: $TAG already exists. Bump the version instead of moving a tag."
  exit 1
fi

git fetch origin --quiet 2>/dev/null || true
if [[ -n "$(git rev-list "HEAD..origin/main" 2>/dev/null)" ]]; then
  echo -e "${RED}ERROR${RESET}: main is behind origin/main. Pull the merged pull request first."
  exit 1
fi

if ! grep -qE "^## +\[?${VERSION}" "$SCRIPT_DIR/CHANGELOG.md"; then
  echo -e "${RED}ERROR${RESET}: CHANGELOG.md has no '## ${VERSION}' section."
  exit 1
fi

echo -e "${GREEN}ok${RESET} on main, clean, version matches, changelog entry present, up to date with origin"

if [[ "$DRY_RUN" == "true" ]]; then
  echo -e "${YELLOW}----${RESET} Dry run — nothing tagged."
  exit 0
fi

# ── Tag and push ──────────────────────────────────────────────────────────────
echo -e "${YELLOW}----${RESET} Tagging $TAG..."
# NOT `-f`. Moving an existing tag rewrites what a release points at, and
# anything that pinned it — the site's renderers pin `tag = "v1.0.0"` — silently
# gets different code.
git tag -a "$TAG" -m "Release $TAG"

echo -e "${YELLOW}----${RESET} Pushing $TAG..."
# M6_RELEASE=1: the hook allows a release tag only when it is set, which is what
# stops a tag being pushed by hand without going through this script.
M6_RELEASE=1 git push origin "$TAG"

echo ""
echo -e "${GREEN}Released $TAG.${RESET}"
echo "  https://github.com/mgrosvenor/m6/releases/tag/$TAG"
