#!/usr/bin/env bash
# dev.sh — start the 08-logviewer example from ~/m6-examples.
#
# Builds binaries from this repo, then delegates to the example's dev.sh
# which starts m6-html + m6-file + m6-http, tails process logs into
# logs/*.log, and opens the live log viewer in the browser.
#
# Prerequisites:
#   brew install mkcert   (first run only — generates trusted dev TLS cert)
set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
EXAMPLE="$HOME/m6-examples/examples/08-logviewer"

if [[ ! -d "$EXAMPLE" ]]; then
    echo "ERROR: $EXAMPLE not found." >&2
    exit 1
fi

M6="$REPO" exec "$EXAMPLE/dev.sh" "$@"
