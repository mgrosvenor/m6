#!/usr/bin/env bash
# find-deployment.sh — locate the deployment repository, without naming one.
#
# Sourced by tools/merge.sh, tools/release.sh and tools/perfcheck.sh. Sets
# DEPLOYMENT to a directory containing `deploy/run-tests.sh`.
#
# ── Why this exists ──────────────────────────────────────────────────────────
#
# m6 is a generic web system. A site built on it is a separate repository, and
# which one is a property of whoever is working here, not of m6.
#
# Those three scripts used to default to one particular site's directory under
# $HOME, which is somebody's own deployment appearing in the tooling of a generic
# system. An override existed, but the default is what anyone actually used, so
# the name was load-bearing in practice.
#
# ── How it decides ───────────────────────────────────────────────────────────
#
# 1. `$DEPLOYMENT_REPO` if set. Explicit always wins.
# 2. `$SITE_REPO`, the name the old scripts used, so existing shells and CI
#    configuration keep working.
# 3. Otherwise, look for exactly one sibling of this checkout that contains
#    `deploy/run-tests.sh`. Exactly one: two candidates is ambiguous and gets
#    an error naming both rather than a guess between them.
#
# A guess here is worse than a failure. These scripts run the checks that decide
# whether a merge or a release happens, so picking the wrong deployment
# repository would run the wrong checks and report them as this one's.

_m6_find_deployment() {
    local root="$1"

    if [[ -n "${DEPLOYMENT_REPO:-}" ]]; then
        if [[ -x "$DEPLOYMENT_REPO/deploy/run-tests.sh" ]]; then
            printf '%s\n' "$DEPLOYMENT_REPO"
            return 0
        fi
        echo "DEPLOYMENT_REPO is set to '$DEPLOYMENT_REPO' but there is no" \
             "executable deploy/run-tests.sh in it." >&2
        return 1
    fi

    if [[ -n "${SITE_REPO:-}" ]]; then
        if [[ -x "$SITE_REPO/deploy/run-tests.sh" ]]; then
            printf '%s\n' "$SITE_REPO"
            return 0
        fi
        echo "SITE_REPO is set to '$SITE_REPO' but there is no executable" \
             "deploy/run-tests.sh in it." >&2
        return 1
    fi

    local parent found=() d
    parent="$(cd "$root/.." && pwd)"
    for d in "$parent"/*; do
        [[ -d "$d" ]] || continue
        [[ "$d" == "$root" ]] && continue
        [[ -x "$d/deploy/run-tests.sh" ]] && found+=("$d")
    done

    case "${#found[@]}" in
        1) printf '%s\n' "${found[0]}"; return 0 ;;
        0) echo "cannot find a deployment repository.

  These checks run from a deployment repository's deploy/run-tests.sh, because
  that is what knows the build host and the site being served. m6 does not know
  which deployment is yours.

  Point at it:   export DEPLOYMENT_REPO=/path/to/your-site
  Or place it beside this checkout, as a sibling directory containing
  deploy/run-tests.sh." >&2
           return 1 ;;
        *) echo "found more than one deployment repository beside this checkout:" >&2
           printf '    %s\n' "${found[@]}" >&2
           echo "
  Which one is ambiguous, and running the wrong one's checks would report them
  as this one's. Say which: export DEPLOYMENT_REPO=..." >&2
           return 1 ;;
    esac
}
