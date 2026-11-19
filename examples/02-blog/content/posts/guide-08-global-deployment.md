+++
title   = "Guide 08: Global Deployment"
date    = "2025-01-01"
summary = "Six nodes, five continents, one origin. A WireGuard hub-and-spoke backbone carries H2C traffic from cache nodes to the origin. GeoDNS routes each user to the nearest edge."
tags    = ["guide", "tutorial", "deployment", "global", "wireguard", "cdn"]
cover   = "https://picsum.photos/seed/guide08/1200/500"
+++

**Guide series:**
[Prerequisites](/blog/guide-00-prerequisites) →
[Example 01](/blog/guide-01-static-site) →
[Example 02](/blog/guide-02-blog) →
[Example 03](/blog/guide-03-contact-form) →
[Example 04](/blog/guide-04-auth) →
[Example 05](/blog/guide-05-cms) →
[Example 06](/blog/guide-06-systemd) →
[Example 07](/blog/guide-07-dev-to-production) →
**Example 08: Global Deployment**

---

`examples/09-global-deployment/` shows how to spread a single m6 site across six nodes worldwide using WireGuard as an encrypted backbone and GeoDNS to route each user to their nearest edge.

## Architecture

<pre class="mermaid">
graph TD
    DNS["GeoDNS\n(ClouDNS)"] -->|Oceania| ORIGIN
    DNS -->|Americas West| EDGEA
    DNS -->|Americas East| EDGEB
    DNS -->|Americas Central| EDGEC
    DNS -->|Europe| EDGED
    DNS -->|Asia| EDGEE

    subgraph "Origin"
        ORIGIN["m6-http\nbind=0.0.0.0:443 TLS\nh2c_bind=10.0.0.1:80"]
        BACKENDS["m6-html · m6-file\nm6-auth · render-cms"]
        ORIGIN --> BACKENDS
    end

    subgraph "WireGuard 10.0.0.0/24"
        EDGEA["edge-a\n10.0.0.2"]
        EDGEB["edge-b\n10.0.0.3"]
        EDGEC["edge-c\n10.0.0.4"]
        EDGED["edge-d\n10.0.0.5"]
        EDGEE["edge-e\n10.0.0.6"]
    end

    EDGEA -->|H2C| ORIGIN
    EDGEB -->|H2C| ORIGIN
    EDGEC -->|H2C| ORIGIN
    EDGED -->|H2C| ORIGIN
    EDGEE -->|H2C| ORIGIN
</pre>

One **origin** runs all backend services. Five **cache nodes** (edge-a, edge-b, edge-c, edge-d, edge-e) each run a single `m6-http` instance that proxies everything upstream.

The cache nodes are pure HTTP proxies. They hold no content, no auth database, no CMS. All writes go through the origin. Reads are served locally when cached.

## Why H2C over WireGuard?

The cache nodes talk to the origin over **H2C** (HTTP/2 cleartext) on WireGuard. WireGuard already encrypts the tunnel end-to-end, so adding TLS on top would mean:

- Double encryption, which wastes CPU on both ends
- Certificate management for internal-only endpoints
- No benefit, because WireGuard peers are authenticated by public key

H2C on WireGuard gives you HTTP/2 multiplexing (multiple in-flight requests per connection, header compression) with WireGuard's encryption and zero-round-trip reconnect. It is faster and simpler than an internal TLS setup.

## Node roles

### Origin

One m6-http instance handles both the public edge and the H2C backbone via two fields in the system config:

**`configs/origin.toml`:**

```toml
[server]
bind     = "0.0.0.0:443"
tls_cert = "/run/credentials/m6-http-origin.service/tls-cert.pem"
tls_key  = "/run/credentials/m6-http-origin.service/tls-key.pem"
h2c_bind = "10.0.0.1:80"
internal_binds = ["10.0.0.1:80"]
internal_peers = ["10.0.0.2", "10.0.0.3", "10.0.0.4", "10.0.0.5", "10.0.0.6"]

[auth]
backend = "m6-auth-check"

[genid]
read_require  = "role:genid-read"
write_require = "role:genid-write"

[node]
name = "origin"
```

- `bind`: public HTTPS on all interfaces, served to end users and Oceania GeoDNS traffic
- `h2c_bind`: cleartext HTTP/2 on the WireGuard interface, only reachable from cache nodes
- `internal_binds`: declares the h2c listener internal, so m6-http trusts the client address a cache node forwards. m6-http refuses to start with `h2c_bind` set and no `internal_binds`.
- `internal_peers`: the addresses the cache nodes connect from. m6-http closes a connection from any other address, so trust does not depend on the firewall alone.
- `[auth]`: the check backend m6-http asks who a caller is belongs to this node, so it lives in its system config.
- `[genid]`: the origin is the source of the site's generation ID. A publish is one `POST /genid` to it. It needs `StateDirectory=` in its unit, and a cache node follows the ID on its heartbeat.

Both listeners share the same site config and backend Unix sockets in `/run/m6/*.sock`.

### Cache nodes

Each cache node runs one m6-http instance with:

**`configs/cache-<node>.toml`** for the TLS bind and the node-specific cert path:

```toml
# edge-d example
[server]
bind     = "0.0.0.0:443"
tls_cert = "/run/credentials/m6-http-cache.service/tls-cert.pem"
tls_key  = "/run/credentials/m6-http-cache.service/tls-key.pem"

[node]
name = "edge-d"
```

**`configs/cache-site.toml`** is identical on every cache node:

```toml
[[backend]]
name = "origin"
url  = "h2c://10.0.0.1:80"

[[route]]
path    = "/"
backend = "origin"

# ... full route table mirrors origin-site.toml ...

# CMS and auth routes always bypass cache
[[route]]
path      = "/cms"
backend   = "origin"
require   = "group:editors"
cache     = "no-store"
```

The `cache = "no-store"` on CMS and auth routes ensures writers always reach the origin. Every other route can be served from the cache node's in-memory HTTP cache.

## WireGuard hub-and-spoke

The origin is the WireGuard hub. Each cache node is a spoke.

**`wireguard/origin-wg0.conf`:**

```ini
[Interface]
Address    = 10.0.0.1/24
ListenPort = 51820
PrivateKey = <origin-private-key>

[Peer]  # edge-a
PublicKey  = <edge-a-public-key>
AllowedIPs = 10.0.0.2/32

[Peer]  # edge-b
PublicKey  = <edge-b-public-key>
AllowedIPs = 10.0.0.3/32

# ... edge-d (10.0.0.5), edge-e (10.0.0.6), edge-c (10.0.0.4)
```

**`wireguard/edge-a-wg0.conf`** (spoke):

```ini
[Interface]
Address    = 10.0.0.2/24
PrivateKey = <edge-a-private-key>

[Peer]  # origin
PublicKey           = <origin-public-key>
Endpoint            = example.com:51820
AllowedIPs          = 10.0.0.1/32
PersistentKeepalive = 25
```

`PersistentKeepalive = 25` keeps the NAT mapping alive on VPS providers that drop idle UDP sessions.

## systemd units

Two units on the origin, one on each cache node:

**Origin (one unit for both listeners):**

```ini
# m6-http-origin.service
[Service]
ExecStart=/usr/local/bin/m6-http \
    /var/www/my-blog \
    /etc/m6/origin.toml
```

`origin.toml` has both `bind` (TLS) and `h2c_bind` (WireGuard backbone), one process with two listening sockets.

**Cache nodes (one unit, parameterised via EnvironmentFile):**

```ini
# m6-http-cache.service
[Service]
EnvironmentFile=/etc/m6/node-config
ExecStart=/usr/local/bin/m6-http \
    /var/www/my-blog \
    ${NODE_CONFIG}
```

`/etc/m6/node-config` on each cache node contains just:

```bash
NODE_CONFIG=/etc/m6/cache.toml
```

The same `.service` file deploys identically to all five edges. Only the environment file differs per node.

## TLS certificates

Cache nodes need per-node certificates for their own hostname (`edge-a.example.com`, `edge-d.example.com`, and so on). Certbot DNS challenge is the easiest path for wildcard certs:

```bash
# One wildcard covers all nodes
certbot certonly --dns-cloudflare \
    -d "*.example.com" \
    --dns-cloudflare-credentials /etc/m6/cloudflare.ini
```

Or use individual per-node certs with standalone certbot on each node.

The deploy hook restarts m6-http, and systemd copies the new certificate into the unit when it starts:

```bash
# /etc/letsencrypt/renewal-hooks/deploy/m6-http-restart.sh
systemctl restart m6-http-origin
```

On a cache node the hook restarts `m6-http-cache`.

## GeoDNS with ClouDNS

ClouDNS GeoDNS routes each user to their nearest node by returning a different A record per geographic region:

| Region | Hostname resolves to |
|---|---|
| Oceania + default | example.com |
| Americas West | edge-a.example.com |
| Americas East | edge-b.example.com |
| Americas Central | edge-c.example.com |
| Europe | edge-d.example.com |
| Asia | edge-e.example.com |

The actual A records point to each server's public IP. GeoDNS is purely a routing layer. It has no knowledge of m6 or your content.

All six names (`example.com`, `edge-a.example.com`, and so on) are A records that ClouDNS serves conditionally based on the resolver's location.

## First-time setup

**Origin:**

```bash
./setup-origin.sh example.com admin@example.com
```

The script installs WireGuard, certbot, and ufw, generates auth keys in `/etc/m6/`, patches the domain into the system config, and installs systemd units.

**Each cache node:**

```bash
# On the cache node
./setup-cache-node.sh edge-a edge-a.example.com wireguard/edge-a-wg0.conf
```

Then sync the WireGuard keys (generate with `wg genkey | tee privkey | wg pubkey > pubkey` on each node, patch into the `.conf` files, then `wg set wg0 peer <pubkey> ...` or restart `wg-quick`).

## Deploy

```bash
# From your dev machine, push content to the origin
./deploy.sh user@example.com

# From the origin, push static assets to cache nodes
# (cache nodes hold no server-side content, but having templates
#  locally avoids a round-trip for error pages)
rsync -av --delete \
  --exclude 'content/drafts/' --exclude 'keys/' \
  /var/www/my-blog/ root@edge-a.example.com:/var/www/my-blog/
```

Because cache nodes proxy everything to the origin, you only need to keep static assets in sync. Blog content is always fetched live from the origin.

## Local demo

Run the entire 6-node topology on one machine using loopback addresses:

```bash
cd examples/09-global-deployment
./dev.sh
```

This starts:

| Instance | Address | Role |
|---|---|---|
| Origin | https://127.0.0.1:9001 | Public TLS for direct access + CMS |
| Origin H2C backbone | http://127.0.0.1:9000 | Cache-node backbone (internal) |
| Cache edge-a | https://127.0.0.1:9002 | edge-a proxy |
| Cache edge-b | https://127.0.0.1:9003 | edge-b proxy |
| Cache edge-c | https://127.0.0.1:9004 | edge-c proxy |
| Cache edge-d | https://127.0.0.1:9005 | edge-d proxy |
| Cache edge-e | https://127.0.0.1:9006 | edge-e proxy |

One m6-http process on the origin handles both listeners (`bind` for TLS, `h2c_bind` for H2C). All five cache instances proxy back to `127.0.0.1:9000` over plain H2C (mirroring production WireGuard). You can hit any of the five TLS ports and get the same blog served from the same origin.

TLS uses a `mkcert` self-signed cert generated on first run. The CMS is available at `https://127.0.0.1:9001/cms` (login: admin / admin).

## Cost

Six entry-level VPS nodes (~$6/month each on most providers) gives you global reach for ~$36/month total. There is no CDN contract, no origin shield fee, no per-GB egress pricing beyond what the VPS provider charges (typically 1–2 TB/month included). The WireGuard backbone traffic between nodes is minimal. It is only cache misses and CMS writes.

---

**← [Example 07: Dev to Production](/blog/guide-07-dev-to-production)**
