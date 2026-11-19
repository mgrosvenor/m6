#!/usr/bin/env bash
# setup-cache-node.sh — first-time setup for a cache node (edge-a/edge-b/edge-c/edge-d/edge-e).
# Usage: ./setup-cache-node.sh <node> <domain> <wg-config>
# Example: ./setup-cache-node.sh edge-a example.com wireguard/edge-a-wg0.conf
set -euo pipefail

NODE="${1:?Usage: $0 <node> <domain> <wg-conf>}"
DOMAIN="${2:?Usage: $0 <node> <domain> <wg-conf>}"
WG_CONF="${3:?Usage: $0 <node> <domain> <wg-conf>}"
SITE_DIR="/var/www/my-blog"

echo "=== Installing dependencies ==="
apt-get update -q
apt-get install -y wireguard certbot ufw

echo "=== Creating m6 user ==="
id m6 &>/dev/null || useradd --system --no-create-home --shell /usr/sbin/nologin m6

echo "=== Creating site directory ==="
mkdir -p "$SITE_DIR"
chown m6:m6 "$SITE_DIR"

echo "=== Setting up /etc/m6 ==="
mkdir -p /etc/m6

# System config
CONF_SRC="configs/cache-${NODE}.toml"
[ -f "$CONF_SRC" ] || CONF_SRC="configs/cache-edge-a.toml"   # fallback to template
install -m 640 -o root -g m6 "$CONF_SRC" "/etc/m6/cache.toml"
sed -i "s/example.com/$DOMAIN/g" /etc/m6/cache.toml

# The token this node follows the origin's site generation with. It has the genid-read
# role, belongs to this node alone (user reader-$NODE) and is minted on the origin with
# --ttl-days 90 (see setup-origin.sh, which also says how to rotate it), then copied
# here. The unit delivers it to m6-http with LoadCredential.
if [ ! -s /etc/m6/genid-read ]; then
    echo "NOTE: put the genid-read token minted for reader-$NODE in /etc/m6/genid-read (mode 600) before starting m6-http-cache."
fi

# Node config pointer for systemd unit
echo "NODE_CONFIG=/etc/m6/cache.toml" > /etc/m6/node-config

echo "=== WireGuard ==="
install -m 600 "$WG_CONF" /etc/wireguard/wg0.conf
systemctl enable --now wg-quick@wg0

echo "=== Firewall ==="
ufw allow 22/tcp
# Port 80/tcp is for the certbot certificate challenge only, nothing else listens there.
ufw allow 80/tcp
ufw allow 443/tcp
ufw allow 443/udp
ufw --force enable

echo "=== TLS certificate ==="
# Cert is for the shared domain — SAN or wildcard needed for multi-node.
# Using standalone temporarily; switch to DNS challenge for wildcard.
certbot certonly --standalone --agree-tos --non-interactive \
    --email "admin@${DOMAIN}" -d "$DOMAIN" || \
    echo "NOTE: certbot failed — run manually after DNS is pointed at this node."
# Auto-renewal hook. systemd copies the new certificate into the unit when it starts, so this restart serves it.
mkdir -p /etc/letsencrypt/renewal-hooks/deploy
cat > /etc/letsencrypt/renewal-hooks/deploy/m6-http-restart.sh << 'HOOK'
#!/bin/bash
systemctl restart m6-http-cache
HOOK
chmod +x /etc/letsencrypt/renewal-hooks/deploy/m6-http-restart.sh

echo "=== Installing systemd unit ==="
sed "s/example.com/$DOMAIN/g" systemd/m6-http-cache.service > /etc/systemd/system/m6-http-cache.service
chmod 644 /etc/systemd/system/m6-http-cache.service
systemctl daemon-reload

echo "=== Deploying site content from origin ==="
echo "Run on your dev machine:"
echo "  rsync -av --delete --exclude 'content/drafts/' \\"
echo "    --exclude 'keys/' /var/www/my-blog/ root@<THIS_NODE_IP>:/var/www/my-blog/"

echo ""
echo "Done — $NODE cache node configured."
echo "  Start: systemctl enable --now m6-http-cache"
echo "  Verify tunnel: ping 10.0.0.1"
echo "  Verify cache: curl -sk https://$DOMAIN/ | head -5"
