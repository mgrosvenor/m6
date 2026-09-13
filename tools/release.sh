#!/usr/bin/env bash
# release.sh — cut a release: develop into main, with a changelog entry and a tag.
#
#   ./tools/release.sh 0.3.0
#
# main only ever advances this way. It is what the world sees on GitHub, so it
# only ever holds released work, and every release says what changed.
#
# What it does:
#   1. refuses unless develop is clean, current, and passes everything
#   2. refuses unless CHANGELOG.md has an entry for this version
#   3. merges develop into main, no fast-forward, so the release is one commit
#   4. tags it v<version>
#   5. pushes main and the tag, with M6_RELEASE=1 so the hook allows it
set -uo pipefail
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RESET='\033[0m'
die() { echo -e "${RED}$*${RESET}" >&2; exit 1; }
note() { echo -e "${YELLOW}----${RESET} $*"; }

VERSION="${1:-}"
[[ -n "$VERSION" ]] || die "usage: $0 <version>   e.g. $0 0.3.0"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must be MAJOR.MINOR.PATCH, got '$VERSION'"
TAG="v$VERSION"

git rev-parse --verify "$TAG" >/dev/null 2>&1 && die "$TAG already exists"
git diff --quiet && git diff --cached --quiet || die "working tree is dirty"

grep -qE "^## +(\[)?$VERSION" CHANGELOG.md \
  || die "CHANGELOG.md has no '## $VERSION' section.

  A release says what changed in it. Write the entry first: what changed, why
  it mattered, and how it was verified."

note "checking out develop"
git checkout develop --quiet || die "no develop branch"
git pull --ff-only --quiet || die "develop has diverged from origin"

SITE="${SITE_REPO:-$HOME/dr-grosvenor-site}"
[[ -x "$SITE/deploy/run-tests.sh" ]] || die "cannot find $SITE/deploy/run-tests.sh (set SITE_REPO)"
note "running everything on the build host before cutting $TAG"
( cd "$SITE" && ./deploy/run-tests.sh ) || die "checks failed. No release."

note "merging develop into main"
git checkout main --quiet || die "no main branch"
git pull --ff-only --quiet || die "main has diverged from origin"
git merge --no-ff --no-commit develop || die "merge conflict between develop and main"
git commit --quiet -m "Release $TAG

$(sed -n "/^## \+\(\[\)\?$VERSION/,/^## /p" CHANGELOG.md | sed '$d' | tail -n +2 | head -40)

Checks: tests, clippy, h1/h2/h3 conformance, performance — all passed on the
build host at $(date -u '+%Y-%m-%d %H:%M UTC')." || die "release commit failed"

git tag -a "$TAG" -m "Release $TAG" || die "could not tag"

note "pushing main and $TAG"
M6_RELEASE=1 git push origin main || die "push to main failed"
git push origin "$TAG" || die "tag push failed"

git checkout develop --quiet
echo -e "${GREEN}released $TAG${RESET}"
echo "main now holds $TAG. develop is where work continues."
