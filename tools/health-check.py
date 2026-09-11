#!/usr/bin/env python3
"""The hourly health check, as a program instead of a procedure.

`docs/health-check.md` is the standing order. Running it by hand meant a
dozen ssh round trips per run, re-deriving the same facts every hour, and
re-learning the same traps every time someone new ran it. This encodes the
traps so they cannot be re-learned:

  - **Three nodes, not one.** A cache node answers most requests from its own
    cache and those never reach origin, so a syd-only reading is biased, not
    partial. Every part runs on all three.
  - **Analytics lives at a different path per role.** Origin writes to
    /var/www/dr-grosvenor-site/logs/, the cache nodes to /var/www/m6-cache/logs/.
    Looking for origin's path on a cache node finds nothing and looks exactly
    like "analytics is off there".
  - **The record shape is nested.** It is `timestamp` and `fields.user_agent`,
    not `ts` and `user_agent`. Getting this wrong yields an empty result that
    looks like a quiet hour.
  - **A `periodic stats` line is a 10 second window.** An idle window is all
    zeros and looks exactly like broken instrumentation. That is a different
    fault from part A's "no lines at all", and the two are reported
    differently.
  - **A number measured from traffic we generated is not a production
    number.** Generated figures are labelled GENERATED and never mixed into
    the 24h observed table.
  - **Bot user agents are forged.** On 2026-09-11 a single IP sent 890
    requests rotating 526 user agents, 886 of them bot-shaped: ClaudeBot,
    GPTBot, Applebot, PerplexityBot and a dozen more, one request each. A
    keyword grep would have reported fifteen AI crawlers that never came.
    Any IP presenting many distinct bot UAs is reported as forgery, and
    excluded from the genuine crawler list.
  - **journald output carries ANSI colour codes.** Strip them or greps
    silently match nothing.

Read-only. It runs `journalctl`, `systemctl show`, `df`, `ls` and a loopback
`curl` on each node, and nothing else. It never writes to a node and never
touches the firewall.

Usage:
    tools/health-check.py                  # observed only, no traffic generated
    tools/health-check.py --load           # also generate load and read a loaded window
    tools/health-check.py --minutes 120    # security/crawler window (default 60)
    tools/health-check.py --json           # machine-readable, for trend tracking
"""

import argparse
import concurrent.futures
import json
import re
import subprocess
import sys
import time

# ── Fleet ────────────────────────────────────────────────────────────────────
# role decides the service name and the analytics path, which is the thing
# that has been got wrong before.
NODES = [
    {"name": "syd", "host": "syd.mgrosvenor.com", "role": "origin",
     "service": "m6-http-origin",
     "analytics": "/var/www/dr-grosvenor-site/logs/analytics.ndjson",
     "units": ["m6-http-origin", "m6-html", "m6-file", "render-contact",
               "render-analytics", "m6-http-redirect"]},
    {"name": "lon", "host": "lon.mgrosvenor.com", "role": "cache",
     "service": "m6-http-cache",
     "analytics": "/var/www/m6-cache/logs/analytics.ndjson",
     "units": ["m6-http-cache", "m6-http-redirect"]},
    {"name": "chi", "host": "chi.mgrosvenor.com", "role": "cache",
     "service": "m6-http-cache",
     "analytics": "/var/www/m6-cache/logs/analytics.ndjson",
     "units": ["m6-http-cache", "m6-http-redirect"]},
]

WARM_PATHS = ["/", "/capabilities", "/projects", "/publications",
              "/experience", "/gallery", "/videos"]

# ── Baselines ────────────────────────────────────────────────────────────────
# hit_p50 is deliberately absent. The recorded 1.7-2.2us band has never been
# reproduced: syd has measured a flat ~3.3us across hundreds of windows on an
# idle box, three separate runs. Until it is re-derived, asserting a band here
# would manufacture an hourly regression alert for a baseline nobody trusts.
# See HANDOVER.md section 6.
DISK_WARN_PCT = 80
TTFB_WARN_S = 0.010
HIT_RATE_FLOOR = 0.10      # per node, 24h. Below this the edge lifetime regressed.

BOT_RE = re.compile(
    r"bot\b|crawl|spider|scout|probe|slurp|fetcher|Amzn-|GPTBot|ClaudeBot|"
    r"Claude-User|Claude-Web|anthropic|Perplexity|Applebot|Bytespider|CCBot|"
    r"facebookexternalhit|Twitterbot|LinkedInBot|MJ12|Yandex|Baidu|DuckDuck|"
    r"Googlebot|Google-Extended|bingbot|Amazonbot|WhatsApp|Discord|Slackbot|"
    r"Grok|OAI-SearchBot|ChatGPT-User",
    re.I)

PROBE_RE = re.compile(
    r"\.env|wp-admin|wp-login|phpmyadmin|/\.git|xmlrpc\.php|/admin\b|/actuator|"
    r"credentials|passwd|/@fs|\.\./|/fetch\b|/proxy\b|/webhook|/api/fetch|"
    r"/redirect\b|\.aws|\.azure|gcloud|/shell|/cgi-bin|/console",
    re.I)

INJECTION_RE = re.compile(
    r"union\s+select|or\s+1=1|<script|javascript:|%3cscript|\bexec\b|"
    r"/etc/passwd|base64_decode|concat\(|sleep\(|benchmark\(",
    re.I)

# ── Remote probe ─────────────────────────────────────────────────────────────
# Sent over ssh stdin to `python3 -`, so nothing has to be installed on a node
# and a node can never drift from the version in this repo. It prints one JSON
# object. Aggregation happens on the node so a 46MB analytics file does not
# cross the network every hour.
REMOTE_PROBE = r'''
import collections, json, re, subprocess, sys

minutes  = int(sys.argv[1])
service  = sys.argv[2]
apath    = sys.argv[3]
units    = sys.argv[4].split(",")

ANSI = re.compile(r"\x1b\[[0-9;]*m")

def sh(cmd, timeout=120):
    try:
        p = subprocess.run(["bash", "-lc", cmd], capture_output=True,
                           text=True, timeout=timeout)
        return ANSI.sub("", p.stdout)
    except Exception as e:
        return ""

out = {}

# ── A. is logging alive ──────────────────────────────────────────────────────
# Target histogram over the last 20 minutes. `analytics` alone, or `stats`
# missing while requests are being served, means a config reload silenced
# every target but analytics.
targets = collections.Counter()
raw = sh("journalctl -u %s --since '20 minutes ago' --no-pager -o cat" % service)
for line in raw.splitlines():
    m = re.search(r"(INFO|WARN|ERROR) *([a-z_0-9:]+):", line)
    if m:
        targets[m.group(2)] += 1
out["log_targets"] = dict(targets)

# ── B. performance ───────────────────────────────────────────────────────────
STATS = re.compile(
    r"cache_hits=(\d+) cache_misses=(\d+).*?hit_p50_ns=(\d+) hit_p99_ns=(\d+)")
REQS = re.compile(r"requests=(\d+)")

def windows(since):
    rows = []
    txt = sh("journalctl -u %s --since '%s' --no-pager -o cat | grep 'periodic stats'"
             % (service, since), timeout=180)
    for line in txt.splitlines():
        m = STATS.search(line)
        if not m:
            continue
        r = REQS.search(line)
        rows.append({
            "hits": int(m.group(1)), "misses": int(m.group(2)),
            "p50": int(m.group(3)), "p99": int(m.group(4)),
            "requests": int(r.group(1)) if r else 0,
            "ts": line.split()[0] if line.split() else "",
        })
    return rows

w24 = windows("24 hours ago")
h = sum(r["hits"] for r in w24)
m_ = sum(r["misses"] for r in w24)
live = [r for r in w24 if r["hits"] > 0]
out["stats_24h"] = {
    "hits": h, "misses": m_,
    "rate": (h / (h + m_)) if (h + m_) else 0.0,
    "p50": (sum(r["p50"] for r in live) / len(live)) if live else 0,
    "p99": (sum(r["p99"] for r in live) / len(live)) if live else 0,
    "windows": len(w24), "windows_with_hits": len(live),
}
# The most recent windows, so a loaded one can be picked out after --load.
out["recent_windows"] = windows("90 seconds ago")

# Five samples, median reported. One sample is not a measurement: on
# 2026-09-11 a single loopback read landed at 6.58ms against a ~4.3ms
# baseline and read as a 50% regression, while seven consecutive samples
# taken a minute later ran 4.38-4.68ms. The outlier coincided with the
# check's own generated load. Same lesson as BENCHMARKS.md: report the
# median and the spread, so noise looks like noise.
lb = sh("for i in $(seq 1 5); do curl -sk -o /dev/null "
        "-w 'ttfb=%{time_starttransfer} tls=%{time_appconnect}\n' "
        "-H 'Host: mgrosvenor.com' https://127.0.0.1/capabilities; done")
samples = [(float(a), float(b))
           for a, b in re.findall(r"ttfb=([\d.]+) tls=([\d.]+)", lb)]
if samples:
    ttfbs = sorted(s for s, _ in samples)
    tlss = sorted(t for _, t in samples)
    mid = len(ttfbs) // 2
    out["loopback"] = {
        "ttfb": ttfbs[mid], "tls": tlss[mid],
        "ttfb_min": ttfbs[0], "ttfb_max": ttfbs[-1], "n": len(ttfbs),
    }
else:
    out["loopback"] = None

# ── C. disk and logs ─────────────────────────────────────────────────────────
df = sh("df -h / | tail -1").split()
out["disk"] = {"size": df[1], "used": df[2], "avail": df[3],
               "pct": int(df[4].rstrip("%"))} if len(df) >= 5 else None
out["analytics_bytes"] = int(sh("stat -c %%s %s 2>/dev/null || echo 0" % apath).strip() or 0)
out["journal_usage"] = sh("journalctl --disk-usage").strip()

# ── D. services ──────────────────────────────────────────────────────────────
svc = {}
for u in units:
    svc[u] = {
        "active": sh("systemctl is-active %s" % u).strip(),
        "restarts": sh("systemctl show -p NRestarts --value %s" % u).strip(),
    }
out["services"] = svc

errs = sh("journalctl -u %s --since '%d minutes ago' --no-pager -o cat "
          "| grep -E 'WARN|ERROR' | grep -v 'periodic stats' | tail -25"
          % (service, minutes))
out["errors"] = [l for l in errs.splitlines() if l.strip()]

ufw = collections.Counter()
for line in sh("journalctl -k --since '%d minutes ago' --no-pager | grep -i ufw" % minutes).splitlines():
    m = re.search(r"SRC=([0-9.]+)", line)
    if m:
        ufw[m.group(1)] += 1
out["ufw"] = {"total": sum(ufw.values()), "top": ufw.most_common(10)}

if "render-contact" in units:
    rc = sh("journalctl -u render-contact --since '%d minutes ago' --no-pager -o cat "
            "| grep -Ei 'rejected|message sent|smtp|error|warn' | tail -20" % minutes)
    out["render_contact"] = [l for l in rc.splitlines() if l.strip()]

# ── E. analytics: aggregate on the box ───────────────────────────────────────
cut = sh("date -u -d '%d minutes ago' +%%Y-%%m-%%dT%%H:%%M" % minutes).strip()
by_ua  = collections.defaultdict(lambda: {"n": 0, "ips": collections.Counter(),
                                          "paths": collections.Counter()})
by_ip  = collections.defaultdict(lambda: {"n": 0, "uas": set(),
                                          "paths": collections.Counter(),
                                          "status": collections.Counter(),
                                          "first": None, "last": None})
status = collections.Counter()
probes, injections, total = [], [], 0

try:
    with open(apath, "r", errors="replace") as fh:
        # Seek near the end: an hour of traffic is never more than the last
        # few MB, and re-reading 46MB every hour is waste.
        try:
            import os
            size = os.path.getsize(apath)
            if size > 24_000_000:
                fh.seek(size - 24_000_000)
                fh.readline()
        except Exception:
            pass
        for line in fh:
            try:
                d = json.loads(line)
            except Exception:
                continue
            if d.get("timestamp", "") < cut:
                continue
            f = d.get("fields", {})
            if f.get("message") != "request":
                continue
            total += 1
            ua, ip, path = f.get("user_agent", ""), f.get("client_ip", ""), f.get("path", "")
            st = f.get("status")
            status[st] += 1
            u = by_ua[ua]; u["n"] += 1; u["ips"][ip] += 1; u["paths"][path] += 1
            i = by_ip[ip]; i["n"] += 1; i["uas"].add(ua); i["paths"][path] += 1
            i["status"][st] += 1
            ts = d.get("timestamp", "")
            if i["first"] is None or ts < i["first"]: i["first"] = ts
            if i["last"] is None or ts > i["last"]:  i["last"] = ts
            if len(probes) < 400:
                probes.append((ts, ip, path, st, ua))
except FileNotFoundError:
    out["analytics_error"] = "not found at %s" % apath

out["analytics"] = {
    "cutoff": cut,
    "total": total,
    "status": {str(k): v for k, v in status.items()},
    # Full ip list per UA, not a top-6. Truncating here made the forged
    # count exceed the forger's own request total, because every request from
    # an unlisted ip was attributed to forgery.
    "by_ua": {ua: {"n": v["n"], "ips": v["ips"].most_common(),
                   "ip_total": len(v["ips"]),
                   "paths": v["paths"].most_common(6)}
              for ua, v in by_ua.items()},
    "by_ip": {ip: {"n": v["n"], "ua_count": len(v["uas"]),
                   "paths": v["paths"].most_common(12),
                   "status": {str(k): c for k, c in v["status"].items()},
                   "first": v["first"], "last": v["last"]}
              for ip, v in by_ip.items()},
    "sample": probes,
}
print(json.dumps(out))
'''


# ── Local side ───────────────────────────────────────────────────────────────

def probe(node, minutes):
    """One ssh per node. Returns (node, result_dict_or_None, error_string)."""
    cmd = [
        "ssh", "-o", "ConnectTimeout=20", "-o", "BatchMode=yes",
        "root@%s" % node["host"],
        "python3 - %d %s %s %s" % (minutes, node["service"], node["analytics"],
                                   ",".join(node["units"])),
    ]
    try:
        p = subprocess.run(cmd, input=REMOTE_PROBE, capture_output=True,
                           text=True, timeout=420)
    except subprocess.TimeoutExpired:
        return node, None, "ssh timed out"
    if p.returncode != 0:
        return node, None, (p.stderr or "").strip()[:400] or "ssh failed"
    try:
        return node, json.loads(p.stdout.strip().splitlines()[-1]), None
    except Exception as e:
        return node, None, "unparseable probe output: %s" % e


def generate_load(rounds=10):
    """Warm, then load. Returns the number of requests made.

    Anything measured from this is GENERATED and is reported as such. It
    exists to prove instrumentation is live and to read a non-idle window,
    never to describe production.
    """
    made = 0
    for _ in range(rounds + 1):
        for p in WARM_PATHS:
            subprocess.run(["curl", "-s", "--compressed", "-o", "/dev/null",
                            "https://mgrosvenor.com" + p],
                           capture_output=True, timeout=30)
            made += 1
    return made


def cache_headers():
    """Origin's headers. Checked once for the fleet, not per node."""
    want = {
        "/capabilities": "public, max-age=60, s-maxage=86400, stale-while-revalidate=60",
        "/contact": "no-store",
    }
    rows = []
    for path, expect in want.items():
        try:
            p = subprocess.run(["curl", "-sI", "https://mgrosvenor.com" + path],
                               capture_output=True, text=True, timeout=30)
            got = ""
            for line in p.stdout.splitlines():
                if line.lower().startswith("cache-control:"):
                    got = line.split(":", 1)[1].strip()
            rows.append((path, expect, got, got == expect))
        except Exception as e:
            rows.append((path, expect, "ERROR %s" % e, False))

    # A versioned asset, discovered rather than hardcoded: the hash changes on
    # every deploy, so a fixed URL would rot into a false alarm.
    asset, got = None, ""
    try:
        p = subprocess.run(["curl", "-s", "https://mgrosvenor.com/capabilities"],
                           capture_output=True, text=True, timeout=30)
        m = re.search(r"(/assets/[A-Za-z0-9_./-]+\?v=[a-z0-9]+)", p.stdout)
        if m:
            asset = m.group(1)
            q = subprocess.run(["curl", "-sI", "https://mgrosvenor.com" + asset],
                               capture_output=True, text=True, timeout=30)
            for line in q.stdout.splitlines():
                if line.lower().startswith("cache-control:"):
                    got = line.split(":", 1)[1].strip()
    except Exception:
        pass
    ok = "max-age=31536000" in got and "immutable" in got
    rows.append((asset or "(no ?v= asset found)",
                 "max-age=31536000, immutable", got, ok))
    return rows


def classify_crawlers(analytics):
    """Split bot-shaped traffic into forged and genuine.

    An IP presenting many distinct user agents is rotating them. That is not a
    crawler, and counting its ClaudeBot and GPTBot strings as sightings is how
    a report invents fifteen AI crawler visits out of one attacker.
    """
    by_ip = analytics.get("by_ip", {})
    forgers = {ip for ip, v in by_ip.items()
               if v["ua_count"] >= 10 and v["n"] >= 20}

    genuine, forged_n = {}, 0
    for ua, v in analytics.get("by_ua", {}).items():
        if not BOT_RE.search(ua):
            continue
        real_ips = [(ip, n) for ip, n in v["ips"] if ip not in forgers]
        forged_n += v["n"] - sum(n for _, n in real_ips)
        if real_ips:
            genuine[ua] = {"n": sum(n for _, n in real_ips),
                           "ips": real_ips,
                           "paths": v["paths"]}
    return genuine, forged_n, forgers


def fmt_bytes(n):
    for unit in ("B", "K", "M", "G"):
        if n < 1024 or unit == "G":
            return "%.0f%s" % (n, unit)
        n /= 1024.0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--minutes", type=int, default=60,
                    help="security and crawler window (default 60)")
    ap.add_argument("--load", action="store_true",
                    help="generate traffic and read a loaded window; always labelled GENERATED")
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    args = ap.parse_args()

    generated = 0
    if args.load:
        print("generating load (labelled GENERATED, never mixed into observed)...",
              file=sys.stderr)
        generated = generate_load()
        time.sleep(11)   # let a 10s window close over the traffic

    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as ex:
        results = list(ex.map(lambda n: probe(n, args.minutes), NODES))

    headers = cache_headers()

    if args.json:
        print(json.dumps({
            "generated_requests": generated,
            "cache_headers": [{"path": p, "expect": e, "got": g, "ok": ok}
                              for p, e, g, ok in headers],
            "nodes": {n["name"]: (r if r else {"error": err})
                      for n, r, err in results},
        }, indent=2))
        return 0

    faults, warnings = [], []
    print("\n" + "=" * 74)
    print("mgrosvenor.com health check   %s UTC   window=%dm"
          % (time.strftime("%Y-%m-%dT%H:%M", time.gmtime()), args.minutes))
    print("=" * 74)

    # ── A ────────────────────────────────────────────────────────────────────
    print("\nA. LOGGING")
    for node, r, err in results:
        if err:
            faults.append("%s: probe failed: %s" % (node["name"], err))
            print("  %-4s UNREACHABLE  %s" % (node["name"], err))
            continue
        t = r["log_targets"]
        stats = any("stats" in k for k in t)
        only_analytics = set(t) == {"analytics"}
        if only_analytics or not stats:
            faults.append(
                "%s: LOGGING BLIND. Fix: systemctl restart %s"
                % (node["name"], node["service"]))
            print("  %-4s FAULT  targets=%s" % (node["name"], t))
        else:
            print("  %-4s ok     %s" % (node["name"],
                  ", ".join("%s=%d" % (k, v) for k, v in sorted(t.items()))))

    # ── B ────────────────────────────────────────────────────────────────────
    print("\nB. PERFORMANCE  (24h observed, per node)")
    # "windows" is the count the percentiles are averaged over, which is
    # windows that actually served a hit. Total windows is ~8640 in 24h on
    # every node regardless of traffic, so it says nothing.
    print("  %-5s %8s %8s %8s %10s %10s %10s" %
          ("node", "hits", "misses", "rate", "hit p50", "hit p99", "windows*"))
    for node, r, err in results:
        if err:
            continue
        s = r["stats_24h"]
        print("  %-5s %8d %8d %8.4f %8.0fns %8.0fns %10d" %
              (node["name"], s["hits"], s["misses"], s["rate"],
               s["p50"], s["p99"], s["windows_with_hits"]))
        if s["hits"] + s["misses"] > 200 and s["rate"] < HIT_RATE_FLOOR:
            warnings.append("%s: 24h cache_hit_rate %.4f below %.2f floor"
                            % (node["name"], s["rate"], HIT_RATE_FLOOR))

    print("  *windows with at least one cache hit, which is what p50/p99 average over")

    if args.load:
        print("\n  GENERATED (%d requests made by this tool; not a production number)"
              % generated)
        for node, r, err in results:
            if err:
                continue
            loaded = [w for w in r.get("recent_windows", []) if w["hits"] + w["misses"] > 0]
            if loaded:
                w = max(loaded, key=lambda x: x["hits"] + x["misses"])
                tot = w["hits"] + w["misses"]
                print("  %-5s window hits=%d misses=%d rate=%.4f p50=%dns p99=%dns"
                      % (node["name"], w["hits"], w["misses"],
                         w["hits"] / tot if tot else 0, w["p50"], w["p99"]))
            else:
                print("  %-5s no non-idle window (traffic went to another node)"
                      % node["name"])

    print("\n  loopback TTFB")
    for node, r, err in results:
        if err or not r.get("loopback"):
            continue
        lb = r["loopback"]
        flag = "  SLOW" if lb["ttfb"] > TTFB_WARN_S else ""
        if flag:
            warnings.append("%s: loopback TTFB median %.1fms" % (node["name"], lb["ttfb"] * 1000))
        print("  %-5s ttfb=%.2fms tls=%.2fms   (median of %d, %.2f-%.2fms)%s"
              % (node["name"], lb["ttfb"] * 1000, lb["tls"] * 1000,
                 lb.get("n", 1), lb.get("ttfb_min", lb["ttfb"]) * 1000,
                 lb.get("ttfb_max", lb["ttfb"]) * 1000, flag))

    print("\n  cache headers (origin)")
    for path, expect, got, ok in headers:
        if not ok:
            faults.append("cache header regressed on %s: got %r" % (path, got))
        print("  %-48s %s" % (path[:48], "ok" if ok else "REGRESSED: %r" % got))

    # ── C ────────────────────────────────────────────────────────────────────
    print("\nC. DISK AND LOGS")
    for node, r, err in results:
        if err:
            continue
        d = r.get("disk")
        if d and d["pct"] >= DISK_WARN_PCT:
            warnings.append("%s: disk %d%%" % (node["name"], d["pct"]))
        print("  %-5s disk %s used of %s (%d%%)   analytics %s   journal %s"
              % (node["name"], d["used"], d["size"], d["pct"],
                 fmt_bytes(r["analytics_bytes"]), r["journal_usage"]))

    # ── D ────────────────────────────────────────────────────────────────────
    print("\nD. SECURITY AND TRAFFIC")
    for node, r, err in results:
        if err:
            continue
        bad = [(u, v) for u, v in r["services"].items()
               if v["active"] != "active" or (v["restarts"] or "0") != "0"]
        for u, v in bad:
            faults.append("%s: %s is %s with %s restarts"
                          % (node["name"], u, v["active"], v["restarts"]))
        print("  %-5s %d services active, %d restarts%s"
              % (node["name"], sum(1 for v in r["services"].values()
                                   if v["active"] == "active"),
                 sum(int(v["restarts"] or 0) for v in r["services"].values()),
                 "" if not bad else "   <-- SEE FAULTS"))
        if r.get("errors"):
            print("        %d WARN/ERROR lines; latest: %s"
                  % (len(r["errors"]), r["errors"][-1][:110]))
        if r.get("render_contact"):
            print("        render-contact: %d notable lines" % len(r["render_contact"]))

    for node, r, err in results:
        if err:
            continue
        a = r.get("analytics", {})
        by_ip = a.get("by_ip", {})
        # Bursts, probes and injections.
        for ip, v in sorted(by_ip.items(), key=lambda x: -x[1]["n"])[:4]:
            probe_paths = [(p, n) for p, n in v["paths"] if PROBE_RE.search(p)]
            inj = [(p, n) for p, n in v["paths"] if INJECTION_RE.search(p)]
            # Volume alone is not suspicious. A burst counts only when it is
            # also failing: an ordinary heavy client is not an incident, and
            # reporting one trains the reader to skim the fault list. This
            # rule fired on the check's own load generator, 178 requests from
            # the operator's own address, all served 200.
            errors = sum(n for code, n in v["status"].items() if int(code) >= 400)
            error_ratio = errors / v["n"] if v["n"] else 0.0
            burst = v["n"] >= 100 and error_ratio >= 0.5
            if not (probe_paths or inj or burst):
                continue
            served = v["status"].get("200", 0)
            label = []
            if v["ua_count"] >= 10:
                label.append("%d UAs (rotating)" % v["ua_count"])
            if burst:
                label.append("%.0f%% refused" % (error_ratio * 100))
            if probe_paths:
                label.append("%d probe paths" % len(probe_paths))
            if inj:
                label.append("INJECTION-SHAPED")
            faults.append(
                "%s: %s sent %d requests (%s), %s..%s, %d served 200"
                % (node["name"], ip, v["n"], "; ".join(label) or "burst",
                   (v["first"] or "")[11:19], (v["last"] or "")[11:19], served))
            print("\n  %s  %s  %d requests  %s" %
                  (node["name"], ip, v["n"], "; ".join(label)))
            print("     window %s -> %s   status %s"
                  % ((v["first"] or "")[11:19], (v["last"] or "")[11:19], v["status"]))
            seen = set()
            for p, n in probe_paths + inj:
                if p in seen:
                    continue
                seen.add(p)
                if len(seen) > 12:
                    break
                print("       %4d  %s" % (n, p[:90]))

        u = r.get("ufw", {})
        if u.get("total"):
            print("  %-5s ufw blocked %d packets from %d sources (top %s)"
                  % (node["name"], u["total"], len(u["top"]),
                     ", ".join("%s x%d" % (ip, n) for ip, n in u["top"][:3])))

    # ── E ────────────────────────────────────────────────────────────────────
    print("\nE. CRAWLERS  (reported every run, even a quiet one)")
    any_crawler = False
    for node, r, err in results:
        if err:
            continue
        genuine, forged, forgers = classify_crawlers(r.get("analytics", {}))
        if forged:
            print("  %-5s %d bot-shaped requests were FORGED by %s and are excluded"
                  % (node["name"], forged, ", ".join(sorted(forgers)) or "a UA rotator"))
        if not genuine:
            print("  %-5s no genuine crawler traffic in the window" % node["name"])
            continue
        any_crawler = True
        for ua, v in sorted(genuine.items(), key=lambda x: -x[1]["n"]):
            ips = ", ".join(ip for ip, _ in v["ips"][:3])
            more = "" if len(v["ips"]) <= 3 else " (+%d more)" % (len(v["ips"]) - 3)
            # 10.0.0.x is the WireGuard backbone, not a visitor. Origin
            # attributes every relayed request to the tunnel rather than to
            # the client; that fix is written and waiting on the freeze. Until
            # then a crawler that reached lon or chi appears here with the
            # tunnel's address, and reporting that as its source is wrong.
            relayed = "   [relayed: true source is on the cache node]" \
                if any(ip.startswith("10.0.0.") for ip, _ in v["ips"]) else ""
            print("  %-5s %4d  %s%s%s" % (node["name"], v["n"], ips, more, relayed))
            print("        UA: %s" % ua)
            print("        paths: %s"
                  % ", ".join("%s x%d" % (p, n) for p, n in v["paths"][:4]))
    if not any_crawler:
        print("  (none seen on any node)")

    # ── Verdict ──────────────────────────────────────────────────────────────
    print("\n" + "=" * 74)
    if faults:
        print("FAULTS (%d)" % len(faults))
        for f in faults:
            print("  - %s" % f)
    if warnings:
        print("WARNINGS (%d)" % len(warnings))
        for w in warnings:
            print("  - %s" % w)
    if not faults and not warnings:
        print("ALL CLEAR on all three nodes.")
    print("=" * 74 + "\n")
    return 1 if faults else 0


if __name__ == "__main__":
    sys.exit(main())
