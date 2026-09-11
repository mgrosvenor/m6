# Hourly health check: the standing order

mgrosvenor.com runs on **three nodes, not one**. Every part of this check runs
on all three unless it says otherwise, and the report carries a row per node.

| node | role | service | WireGuard | serves |
|---|---|---|---|---|
| `syd` | origin | `m6-http-origin` | `10.0.0.1` | AU visitors directly, plus every cache miss from lon and chi |
| `lon` | cache | `m6-http-cache` | `10.0.0.4` | EU visitors |
| `chi` | cache | `m6-http-cache` | `10.0.0.5` | US visitors |

## Why this file exists

The check was run against `syd` alone for several hours running. That is not a
partial view, it is a **biased** one, and in two specific ways:

1. **A cache node serves most requests from its own cache, and those requests
   never reach origin.** Syd's analytics therefore records misses and direct-AU
   traffic only. Crawlers, bursts and probes that lon or chi answered are
   invisible there.
2. **Each node writes its own analytics, to a different path.** Origin uses
   `/var/www/dr-grosvenor-site/logs/analytics.ndjson`; the cache nodes use
   `/var/www/m6-cache/logs/analytics.ndjson`. `log_path` defaults to
   `logs/analytics.ndjson` relative to the unit's `WorkingDirectory`, which
   differs per role. Looking for origin's path on a cache node finds nothing
   and looks exactly like "analytics is not running there". It is running. Read
   all three files.

The perf check had the same shape of error. Warming seven pages and then
hammering them measures the warm-up, not production: it reported
`cache_hit_rate=1.0000` every hour while the real edge nodes were running at
15 to 19 percent over 24 hours.

**Rule: a number that only describes syd is not a fleet number, and a number
measured from traffic we generated is not a production number.** Say which it
is, every time.

## A. Logging is alive (first, everything else depends on it)

A config reload silences every m6-http log target except `analytics`, found
2026-09-06. Every deploy touches `site.toml`, so every deploy can do it. A
restart of the node's own service restores it.

```sh
# syd
ssh root@syd.mgrosvenor.com "journalctl -u m6-http-origin --since '20 minutes ago' --no-pager -o cat \
  | sed 's/\x1b\[[0-9;]*m//g' \
  | sed -n 's/.*\(INFO\|WARN\|ERROR\) *\([a-z_0-9:]*\):.*/\2/p' | sort | uniq -c"
# lon and chi, same but -u m6-http-cache
```

FAULT if `analytics` is the only target present, or if `m6_http_lib::stats` is
absent while requests are being served. Say **logging is blind on \<node\>**,
give the fix (`systemctl restart m6-http-origin` on syd, `m6-http-cache` on
lon/chi), and do not report "all clear" for that node.

## B. Performance

Report **per node**, and label each number as observed or generated.

- `cache_hit_rate` from a 10s `periodic stats` window is a **window**, not a
  lifetime. An idle window is all zeros and looks exactly like broken
  instrumentation; that is not the same fault as part A's "no lines at all".
- The fleet number that matters is the **24h hit rate per node**, computed from
  the journal rather than from traffic we just made:

```sh
journalctl -u <service> --since '24 hours ago' --no-pager -o cat | sed 's/\x1b\[[0-9;]*m//g' \
 | grep 'periodic stats' \
 | sed -E 's/.*cache_hits=([0-9]+) cache_misses=([0-9]+).*hit_p50_ns=([0-9]+) hit_p99_ns=([0-9]+).*/\1 \2 \3 \4/' \
 | awk '{h+=$1;m+=$2;if($1>0){n++;s50+=$3;s99+=$4}} END {printf "hits=%d misses=%d rate=%.4f p50=%.0f p99=%.0f\n",h,m,(h+m)?h/(h+m):0,n?s50/n:0,n?s99/n:0}'
```

- On-box loopback TTFB on each node:
  `curl -sk -o /dev/null -w 'ttfb=%{time_starttransfer} tls=%{time_appconnect}\n' -H 'Host: mgrosvenor.com' https://127.0.0.1/capabilities`
- Cache headers, once (they are origin's): `/capabilities` must be
  `public, max-age=60, s-maxage=86400, stale-while-revalidate=60`; a `?v=`
  asset `max-age=31536000, immutable`; `/contact` `no-store`.

### Baselines, and how much to trust them

| quantity | recorded baseline | status |
|---|---|---|
| hit p50 / p99 | 1.7-2.2us / 2.0-2.7us | **unverified.** Syd has measured a flat ~3.3us p50 across 293 windows in 24h with the box idle. Either the band was derived some other way or the drift predates the visible window. Re-derive before treating a miss as an incident. |
| steady-state hit rate | ~1.0000 | **wrong as a fleet target.** True only of a freshly warmed page hit repeatedly. Real 24h rates: syd 0.56, lon 0.16, chi 0.19. |
| loopback TTFB | ~4.3ms, ~3.8ms of it TLS | holds |

## C. Disk and logs

`df -h /`, `journalctl --disk-usage`, and the size of that node's analytics
file:

| node | analytics path |
|---|---|
| syd | `/var/www/dr-grosvenor-site/logs/analytics.ndjson` |
| lon, chi | `/var/www/m6-cache/logs/analytics.ndjson` |

Call out any node above 80%. Syd hit 82% on 2026-09-06 from build artefacts;
logrotate and a journald cap are installed.

## D. Security and traffic, last 60 minutes, per node

1. The node's own journal, `journalctl -k | grep -i ufw`, and **that node's
   own `analytics.ndjson`** (paths in part C).
2. Look for: bursts from one source, probe paths (`/wp-admin`, `/.env`,
   `/phpmyadmin`, `/.git`, `/admin`, `/xmlrpc.php`), repeated 401/403/404 from
   one source, SQLi/XSS-shaped queries, unusual UAs, ufw blocks, 5xx spikes,
   crash loops (`systemctl is-active` + `NRestarts`).
3. Services to check: syd `m6-http-origin`, `m6-html`, `m6-file`,
   `render-contact`, `render-analytics`, `m6-http-redirect`; lon/chi
   `m6-http-cache`, `m6-http-redirect`.
4. `render-contact` (syd only) for `rejected` / `message sent` / `SMTP`.

## E. Crawlers, every run, even a quiet one

Read the **full distinct user-agent list**, do not keyword-grep. A keyword list
has already missed CyberConvoyScout, host-probe, ForestEngine and
`libredtail-http`.

Named list to confirm against: Googlebot, Google-Extended, Bingbot, DuckDuckBot,
YandexBot, Baiduspider, Applebot; GPTBot, ChatGPT-User, OAI-SearchBot,
ClaudeBot, Claude-Web, Claude-User, anthropic-ai, PerplexityBot, CCBot,
Bytespider, Amazonbot, facebookexternalhit, Twitterbot, LinkedInBot, MJ12bot;
and anything self-identifying as bot/crawler/spider/scout/probe, anything that
is not a plausible browser string, and anything carrying an IP-literal
`Referer`.

Read **all three** analytics files, not origin's alone. A crawler that lon or
chi answered from cache never reaches origin and appears only in that node's
own file.

Report every sighting explicitly: name, **exact UA**, paths, timestamps, source
node.

## Known gaps, state them in the report rather than working around them

- **Relayed requests are attributed to the tunnel.** A request forwarded by a
  cache node logs `client_ip` as `10.0.0.4` or `10.0.0.5`, so its real source is
  not recoverable from syd. Fixed in the h2c forwarded-address work
  (`forward::ForwardedTrust`), not yet deployed.
- Firewall blocks are per-IP only. No CIDR rules.

## Reading logs

journald output carries ANSI colour codes. Strip them before any grep or
pattern match, or it silently matches nothing:

```sh
sed 's/\x1b\[[0-9;]*m//g'
```

## Killing leftover processes

Kill by **PID**. Never `pkill -f <pattern>` with a pattern naming a port or a
config path: over ssh the command line contains that string too, so pkill
matches its own session and kills the connection. That has happened three times
in this project.
