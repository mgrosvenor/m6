#!/usr/bin/env bash
# setup-origin.sh — first-time setup for the origin node.
# Run as root on a fresh Ubuntu 22.04 / Debian 12 VPS.
set -euo pipefail

DOMAIN="${1:?Usage: $0 <domain> <admin-email>}"
EMAIL="${2:?Usage: $0 <domain> <admin-email>}"
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

# Auth signing keys
if [ ! -f /etc/m6/auth.pem ]; then
    # `ec_param_enc:named_curve` is not redundant: macOS's LibreSSL writes explicit
    # curve parameters where OpenSSL writes the prime256v1 OID, and the token signer
    # only accepts the named form. Without it every login on a Mac fails with
    # "JWT encode error: InvalidEcdsaKey" even with the right password. The long
    # version is in examples/05-cms/setup.sh.
    openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -pkeyopt ec_param_enc:named_curve -out /etc/m6/auth.pem
    openssl pkey -in /etc/m6/auth.pem -pubout -out /etc/m6/auth.pub
    chmod 600 /etc/m6/auth.pem
    chmod 600 /etc/m6/auth.pub
fi

# m6-auth config. systemd reads a credential as root, so the two key files stay root's.
cat > /etc/m6/m6-auth.conf << 'CONF'
[storage]
path = "/var/lib/m6-auth/auth.db"

[tokens]
issuer = "example.com"

[keys]
private_key = "/run/credentials/m6-auth.service/auth.pem"
public_key  = "/run/credentials/m6-auth.service/auth.pub"
CONF
chown root:m6 /etc/m6/m6-auth.conf
chmod 640 /etc/m6/m6-auth.conf
sed -i "s/example.com/$DOMAIN/g" /etc/m6/m6-auth.conf

# The origin is the source of the site's generation ID. A publish is one
# `POST /genid` to it, with a token that has the genid-write role, and each cache
# node follows the ID with a token that has the genid-read role. Each cache node
# gets its own reader, so one node's token can be revoked alone. Once the auth
# database exists (run the unit, then):
#
#   CONF=/etc/m6/m6-auth.conf
#   m6-auth-cli $CONF user add publisher --role genid-write --password "$(openssl rand -hex 16)"
#   m6-auth-cli $CONF token create publisher --name publish --ttl-days 90 > /etc/m6/genid-write.token
#   for node in edge-a edge-b edge-c edge-d edge-e; do
#       m6-auth-cli $CONF user add "reader-$node" --role genid-read --password "$(openssl rand -hex 16)"
#       m6-auth-cli $CONF token create "reader-$node" --name "read-$node" --ttl-days 90   # copy to /etc/m6/genid-read on $node only
#   done
#   chown root:root /etc/m6/genid-write.token && chmod 600 /etc/m6/genid-write.token
#
# Every token expires 90 days after it is minted, and from then on /genid answers
# 401 to it: the CMS can no longer publish and the cache node stops following.
# To rotate one before that, mint a new token for the same user the same way,
# replace its credential file (/etc/m6/genid-write.token here,
# /etc/m6/genid-read on the cache node), restart the unit that loads it
# (render-cms, or m6-http-cache on the cache node), then revoke the old token
# (`m6-auth-cli $CONF token ls <user>`, then `token revoke <id>`).

# System config — one instance handles both public TLS and H2C backbone
install -m 640 -o root -g m6 configs/origin.toml /etc/m6/origin.toml
sed -i "s/example.com/$DOMAIN/g" /etc/m6/origin.toml

echo "=== WireGuard ==="
install -m 600 wireguard/origin-wg0.conf /etc/wireguard/wg0.conf
systemctl enable --now wg-quick@wg0

echo "=== Firewall ==="
ufw allow 22/tcp
# The backbone: h2c on the WireGuard address is reachable through wg0 only.
# `insert 1` puts these rules before any `allow 80/tcp` already in the chain.
# The deny goes in first so the wg0 allow lands above it.
ufw insert 1 deny to 10.0.0.1 port 80 proto tcp
ufw insert 1 allow in on wg0 to 10.0.0.1 port 80 proto tcp
# Certbot standalone answers the certificate HTTP-01 challenge on public port 80 at issue and renewal.
ufw allow 80/tcp
ufw allow 443/tcp
ufw allow 443/udp    # H3/QUIC
ufw allow 51820/udp  # WireGuard
ufw --force enable

echo "=== TLS certificate ==="
certbot certonly --standalone --agree-tos --email "$EMAIL" -d "$DOMAIN"
# Auto-renewal hook. systemd copies the new certificate into the unit when it starts, so this restart serves it.
mkdir -p /etc/letsencrypt/renewal-hooks/deploy
cat > /etc/letsencrypt/renewal-hooks/deploy/m6-http-restart.sh << 'HOOK'
#!/bin/bash
systemctl restart m6-http-origin
HOOK
chmod +x /etc/letsencrypt/renewal-hooks/deploy/m6-http-restart.sh

echo "=== Installing systemd units ==="
sed "s/example.com/$DOMAIN/g" systemd/m6-http-origin.service > /etc/systemd/system/m6-http-origin.service
chmod 644 /etc/systemd/system/m6-http-origin.service
# (Install m6-html, m6-file, m6-auth, render-cms units from example 06/07)
systemctl daemon-reload

echo ""
echo "Done. Next steps:"
echo "  1. Copy WireGuard keys into /etc/wireguard/wg0.conf (replace placeholders)"
echo "  2. Deploy site content: ./deploy.sh user@$DOMAIN"
echo "  3. Start services:"
echo "       systemctl enable --now m6-html m6-file m6-auth render-cms"
echo "       systemctl enable --now m6-http-origin"
