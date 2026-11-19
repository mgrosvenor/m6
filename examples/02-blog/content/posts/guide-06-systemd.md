+++
title   = "Guide 06: Production with systemd"
date    = "2025-01-02"
summary = "Run every m6 process as a systemd unit with proper ordering, restart policy, log capture, security hardening, and horizontal scaling."
tags    = ["guide", "tutorial", "systemd", "production", "deployment"]
cover   = "https://picsum.photos/seed/guide06/1200/500"
+++

**Guide series:**
[Prerequisites](/blog/guide-00-prerequisites) →
[Example 01](/blog/guide-01-static-site) →
[Example 02](/blog/guide-02-blog) →
[Example 03](/blog/guide-03-contact-form) →
[Example 04](/blog/guide-04-auth) →
[Example 05](/blog/guide-05-cms) →
**Example 06 — systemd** →
[Example 07](/blog/guide-07-dev-to-production)

---

`examples/06-systemd/` takes the CMS blog from example 05 and runs it properly under systemd. Every process is a unit. Ordering, restart policy, log capture, socket directory permissions, deploy workflow, and horizontal scaling are all covered.

## Server preparation

```bash
# Dedicated user — no login shell, no home directory
useradd --system --no-create-home --shell /usr/sbin/nologin m6

# Site directory — owned by m6 user
mkdir -p /var/www/my-blog
chown m6:m6 /var/www/my-blog

# Auth keys, kept root-owned because systemd reads them as a credential
mkdir -p /etc/m6
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -pkeyopt ec_param_enc:named_curve -out /etc/m6/auth.pem
openssl pkey -in /etc/m6/auth.pem -pubout -out /etc/m6/auth.pub
chmod 600 /etc/m6/auth.pem
chmod 600 /etc/m6/auth.pub

# TLS certificate via Let's Encrypt
certbot certonly --standalone -d example.com
# Outputs /etc/letsencrypt/live/example.com/fullchain.pem
#         /etc/letsencrypt/live/example.com/privkey.pem
# The m6-http unit delivers both through LoadCredential

# System config — this node's own state, passed to m6-http as its second argument
cat > /etc/m6/my-blog.toml << 'TOML'
[server]
bind     = "0.0.0.0:443"
tls_cert = "/run/credentials/m6-http.service/tls-cert.pem"
tls_key  = "/run/credentials/m6-http.service/tls-key.pem"

[node]
name = "origin"

[auth]
backend = "m6-auth-check"

[genid]
read_require  = "role:genid-read"
write_require = "role:genid-write"
TOML
chown root:m6 /etc/m6/my-blog.toml
chmod 640 /etc/m6/my-blog.toml
```

systemd copies the certificate and key into m6-http's credential directory when the unit starts, so the hook restarts m6-http after each renewal:

```bash
# /etc/letsencrypt/renewal-hooks/deploy/m6-http-restart.sh, mode 0755
#!/bin/sh
systemctl restart m6-http
```

Until it runs, m6-http keeps serving the previous certificate.

## Deploy the site directory

```bash
# From your development machine
rsync -av --delete \
  --exclude 'content/drafts/' \
  --exclude 'keys/' \
  examples/05-cms/ \
  user@example.com:/var/www/my-blog/

# On the server, fix ownership after rsync
chown -R m6:m6 /var/www/my-blog
```

`content/posts/` (published posts) and `keys/` are excluded — these live exclusively on the server.

## Unit files

<pre class="mermaid">
flowchart TD
    net["network.target"]
    net --> mhtml["m6-html.service"]
    net --> mfile["m6-file.service"]
    net --> mauth["m6-auth.service"]
    net --> rcms["render-cms.service"]
    mhtml --> mhttp["m6-http.service"]
    mfile --> mhttp
    mauth --> mhttp
    rcms --> mhttp
</pre>

One unit per process. Renderers follow the same pattern; only `ExecStart` and `Description` differ.

### `/etc/systemd/system/m6-html.service`

```ini
[Unit]
Description=m6-html renderer
Documentation=https://github.com/mgrosvenor/m6
After=network.target

[Service]
Type=simple
User=m6
Group=m6

RuntimeDirectory=m6
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes

ExecStart=/usr/local/bin/m6-html \
    /var/www/my-blog \
    /var/www/my-blog/configs/m6-html.conf

Restart=on-failure
RestartSec=2

StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-html

NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

### `/etc/systemd/system/m6-file.service`

```ini
[Unit]
Description=m6-file renderer
After=network.target

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes

ExecStart=/usr/local/bin/m6-file \
    /var/www/my-blog \
    /var/www/my-blog/configs/m6-file.conf

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-file
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6
ReadOnlyPaths=/var/www/my-blog

[Install]
WantedBy=multi-user.target
```

### `/etc/systemd/system/m6-auth.service`

```ini
[Unit]
Description=m6-auth service
After=network.target

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes

ExecStart=/usr/local/bin/m6-auth-server \
    /var/www/my-blog \
    /etc/m6/m6-auth.conf

LoadCredential=auth.pem:/etc/m6/auth.pem
LoadCredential=auth.pub:/etc/m6/auth.pub
StateDirectory=m6-auth
StateDirectoryMode=0700

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-auth
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

m6-auth's keys come from the two `LoadCredential` lines, and its database lives in `/var/lib/m6-auth`, which `StateDirectory=m6-auth` creates.

### `/etc/systemd/system/render-cms.service`

```ini
[Unit]
Description=render-cms custom renderer
After=network.target

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes

ExecStart=/var/www/my-blog/bin/render-cms \
    /var/www/my-blog \
    /etc/m6/render-cms.conf

# The token that lets render-cms publish with POST /genid. It arrives as
# /run/credentials/render-cms.service/genid-write.token
LoadCredential=genid-write.token:/etc/m6/genid-write.token

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=render-cms
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6 /var/www/my-blog/content
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

The `render-cms` binary lives in `/var/www/my-blog/bin/` — deployed alongside the site, not installed system-wide.

### `/etc/systemd/system/m6-http.service`

```ini
[Unit]
Description=m6-http reverse proxy and cache
Documentation=https://github.com/mgrosvenor/m6
After=network.target m6-html.service m6-file.service m6-auth.service render-cms.service

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes

WorkingDirectory=/var/www/my-blog

ExecStart=/usr/local/bin/m6-http /var/www/my-blog /etc/m6/my-blog.toml

LoadCredential=tls-cert.pem:/etc/letsencrypt/live/example.com/fullchain.pem
LoadCredential=tls-key.pem:/etc/letsencrypt/live/example.com/privkey.pem

# The source node writes the site's generation ID here
StateDirectory=m6-http
StateDirectoryMode=0700

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-http

AmbientCapabilities=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6 /var/www/my-blog/logs
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

`After=` orders the start: m6-http expands each pool's socket glob once, at start, and a glob that matches no socket stops it with exit 2, so the backends must have bound their sockets first. m6-auth-server binds two sockets, `m6-auth.sock` for login and `check-m6-auth.sock` for the token check m6-http asks on every protected route and on `/genid`.

`StateDirectory=m6-http` gives m6-http `/var/lib/m6-http`, where the source node writes the site's generation ID, because `ProtectSystem=strict` makes the rest of the disk read-only.

The `render-cms` unit delivers the publisher token with `LoadCredential`, as `/run/credentials/render-cms.service/genid-write.token`, and starts from `/etc/m6/render-cms.conf`. The site's `configs/render-cms.conf` is the development config: its `[genid]` table names the development token and CA. Build the production copy from it, keeping the routes and replacing only that table, which is the last one in the file:

```bash
sed '/^\[genid\]/,$d' /var/www/my-blog/configs/render-cms.conf > /etc/m6/render-cms.conf
cat >> /etc/m6/render-cms.conf << 'CONF'
[genid]
url        = "https://example.com/genid"
connect    = "127.0.0.1:443"
token_file = "/run/credentials/render-cms.service/genid-write.token"
timeout_ms = 10000
CONF
chown root:m6 /etc/m6/render-cms.conf
chmod 640 /etc/m6/render-cms.conf
```

Production leaves `ca_file` out, so the system's certificate store is trusted.

Mint the publisher token once the auth database exists, after the first start of `m6-auth`:

```bash
m6-auth-cli /etc/m6/m6-auth.conf user add publisher --role genid-write --password "$(openssl rand -hex 16)"
m6-auth-cli /etc/m6/m6-auth.conf token create publisher --name publish --ttl-days 90 > /etc/m6/genid-write.token
chmod 600 /etc/m6/genid-write.token
```

Without `--ttl-days` a token lasts 365 days. This one expires 90 days after it is minted. From then on `/genid` answers 401 to it, and the CMS can no longer publish. To rotate it before that, mint a new token the same way into a new file, move it over `/etc/m6/genid-write.token`, run `systemctl restart render-cms` (systemd copies the credential in when the unit starts), then revoke the old one: `m6-auth-cli /etc/m6/m6-auth.conf token ls publisher` lists the tokens with their ids, and `token revoke <id>` ends one. A cache node follows the site with a reader token of its own, one user per node (`reader-edge-a`, and so on, with the `genid-read` role), minted the same way and rotated the same way with `systemctl restart m6-http-cache`. See example 09's `setup-origin.sh`.

## Install and start

```bash
cp examples/06-systemd/systemd/*.service /etc/systemd/system/

systemctl daemon-reload

systemctl enable m6-html m6-file m6-auth render-cms m6-http
systemctl start  m6-html m6-file m6-auth render-cms m6-http

systemctl status m6-html m6-file m6-auth render-cms m6-http
```

## Verify

```bash
# Check m6-http is listening
ss -tlnp | grep :443

# Check all sockets appeared
ls -la /run/m6/
# Expected:
# /run/m6/m6-html.sock
# /run/m6/m6-file.sock
# /run/m6/m6-auth.sock
# /run/m6/render-cms.sock

# Smoke test
curl -s -o /dev/null -w "%{http_code}" https://example.com/
# 200
curl -s -o /dev/null -w "%{http_code}" https://example.com/cms
# 302 (redirect to /login)
```

## Logs

```bash
# Follow all m6 processes together
journalctl -f -u m6-http -u m6-html -u m6-file -u m6-auth -u render-cms

# Structured log processing
journalctl -u m6-http -o cat | jq .

# Errors only
journalctl -u m6-http -p err

# Between timestamps
journalctl -u m6-http --since "2024-01-15 09:00" --until "2024-01-15 10:00"
```

## Deploy updates

```bash
# Template, data or content changes — rsync, then publish
# m6 reads its files at start, so the site is told to read them again with one
# POST /genid (loopback, with the publisher token); a site.toml edit needs a restart
rsync -av --delete \
  --exclude 'content/drafts/' \
  --exclude 'keys/' \
  examples/05-cms/ user@example.com:/var/www/my-blog/

# Binary changed — deploy and restart just that unit
cargo build --release -p render-cms
rsync target/release/render-cms user@example.com:/var/www/my-blog/bin/
ssh user@example.com systemctl restart render-cms
# m6-http connects to the socket again on its next turn — other units unaffected

# TLS cert renewal — certbot's deploy hook restarts m6-http
certbot renew
```

## Horizontal scaling

Add a second m6-html instance with no change to the site config:

```bash
# Create a second config (content identical to m6-html.conf)
cp /var/www/my-blog/configs/m6-html.conf \
   /var/www/my-blog/configs/m6-html-2.conf

# Copy and edit the unit
cp /etc/systemd/system/m6-html.service \
   /etc/systemd/system/m6-html-2.service
# Edit ExecStart to reference m6-html-2.conf, update Description and SyslogIdentifier

systemctl daemon-reload
systemctl enable m6-html-2
systemctl start  m6-html-2

# /run/m6/m6-html-2.sock appears
# m6-http expands the pool's glob at start, so restart it to add the member
systemctl restart m6-http
# Traffic now goes to both instances in turn (round robin)
```

Scale back down:

```bash
systemctl stop m6-html-2
# A request that finds the socket gone tries the next member, so one refused
# connect is the whole cost until the next m6-http restart drops it
```

## Crash recovery

`Restart=on-failure` with `RestartSec=2` handles crashes automatically. During the 2-second restart window a request to that pool tries the next member, and the client gets a 502 only when every member has failed. A backend that restarts on the same socket path serves again on its next turn.

```bash
# Simulate a crash
systemctl kill -s KILL m6-html

# Watch systemd restart it
journalctl -u m6-html -f
```

---

**← [Example 05 — CMS Blog](/blog/guide-05-cms)** | **[Example 07 — Dev to Production](/blog/guide-07-dev-to-production) →**
