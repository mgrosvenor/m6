#!/usr/bin/env bash
# branch.sh — start work on an issue.
#
#   ./tools/branch.sh 42 h3-gate-measures-nothing
#   ./tools/branch.sh 42 h3-gate-measures-nothing --type fix
#
# Branches always come off develop, never off main, and always name the issue
# they belong to. The issue has to exist: a branch named after an issue nobody
# filed is a branch nobody can find later.
set -uo pipefail
RED='\033[0;31m'; GREEN='\033[0;32m'; RESET='\033[0m'
die() { echo -e "${RED}$*${RESET}" >&2; exit 1; }

TYPE=feat
ISSUE=""; SLUG=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --type) TYPE="$2"; shift 2 ;;
    *) if [[ -z "$ISSUE" ]]; then ISSUE="$1"; else SLUG="${SLUG:+$SLUG-}$1"; fi; shift ;;
  esac
done
[[ -n "$ISSUE" && -n "$SLUG" ]] || die "usage: $0 <issue-number> <slug> [--type feat|fix|perf|docs|refactor|test|chore]"
[[ "$ISSUE" =~ ^[0-9]+$ ]] || die "issue must be a number, got '$ISSUE'"
case "$TYPE" in feat|fix|perf|docs|refactor|test|chore) ;; *) die "unknown type '$TYPE'" ;; esac

if command -v gh >/dev/null 2>&1; then
  gh issue view "$ISSUE" >/dev/null 2>&1 \
    || die "issue #$ISSUE does not exist. File it first: gh issue create"
else
  echo "warning: gh not installed, cannot confirm issue #$ISSUE exists" >&2
fi

git diff --quiet && git diff --cached --quiet || die "working tree is dirty; commit or stash first"
git fetch origin develop --quiet || die "cannot reach origin"
git checkout develop --quiet || die "no develop branch"
git pull --ff-only --quiet || die "develop has diverged; sort that out first"

BRANCH="$TYPE/$ISSUE-$SLUG"
git checkout -b "$BRANCH" || die "could not create $BRANCH"
echo -e "${GREEN}on $BRANCH, from develop${RESET}"
echo "when it is ready:  git push origin $BRANCH && gh pr create --base develop --fill"
