+++
title   = "Guide 07: Dev to Production"
date    = "2025-01-01"
summary = "The same site.toml in both environments. The system config holds the node state that differs. A deploy.sh script and a common mistakes checklist."
tags    = ["guide", "tutorial", "deployment", "production"]
cover   = "https://picsum.photos/seed/guide07/1200/500"
+++

**Guide series:**
[Prerequisites](/blog/guide-00-prerequisites) →
[Example 01](/blog/guide-01-static-site) →
[Example 02](/blog/guide-02-blog) →
[Example 03](/blog/guide-03-contact-form) →
[Example 04](/blog/guide-04-auth) →
[Example 05](/blog/guide-05-cms) →
[Example 06](/blog/guide-06-systemd) →
**Example 07 — Dev to Production** →
[Example 08](/blog/guide-08-global-deployment)

---

`examples/07-dev-to-production/` is the complete workflow for taking a site from local development to a production server. The same site directory runs in both environments — the system config holds the node state that differs.

## The split

**`site.toml` — version controlled, deployed with the site, no secrets:**

```toml
[site]
name   = "My Blog"
domain = "example.com"

[log]
level  = "info"

[errors]
mode = "internal"

# All backends and routes — identical in dev and production
[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html-*.sock"
# ... full backend and route table as in example 05
```

`site.toml` has no bind address, no certificate paths and no `[auth]`. Those describe one node, so they live in the system config. m6-http logs a warning at start for each node key it finds in `site.toml`.

**`configs/system-dev.toml` — development system config, version controlled:**

```toml
# The node state in development.
# Committed to the repo — contains no secrets. @M6_DEV_HOME@ stands for the
# per-user directory the dev scripts create (dev-home.sh); dev.sh copies this
# file with the real path filled in.
[server]
bind     = "127.0.0.1:8443"
tls_cert = "@M6_DEV_HOME@/dev/dev.pem"
tls_key  = "@M6_DEV_HOME@/dev/dev-key.pem"

[auth]
backend = "m6-auth-check"

[genid]
read_require  = "role:genid-read"
write_require = "role:genid-write"

[node]
name = "dev"
```

**`/etc/m6/my-blog.toml` — production system config, on the server only:**

```toml
[node]
name = "web-1"

[server]
bind     = "0.0.0.0:443"
tls_cert = "/run/credentials/m6-http.service/tls-cert.pem"
tls_key  = "/run/credentials/m6-http.service/tls-key.pem"

[auth]
backend = "m6-auth-check"

[genid]
read_require  = "role:genid-read"
write_require = "role:genid-write"
```

The system config holds the state of one node: its identity (`[node]`), its binds and TLS paths (`[server]`), its check backend (`[auth]`) and the claims that gate `/genid` (`[genid]`). Each table goes in one file only, and a key in the file that does not own it stops m6-http at start with exit 2. Everything else comes from `site.toml` unchanged.

**`configs/render-contact.conf` — version controlled, uses local mock values:**

```toml
secrets_file = "/etc/m6/contact-secrets.toml"
# On dev machines /etc/m6/contact-secrets.toml doesn't exist
# — silently ignored, [smtp] values below are used.

global_params = ["data/site.json"]

[[route]]
path     = "/contact"
template = "templates/contact.html"
params   = []
cache    = "no-store"

[smtp]
host     = "localhost"
port     = 1025
username = ""
password = ""
from     = "dev@localhost"
to       = "dev@localhost"
```

**`/etc/m6/contact-secrets.toml` — on the server only:**

```toml
[smtp]
host     = "smtp.postmarkapp.com"
port     = 587
username = "live-api-token"
password = "live-api-token"
from     = "noreply@example.com"
to       = "owner@example.com"
```

When `secrets_file` points to a file that exists, it's merged in and wins on conflict. When absent (on dev machines), silently ignored. The `secrets_file` path itself is safe to commit.

## First-time server setup

```bash
# Create system user
useradd --system --no-create-home --shell /usr/sbin/nologin m6

# Create site directory
mkdir -p /var/www/my-blog
chown m6:m6 /var/www/my-blog

# Generate production auth keys
mkdir -p /etc/m6
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -pkeyopt ec_param_enc:named_curve -out /etc/m6/auth.pem
openssl pkey -in /etc/m6/auth.pem -pubout -out /etc/m6/auth.pub
chown m6:m6 /etc/m6/auth.pem /etc/m6/auth.pub
chmod 600 /etc/m6/auth.pem

# Create production system config
cat > /etc/m6/my-blog.toml << 'TOML'
[node]
name = "web-1"

[server]
bind     = "0.0.0.0:443"
tls_cert = "/run/credentials/m6-http.service/tls-cert.pem"
tls_key  = "/run/credentials/m6-http.service/tls-key.pem"

[auth]
backend = "m6-auth-check"

[genid]
read_require  = "role:genid-read"
write_require = "role:genid-write"
TOML
chown root:m6 /etc/m6/my-blog.toml
chmod 640 /etc/m6/my-blog.toml

# Create SMTP secrets
cat > /etc/m6/contact-secrets.toml << 'TOML'
[smtp]
host     = "smtp.postmarkapp.com"
port     = 587
username = "live-api-token"
password = "live-api-token"
from     = "noreply@example.com"
to       = "owner@example.com"
TOML
chown root:m6 /etc/m6/contact-secrets.toml
chmod 640 /etc/m6/contact-secrets.toml

# Obtain TLS certificate
certbot certonly --standalone -d example.com

# Install systemd units
cp examples/07-dev-to-production/systemd/*.service /etc/systemd/system/
systemctl daemon-reload

# Deploy site (see deploy.sh below), then:
systemctl enable m6-html m6-file m6-auth render-cms m6-http
systemctl start  m6-html m6-file m6-auth render-cms m6-http
```

## The `m6-http` unit for this example

The only difference from example 06 is `ExecStart` passes the production system config path explicitly:

```ini
[Service]
ExecStart=/usr/local/bin/m6-http \
    /var/www/my-blog \
    /etc/m6/my-blog.toml
```

Renderer units are identical to example 06 — they take only two positional args and use `secrets_file` for credentials.

## `deploy.sh`

```bash
#!/bin/bash
# Run from your development machine
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
# new content is read when the site's generation advances (POST /genid)
```

For template, data and content changes: `./deploy.sh`, then one `POST /genid` (the CMS sends it when an editor publishes), which reloads every backend and empties the cache. A `site.toml` or system config change takes a restart of m6-http. For renderer binary changes: `./deploy.sh --binary` — only render-cms restarts; all other units and the m6-http cache are unaffected.

## Dev vs production at a glance

| | Development | Production |
|---|---|---|
| `m6-http` invocation | `m6-http $SITE "$(m6_conf $SITE/configs/system-dev.toml)"` | `m6-http /var/www/my-blog /etc/m6/my-blog.toml` |
| `[server] bind` | `127.0.0.1:8443` | `0.0.0.0:443` |
| TLS cert | `@M6_DEV_HOME@/dev/dev.pem` | `/run/credentials/m6-http.service/tls-cert.pem` |
| SMTP | `localhost:1025` (mock) | `smtp.postmarkapp.com:587` |
| SMTP credentials | inline in renderer config | `/etc/m6/contact-secrets.toml` via `secrets_file` |
| `[auth] backend` | `m6-auth-check` | `m6-auth-check` |
| publisher token (`[genid] token_file` in the renderer config) | `@M6_DEV_HOME@/07-dev-to-production/genid-write.token` | `/run/credentials/render-cms.service/genid-write.token` |
| System config | `configs/system-dev.toml` (in repo) | `/etc/m6/my-blog.toml` (on server) |
| Process manager | `dev.sh` | systemd |
| Logs | terminal stdout | journald |

Routes, templates, backends and log settings are identical. The same `site.toml` runs in both environments.

<pre class="mermaid">
flowchart LR
    subgraph "Version Controlled"
        sitetoml["site.toml"]
        sysdev["system-dev.toml"]
        rcconf["render-contact.conf"]
    end
    subgraph "Server Only"
        prodsys["/etc/m6/my-blog.toml"]
        prodsec["/etc/m6/contact-secrets.toml"]
    end
    sitetoml --> HTTP["m6-http"]
    sysdev -. "dev" .-> HTTP
    prodsys -. "prod" .-> HTTP
    rcconf --> RC["render-contact"]
    prodsec -. "prod only" .-> RC
</pre>

## Checking what's in effect

```bash
# See the effective merged config — useful when debugging prod behaviour
m6-http /var/www/my-blog /etc/m6/my-blog.toml --dump-config

# Output shows [server] values from /etc/m6/my-blog.toml,
# everything else from site.toml:
#
# [server]
# bind     = "0.0.0.0:443"
# tls_cert = "/run/credentials/m6-http.service/tls-cert.pem"
# ...
```

## Common mistakes

**Forgetting the system config argument:**

```bash
m6-http /var/www/my-blog
# Exit 2 — second argument required in production
```

**Putting secrets in renderer config instead of `secrets_file`:**

```toml
# Wrong — committed to repo
[smtp]
password = "live-api-token"   # in configs/render-contact.conf

# Right — in /etc/m6/contact-secrets.toml, referenced by secrets_file
```

**Deploying over `data/posts.json` — wipes published posts:**

```bash
# Wrong
rsync -av ./ user@server:/var/www/my-blog/

# Right — always exclude server-managed state
rsync -av \
  --exclude 'content/drafts/' \
  --exclude 'keys/' \
  ./ user@server:/var/www/my-blog/
```

**Editing `site.toml` directly on the server:**

```bash
# Wrong — next deploy overwrites it
ssh server vim /var/www/my-blog/site.toml

# Right — edit locally and deploy
# Server-specific overrides belong in /etc/m6/my-blog.toml
```

---

That completes the guide. For the architecture deep-dives, see the [architecture overview](/blog/architecture), the [m6-http internals post](/blog/m6-http-deep-dive), and the [m6-html and m6-core post](/blog/m6-render-deep-dive).

---

**← [Example 06 — Production with systemd](/blog/guide-06-systemd)** | **[Example 08 — Global Deployment](/blog/guide-08-global-deployment) →**
