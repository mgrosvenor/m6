#!/bin/bash
# deploy.sh — run from development machine
set -e

SERVER="user@example.com"
REMOTE_SITE="/var/www/my-blog"

rsync -av --delete \
  --exclude 'content/drafts/' \
  --exclude 'keys/' \
  --exclude '*.pem' \
  --exclude '*.pub' \
  --exclude 'target/' \
  ./ "$SERVER:$REMOTE_SITE/"

ssh "$SERVER" chown -R m6:m6 "$REMOTE_SITE"

if [[ "$1" == "--binary" ]]; then
  cargo build --release -p render-cms
  rsync target/release/render-cms "$SERVER:$REMOTE_SITE/bin/"
  ssh "$SERVER" systemctl restart render-cms
fi

echo "Deployed."
# restart m6-http so it reads the new site.toml
ssh "$SERVER" systemctl restart m6-http
