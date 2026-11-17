#!/usr/bin/env bash
# check-commit-hashes.sh: every commit hash a document names must exist here.
#
# Usage:
#   ./tools/check-commit-hashes.sh
#
# Why this exists. A history rewrite changes every SHA in the repository at
# once, including the ones written into the documents, and nothing notices. On
# 2026-09-26 the whole history was rewritten and force-pushed, 83e4142
# repointed thirteen dead hashes in docs/PERFORMANCE.md and said so in its
# message, and the rewrite after that one killed the replacements too. At
# 024ec86 all 22 commit references in that document are dead again. The
# repository had no docs check at all (ISS-128: no docs script in tools/, no
# xtask), so the next rewrite would be just as silent.
#
# What counts as a commit reference. Every lowercase hex token of 7 to 40
# characters in every tracked Markdown file is a candidate. A candidate is
# treated as a commit unless it is provably something else.
#
#   - A decimal integer, unless the same line also carries a token with a
#     letter a-f. `31536000` is a max-age, `1048576` a size, `1784823` ns an
#     allocation count. `1005519` is a commit, and the line that carries it
#     also carries `e1ed6dd`, so the letter on the line is what tells them
#     apart. A decimal commit abbreviation alone on a line would be missed,
#     which is the one cost accepted here: flagging every number in the docs
#     gets the checker turned off.
#   - A colour or a C-style constant: #aabbcc, 0x...
#   - A UUID segment: 550e8400-e29b-41d4-a716-446655440000
#   - A content hash in an ETag, or a versioned-asset query (?v=...)
#   - An illustrative build string, "build 0123456789ab"
#
# A hash that exists is live. A hash that does not exist is a failure unless it
# is listed below as known dead history, and then it is printed every run so the
# list stays visible. Each entry names the one document it is allowed in, so
# copying a dead hash into another document still fails. The known-dead list is
# self-cleaning: an entry that resolves is itself an error, because that means
# someone repointed the hash and forgot to remove the allowance.
#
# Only git ls-files and git cat-file are used: no network, no cargo, no testers.
# A shallow clone cannot answer the question, so it is refused rather than
# reported as a tree full of dead hashes. CI checks out full history for it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

if [[ "$(git rev-parse --is-shallow-repository 2>/dev/null)" == "true" ]]; then
  echo "check-commit-hashes: this is a shallow clone, so a missing object proves nothing." >&2
  echo "Fetch the full history (git fetch --unshallow), or run this from CI." >&2
  exit 2
fi

# ── Known dead, and why each is left alone ───────────────────────────────────
# These name commits that were in this repository and are not any more. They are
# historical record: the build a measurement was taken from, in a document that
# dates the measurement.
#
# The pre-2026-09-26 hashes. The copy-removal commit table in
# docs/PERFORMANCE.md carried them under an "in older records" column, and the
# #127 pass removed that table, so no document cites these five now. Table 5 of
# that document is the allocation counts. They stay listed as history, each
# allowed only in a document that already carries it:
KNOWN_DEAD_PRE_REWRITE="1b05a18 479f1df 13280a5 5eb9af3 22ee3a4"
# The post-2026-09-26 hashes 83e4142 repointed to, the rest of the same
# measurement history, and the build README.md's throughput table was measured
# on. A rewrite after 83e4142 removed these as well, which is the second silent
# invalidation this check exists to stop:
KNOWN_DEAD_LATER="4df91be 18d2efe bf17313 fd77816 d7020aa e1ed6dd 1005519 a094851 7932e43 1d40724 dc9be74 8c79ee7 0759ad4 72e155d 3c3ea14 4acf0e0 378c535 0a68cda"
# Another repository's commits. CHANGELOG.md names the quiche fork and says so,
# so git cat-file here can never resolve them. That is correct, not dead.
FOREIGN="efff6235 bdf00278 916a5a25 4bbba71e"

# Every known-dead and foreign entry is allowed in exactly these documents. A
# hash is not a licence to write it anywhere: it is allowed where history
# already records it.
allowed_in() { # hash document -> 0 if this hash is recorded as history there
  local h=$1 f=$2
  case "$f" in
    docs/PERFORMANCE.md | README.md)
      case " $KNOWN_DEAD_PRE_REWRITE $KNOWN_DEAD_LATER " in *" $h "*) return 0 ;; esac
      ;;
    CHANGELOG.md)
      case " $FOREIGN " in *" $h "*) return 0 ;; esac
      ;;
  esac
  return 1
}

# ── Classification ────────────────────────────────────────────────────────────
is_commit_candidate() { # token line has_hex_letter -> 0 if the token can be a commit
  local tok=$1 line=$2 has_hex=$3

  # A decimal integer, unless its line carries a token with a letter a-f.
  if [[ $tok =~ ^[0-9]+$ && $has_hex -eq 0 ]]; then
    return 1
  fi

  # A colour (#aabbcc) or a numeric constant (0x...).
  [[ $line == *"#$tok"* || $line == *"0x$tok"* ]] && return 1

  # A UUID segment: the token borders a hyphen inside a full UUID.
  if [[ $line =~ [0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12} ]]; then
    [[ $line == *"$tok-"* || $line == *"-$tok"* ]] && return 1
  fi

  # A content hash, as an ETag value or a ?v= asset version.
  if [[ $line == *etag* || $line == *ETag* ]]; then
    [[ $line == *"\"$tok\""* || $line == *"\"$tok-"* ]] && return 1
  fi
  [[ $line == *"?v=$tok"* ]] && return 1

  # An illustrative build string, "build 0123456789ab".
  [[ $line == *"build $tok"* ]] && return 1

  return 0
}

# ── Scan ──────────────────────────────────────────────────────────────────────
files=0
tokens=0
candidates=0
live=0
known_dead=0
foreign=0
failures=0

known_dead_output=""
failure_output=""

while IFS= read -r file; do
  files=$((files + 1))
  while IFS=: read -r lineno text; do
    [[ -n $text ]] || continue
    line_tokens="$(printf '%s\n' "$text" | grep -owE '[0-9a-f]{7,40}' || true)"
    [[ -n $line_tokens ]] || continue

    # Does this line carry a token that can only be hex, not a decimal integer?
    has_hex=0
    while IFS= read -r t; do
      [[ $t == *[a-f]* ]] && has_hex=1
    done <<< "$line_tokens"

    while IFS= read -r tok; do
      [[ -n $tok ]] || continue
      tokens=$((tokens + 1))
      is_commit_candidate "$tok" "$text" "$has_hex" || continue
      candidates=$((candidates + 1))

      if allowed_in "$tok" "$file"; then
        if [[ " $FOREIGN " == *" $tok "* ]]; then
          foreign=$((foreign + 1))
        else
          known_dead=$((known_dead + 1))
          known_dead_output+="  $file:$lineno  $tok"$'\n'
        fi
        continue
      fi

      if git cat-file -e "$tok^{commit}" 2>/dev/null; then
        live=$((live + 1))
      else
        failures=$((failures + 1))
        failure_output+="  $file:$lineno  $tok does not name a commit here"$'\n'
      fi
    done <<< "$line_tokens"
  done < <(grep -nE '[0-9a-f]{7,40}' "$file" || true)
done < <(git ls-files '*.md')

# A known-dead entry that now resolves has been repointed: drop the allowance.
stale=0
for h in $KNOWN_DEAD_PRE_REWRITE $KNOWN_DEAD_LATER; do
  if git cat-file -e "$h^{commit}" 2>/dev/null; then
    echo "check-commit-hashes: $h is listed as dead history but resolves at this tip." >&2
    echo "  The document was repointed, so remove $h from tools/check-commit-hashes.sh." >&2
    stale=$((stale + 1))
  fi
done

tip="$(git rev-parse --short=7 HEAD 2>/dev/null || echo 'unknown')"
echo "commit-hash check at $tip: $files Markdown documents, $tokens hex tokens"
echo "  $candidates name a commit: $live live, $known_dead known-dead history, $foreign another repository's"

if [[ -n $known_dead_output ]]; then
  echo "known-dead history (left as written, each allowed only in its own document):"
  printf '%s' "$known_dead_output"
fi

if [[ -n $failure_output ]]; then
  echo >&2
  echo "FAIL: a document names a commit that does not exist here:" >&2
  printf '%s' "$failure_output" >&2
  echo >&2
  echo "Repoint it to the commit that carries the work now, recovering the mapping" >&2
  echo "by matching subjects and diffs as 83e4142 did, or record it in" >&2
  echo "tools/check-commit-hashes.sh as historical with the reason." >&2
fi

if [[ $stale -gt 0 || $failures -gt 0 ]]; then
  exit 1
fi

echo "OK: every commit hash a document names exists here, or is named as dead history."
