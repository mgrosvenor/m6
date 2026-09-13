#!/usr/bin/env bash
# merge.sh — run everything, then merge a branch into develop.
#
#   ./tools/merge.sh fix/42-h3-gate-measures-nothing
#
# Nothing reaches develop without passing, on the build host:
#   - the whole unit and integration suite
#   - zero compiler warnings, release and test builds
#   - clippy, at or under its recorded count
#   - h1, h2 and h3 conformance, at or above their recorded scores
#   - the performance check, within its margin
#
# The merge commit records what ran. The pre-push hook refuses a merge on
# develop without that record, so this is not a convention anyone can forget.
set -uo pipefail
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RESET='\033[0m'
die() { echo -e "${RED}$*${RESET}" >&2; exit 1; }
note() { echo -e "${YELLOW}----${RESET} $*"; }

BRANCH="${1:-$(git rev-parse --abbrev-ref HEAD)}"
[[ "$BRANCH" != "develop" && "$BRANCH" != "main" ]] || die "that is not a work branch"
[[ "$BRANCH" =~ ^(feat|fix|perf|docs|refactor|test|chore)/([0-9]+)- ]] \
  || die "branch '$BRANCH' does not name an issue; see tools/branch.sh"
ISSUE="${BASH_REMATCH[2]}"

git diff --quiet && git diff --cached --quiet || die "working tree is dirty"
git rev-parse --verify "$BRANCH" >/dev/null 2>&1 || die "no such branch: $BRANCH"

# Which deployment repository runs the checks is a property of this working
# copy, not of m6. See tools/find-deployment.sh.
# shellcheck source=tools/find-deployment.sh
. "$(dirname "$0")/find-deployment.sh"
SITE="$(_m6_find_deployment "$(cd "$(dirname "$0")/.." && pwd)")" || exit 1

note "running everything on the build host (this is the slow part, and the point)"
git checkout "$BRANCH" --quiet || die "cannot check out $BRANCH"
if ! ( cd "$SITE" && ./deploy/run-tests.sh m6 ); then
  die "checks failed on the build host. Nothing merged."
fi

note "checks passed; merging $BRANCH into develop"
git checkout develop --quiet || die "no develop branch"
git pull --ff-only --quiet 2>/dev/null || true

git merge --no-ff --no-commit "$BRANCH" || die "merge conflict; resolve, then re-run"
git commit --quiet -m "Merge $BRANCH into develop

Closes #$ISSUE

Checks: tests, clippy, h1/h2/h3 conformance, performance — all passed on the
build host at $(date -u '+%Y-%m-%d %H:%M UTC')." \
  || die "merge commit failed"

echo -e "${GREEN}merged $BRANCH into develop${RESET}"
echo "push with:  git push origin develop"
