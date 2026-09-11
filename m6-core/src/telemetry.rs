//! m6's own telemetry formats, and how to read them back.
//!
//! `/health`, `/perf`, the `periodic stats` log line and the analytics NDJSON
//! stream are all m6 features. Every deployment of m6 emits them and every
//! deployment therefore has the same problem: turning them back into an answer
//! about whether a fleet is healthy, fast, and being probed.
//!
//! That analysis was a shell pipeline, then a Python script sitting beside one
//! site. It belongs here. A site is one instance of m6, not the thing m6 is
//! for.
//!
//! # The write side has no struct
//!
//! `m6-http` emits an analytics row with `tracing::info!(target: "analytics",
//! node = .., path = .., ..)`, and the JSON is produced by the subscriber's
//! formatter. So there is no type describing the format, on either side, and
//! every consumer has re-derived it by looking at a sample. That is how the
//! field set below came to be read as `ts` and `user_agent` rather than
//! `timestamp` and `fields.user_agent`, which yields an empty result that
//! looks exactly like a quiet hour rather than like a bug.
//!
//! [`AnalyticsRecord`] is that missing definition. It is `Serialize` as well
//! as `Deserialize` so the write side can adopt it and the format stops being
//! whatever the logging macro happened to produce.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

// ── Analytics NDJSON ─────────────────────────────────────────────────────────

/// One line of the analytics stream.
///
/// The envelope is the tracing subscriber's: a timestamp and level outside, the
/// event's own fields nested under `fields`. Reading `user_agent` at the top
/// level finds nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsRecord {
    pub timestamp: String,
    #[serde(default)]
    pub level: String,
    pub fields: AnalyticsFields,
}

/// The event's fields, as emitted by `m6_http::analytics`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalyticsFields {
    /// `"request"` for site traffic, `"monitor"` for a `/health` or `/perf`
    /// poll. Monitoring is deliberately a different message so that site
    /// traffic stays honest while the monitoring itself stays visible; every
    /// consumer that wants traffic must filter on this.
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub status: u16,
    #[serde(default)]
    pub cache_state: String,
    #[serde(default)]
    pub client_ip: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub session_new: bool,
    #[serde(default)]
    pub referer_host: Option<String>,
    #[serde(default)]
    pub referer_path: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub latency_ns: Option<u64>,
}

impl AnalyticsRecord {
    pub fn is_request(&self) -> bool {
        self.fields.message == "request"
    }
    pub fn is_monitor(&self) -> bool {
        self.fields.message == "monitor"
    }
    pub fn user_agent(&self) -> &str {
        self.fields.user_agent.as_deref().unwrap_or("")
    }
}

/// Parse an NDJSON stream, keeping records at or after `since`.
///
/// `since` is compared as a string, which works because the timestamps are
/// RFC 3339 in UTC with a fixed number of digits, so lexical order is
/// chronological order. Pass an empty string for everything.
///
/// Unparseable lines are skipped rather than failing the read: this is a log
/// that may be being appended to as it is read, so a torn final line is
/// ordinary and is not a reason to return nothing.
pub fn parse_analytics<'a>(
    text: &'a str,
    since: &'a str,
) -> impl Iterator<Item = AnalyticsRecord> + 'a {
    crate::ndjson::read_str::<AnalyticsRecord>(text)
        .filter(move |rec| rec.timestamp.as_str() >= since)
}

// ── periodic stats log line ──────────────────────────────────────────────────

/// One `periodic stats` line.
///
/// **This is a ten second window, not a lifetime.** An idle window is all
/// zeros, which looks exactly like broken instrumentation; that is a different
/// condition from the log target having gone silent, and the two have
/// different fixes. [`aggregate_windows`] only averages percentiles over
/// windows that actually served a hit, for the same reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeriodicStats {
    pub requests: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub backend_errors: u64,
    pub pool_members: u64,
    pub hit_p50_ns: u64,
    pub hit_p99_ns: u64,
    pub miss_p50_ns: u64,
    pub miss_p99_ns: u64,
}

/// Strip ANSI colour codes.
///
/// journald output carries them, and a pattern match against a coloured line
/// silently finds nothing. Every reader of these logs has hit this.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b && i + 1 < b.len() && b[i + 1] == b'[' {
            i += 2;
            while i < b.len() && !b[i].is_ascii_alphabetic() {
                i += 1;
            }
            i += 1;
        } else {
            let ch_len = s[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            out.push_str(&s[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

fn field(line: &str, key: &str) -> Option<u64> {
    let pat = format!("{key}=");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

impl PeriodicStats {
    /// Parse one line, ANSI codes and all. Returns `None` if it is not a
    /// `periodic stats` line.
    pub fn parse(line: &str) -> Option<Self> {
        let line = strip_ansi(line);
        if !line.contains("periodic stats") {
            return None;
        }
        Some(Self {
            requests: field(&line, "requests").unwrap_or(0),
            cache_hits: field(&line, "cache_hits").unwrap_or(0),
            cache_misses: field(&line, "cache_misses").unwrap_or(0),
            backend_errors: field(&line, "backend_errors").unwrap_or(0),
            pool_members: field(&line, "pool_members").unwrap_or(0),
            hit_p50_ns: field(&line, "hit_p50_ns").unwrap_or(0),
            hit_p99_ns: field(&line, "hit_p99_ns").unwrap_or(0),
            miss_p50_ns: field(&line, "miss_p50_ns").unwrap_or(0),
            miss_p99_ns: field(&line, "miss_p99_ns").unwrap_or(0),
        })
    }

    pub fn served_anything(&self) -> bool {
        self.cache_hits + self.cache_misses > 0
    }
}

/// Aggregate of many windows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatsAggregate {
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
    /// Mean of the per-window p50, over windows that served at least one hit.
    pub hit_p50_ns: u64,
    pub hit_p99_ns: u64,
    /// Windows seen in total. Roughly 8640 per node per day whatever the
    /// traffic, so on its own it says nothing about load.
    pub windows: usize,
    /// Windows that served at least one hit. This is the count the
    /// percentiles are averaged over, and the one worth reporting.
    pub windows_with_hits: usize,
}

pub fn aggregate_windows<'a>(windows: impl IntoIterator<Item = &'a PeriodicStats>) -> StatsAggregate {
    let mut agg = StatsAggregate::default();
    let (mut s50, mut s99) = (0u64, 0u64);
    for w in windows {
        agg.windows += 1;
        agg.hits += w.cache_hits;
        agg.misses += w.cache_misses;
        if w.cache_hits > 0 {
            agg.windows_with_hits += 1;
            s50 += w.hit_p50_ns;
            s99 += w.hit_p99_ns;
        }
    }
    let total = agg.hits + agg.misses;
    agg.hit_rate = if total > 0 { agg.hits as f64 / total as f64 } else { 0.0 };
    if agg.windows_with_hits > 0 {
        agg.hit_p50_ns = s50 / agg.windows_with_hits as u64;
        agg.hit_p99_ns = s99 / agg.windows_with_hits as u64;
    }
    agg
}

// ── Classification ───────────────────────────────────────────────────────────

/// Substrings that mark a user agent as claiming to be a bot.
///
/// A *claim*, not a fact: see [`TrafficSummary`] for why the claim alone is
/// not enough. The generic terms matter as much as the named ones. A list of
/// known crawler names missed `CyberConvoyScout` and `GenomeCrawlerd`, both of
/// which self-identify plainly; `scout` and `crawl` catch them.
pub const BOT_MARKERS: &[&str] = &[
    "bot", "crawl", "spider", "scout", "probe", "slurp", "fetcher", "archiver",
    "amzn-", "gptbot", "claudebot", "claude-user", "claude-web", "anthropic",
    "perplexity", "applebot", "bytespider", "ccbot", "facebookexternalhit",
    "twitterbot", "linkedinbot", "mj12", "yandex", "baidu", "duckduck",
    "googlebot", "google-extended", "bingbot", "amazonbot", "whatsapp",
    "discord", "slackbot", "grok", "oai-searchbot", "chatgpt-user", "semrush",
];

/// Path fragments that only appear in probes. A static site has no `/fetch`.
pub const PROBE_MARKERS: &[&str] = &[
    ".env", "wp-admin", "wp-login", "phpmyadmin", "/.git", "xmlrpc.php",
    "/admin", "/actuator", "credentials", "passwd", "/@fs", "../", "/fetch",
    "/proxy", "/webhook", "/api/fetch", "/redirect", ".aws", ".azure",
    "gcloud", "/shell", "/cgi-bin", "/console", "/.ssh", "/config.json",
];

/// Query or path shapes that indicate an injection attempt.
pub const INJECTION_MARKERS: &[&str] = &[
    "union select", "or 1=1", "<script", "javascript:", "%3cscript",
    "/etc/passwd", "base64_decode", "concat(", "sleep(", "benchmark(",
];

fn matches_any(haystack: &str, needles: &[&str]) -> bool {
    let h = haystack.to_ascii_lowercase();
    needles.iter().any(|n| h.contains(n))
}

pub fn claims_to_be_bot(user_agent: &str) -> bool {
    matches_any(user_agent, BOT_MARKERS)
}

/// Probe shapes, checked raw and decoded, for the same reason as
/// [`looks_like_injection`]: `..%2F` is a traversal and `%2E%65nv` is `.env`.
pub fn looks_like_probe(path: &str) -> bool {
    if matches_any(path, PROBE_MARKERS) {
        return true;
    }
    matches_any(&crate::request::url_decode(path), PROBE_MARKERS)
}

/// Injection shapes, checked against the decoded path.
///
/// A URL carries these percent-encoded: `UNION%20SELECT`, `%3Cscript%3E`,
/// `..%2Fetc%2Fpasswd`. Matching the raw path against `"union select"` finds
/// none of them, which is a detector that reports clean against exactly the
/// traffic it exists to catch. Decoding first is the whole job, and
/// `request::url_decode` is the one decoder: it reassembles UTF-8 from bytes
/// rather than mapping each byte to a code point.
pub fn looks_like_injection(path: &str) -> bool {
    if matches_any(path, INJECTION_MARKERS) {
        return true;
    }
    let decoded = crate::request::url_decode(path);
    matches_any(&decoded, INJECTION_MARKERS)
}

/// What one client address was doing.
#[derive(Debug, Clone, Default)]
pub struct ClientSummary {
    pub requests: u64,
    pub distinct_user_agents: usize,
    pub status: BTreeMap<u16, u64>,
    pub paths: Vec<(String, u64)>,
    pub probe_paths: Vec<(String, u64)>,
    pub injection_paths: Vec<(String, u64)>,
    pub first_seen: String,
    pub last_seen: String,
    /// Presenting many distinct user agents is rotation, not a fleet.
    pub is_rotating_user_agents: bool,
}

impl ClientSummary {
    /// Share of responses that were 4xx or 5xx.
    ///
    /// A client that is mostly being refused is looking for something it has
    /// not found. A client that is mostly being served is using the site.
    pub fn error_ratio(&self) -> f64 {
        if self.requests == 0 {
            return 0.0;
        }
        let errors: u64 = self
            .status
            .iter()
            .filter(|(code, _)| **code >= 400)
            .map(|(_, n)| *n)
            .sum();
        errors as f64 / self.requests as f64
    }
}

/// A crawler we believe actually visited.
#[derive(Debug, Clone)]
pub struct CrawlerSighting {
    pub user_agent: String,
    pub requests: u64,
    pub client_ips: Vec<String>,
    pub paths: Vec<(String, u64)>,
}

/// An hour of traffic, summarised.
#[derive(Debug, Clone, Default)]
pub struct TrafficSummary {
    pub total_requests: u64,
    pub status: BTreeMap<u16, u64>,
    pub clients: Vec<(String, ClientSummary)>,
    /// Crawlers whose claim we accept, because the address presenting the
    /// claim was not also presenting dozens of others.
    pub crawlers: Vec<CrawlerSighting>,
    /// Bot-shaped requests discarded as forged, and who sent them.
    pub forged_bot_requests: u64,
    pub forgers: Vec<String>,
}

/// How many distinct user agents from one address before we stop believing it.
///
/// Real crawler fleets present one user agent from many addresses. The inverse
/// is a rotator: on 2026-09-11 a single address sent 890 requests across 526
/// user agents, 886 of them bot-shaped, and a name-matching check would have
/// reported ClaudeBot, GPTBot, Applebot, PerplexityBot and a dozen more as
/// having visited. None had.
pub const UA_ROTATION_THRESHOLD: usize = 10;
/// Below this volume, many user agents is more likely a shared egress than a
/// rotator, so the address is not accused.
pub const UA_ROTATION_MIN_REQUESTS: u64 = 20;

impl TrafficSummary {
    pub fn from_records<'a>(records: impl IntoIterator<Item = &'a AnalyticsRecord>) -> Self {
        struct Acc {
            n: u64,
            uas: HashSet<String>,
            paths: HashMap<String, u64>,
            status: BTreeMap<u16, u64>,
            first: String,
            last: String,
        }
        let mut per_ip: HashMap<String, Acc> = HashMap::new();
        let mut per_ua: HashMap<String, (u64, HashMap<String, u64>, HashMap<String, u64>)> =
            HashMap::new();
        let mut summary = TrafficSummary::default();

        for rec in records {
            if !rec.is_request() {
                continue;
            }
            let f = &rec.fields;
            summary.total_requests += 1;
            *summary.status.entry(f.status).or_default() += 1;

            let acc = per_ip.entry(f.client_ip.clone()).or_insert_with(|| Acc {
                n: 0,
                uas: HashSet::new(),
                paths: HashMap::new(),
                status: BTreeMap::new(),
                first: rec.timestamp.clone(),
                last: rec.timestamp.clone(),
            });
            acc.n += 1;
            acc.uas.insert(rec.user_agent().to_string());
            *acc.paths.entry(f.path.clone()).or_default() += 1;
            *acc.status.entry(f.status).or_default() += 1;
            if rec.timestamp < acc.first {
                acc.first = rec.timestamp.clone();
            }
            if rec.timestamp > acc.last {
                acc.last = rec.timestamp.clone();
            }

            let e = per_ua
                .entry(rec.user_agent().to_string())
                .or_insert_with(|| (0, HashMap::new(), HashMap::new()));
            e.0 += 1;
            *e.1.entry(f.client_ip.clone()).or_default() += 1;
            *e.2.entry(f.path.clone()).or_default() += 1;
        }

        let forgers: HashSet<String> = per_ip
            .iter()
            .filter(|(_, a)| {
                a.uas.len() >= UA_ROTATION_THRESHOLD && a.n >= UA_ROTATION_MIN_REQUESTS
            })
            .map(|(ip, _)| ip.clone())
            .collect();

        for (ua, (n, ips, paths)) in per_ua {
            if !claims_to_be_bot(&ua) {
                continue;
            }
            let genuine: Vec<(String, u64)> = ips
                .iter()
                .filter(|(ip, _)| !forgers.contains(*ip))
                .map(|(ip, c)| (ip.clone(), *c))
                .collect();
            let genuine_n: u64 = genuine.iter().map(|(_, c)| c).sum();
            summary.forged_bot_requests += n - genuine_n;
            if genuine_n == 0 {
                continue;
            }
            let mut ip_list: Vec<String> = genuine.into_iter().map(|(ip, _)| ip).collect();
            ip_list.sort();
            let mut p: Vec<(String, u64)> = paths.into_iter().collect();
            p.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            p.truncate(8);
            summary.crawlers.push(CrawlerSighting {
                user_agent: ua,
                requests: genuine_n,
                client_ips: ip_list,
                paths: p,
            });
        }
        summary.crawlers.sort_by(|a, b| b.requests.cmp(&a.requests));

        let mut clients: Vec<(String, ClientSummary)> = per_ip
            .into_iter()
            .map(|(ip, a)| {
                let mut paths: Vec<(String, u64)> = a.paths.into_iter().collect();
                paths.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
                let probe_paths: Vec<(String, u64)> = paths
                    .iter()
                    .filter(|(p, _)| looks_like_probe(p))
                    .cloned()
                    .collect();
                let injection_paths: Vec<(String, u64)> = paths
                    .iter()
                    .filter(|(p, _)| looks_like_injection(p))
                    .cloned()
                    .collect();
                let rotating = a.uas.len() >= UA_ROTATION_THRESHOLD
                    && a.n >= UA_ROTATION_MIN_REQUESTS;
                paths.truncate(16);
                (
                    ip,
                    ClientSummary {
                        requests: a.n,
                        distinct_user_agents: a.uas.len(),
                        status: a.status,
                        paths,
                        probe_paths,
                        injection_paths,
                        first_seen: a.first,
                        last_seen: a.last,
                        is_rotating_user_agents: rotating,
                    },
                )
            })
            .collect();
        clients.sort_by(|a, b| b.1.requests.cmp(&a.1.requests));
        summary.clients = clients;

        let mut f: Vec<String> = forgers.into_iter().collect();
        f.sort();
        summary.forgers = f;
        summary
    }

    /// Clients worth a human's attention.
    ///
    /// Probing, injecting and user-agent rotation are suspicious on their own.
    /// **Volume is not.** A burst only counts when it is also failing, because
    /// an ordinary heavy client is not an incident and reporting it as one
    /// trains the reader to skim the fault list.
    ///
    /// This was found by running the check: it flagged 185 requests from the
    /// operator's own address as a fault, in a run where 178 of them were the
    /// health check's own load generation. A rule that fires on the monitoring
    /// traffic is worse than no rule.
    pub fn suspicious(&self, burst_threshold: u64) -> Vec<&(String, ClientSummary)> {
        self.clients
            .iter()
            .filter(|(_, c)| {
                !c.probe_paths.is_empty()
                    || !c.injection_paths.is_empty()
                    || c.is_rotating_user_agents
                    || (c.requests >= burst_threshold && c.error_ratio() >= 0.5)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ts: &str, ip: &str, path: &str, status: u16, ua: &str) -> AnalyticsRecord {
        AnalyticsRecord {
            timestamp: ts.to_string(),
            level: "INFO".to_string(),
            fields: AnalyticsFields {
                message: "request".to_string(),
                node: "sydney".to_string(),
                path: path.to_string(),
                status,
                client_ip: ip.to_string(),
                user_agent: Some(ua.to_string()),
                ..Default::default()
            },
        }
    }

    /// The exact envelope m6-http emits, byte for byte from production.
    ///
    /// Pinned as a literal because every consumer so far has re-derived the
    /// shape by eye and at least one read it as `ts` and a top-level
    /// `user_agent`, which parses to nothing and looks like a quiet hour.
    #[test]
    fn parses_the_real_envelope() {
        let line = r#"{"timestamp":"2026-09-11T06:05:14.986903Z","level":"INFO","fields":{"message":"request","node":"sydney","path":"/capabilities","status":200,"cache_state":"HIT","client_ip":"220.233.79.92","session_id":"494a54891b104c46bd7cdaecab0f202f","session_new":true,"user_agent":"curl/8.7.1","latency_ns":3360}}"#;
        let recs: Vec<_> = parse_analytics(line, "").collect();
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert!(r.is_request());
        assert_eq!(r.fields.node, "sydney");
        assert_eq!(r.fields.path, "/capabilities");
        assert_eq!(r.fields.status, 200);
        assert_eq!(r.fields.cache_state, "HIT");
        assert_eq!(r.user_agent(), "curl/8.7.1");
        assert_eq!(r.fields.latency_ns, Some(3360));
    }

    #[test]
    fn a_torn_last_line_does_not_lose_the_rest() {
        let text = "{\"timestamp\":\"2026-01-01T00:00:00Z\",\"fields\":{\"message\":\"request\"}}\n{\"timestamp\":\"2026-01";
        assert_eq!(parse_analytics(text, "").count(), 1);
    }

    #[test]
    fn monitor_rows_are_not_site_traffic() {
        let mut m = rec("2026-09-11T06:00:00Z", "1.2.3.4", "/health", 200, "curl/8");
        m.fields.message = "monitor".to_string();
        let s = TrafficSummary::from_records(&[m]);
        assert_eq!(s.total_requests, 0, "a /health poll is not a visitor");
    }

    /// A ten second window that served nothing is all zeros, and averaging its
    /// p50 into the fleet number drags the percentile toward zero.
    #[test]
    fn idle_windows_do_not_dilute_the_percentiles() {
        let busy = PeriodicStats {
            cache_hits: 76, cache_misses: 1, hit_p50_ns: 3455, hit_p99_ns: 3968,
            ..Default::default()
        };
        let idle = PeriodicStats::default();
        let agg = aggregate_windows(&[busy.clone(), idle.clone(), idle.clone()]);
        assert_eq!(agg.windows, 3);
        assert_eq!(agg.windows_with_hits, 1);
        assert_eq!(agg.hit_p50_ns, 3455, "idle windows must not be averaged in");
        assert_eq!(agg.hits, 76);
    }

    #[test]
    fn parses_a_real_periodic_stats_line_with_ansi() {
        let line = "\x1b[2m2026-09-11T06:04:13.905431Z\x1b[0m \x1b[32m INFO\x1b[0m \
                    m6_http_lib::stats: periodic stats requests=5163 rps_avg=7 rps_peak=21 \
                    cache_hits=76 cache_misses=1 cache_hit_rate=0.9870 backend_errors=0 \
                    pool_members=4 hit_p0_ns=2983 hit_p50_ns=3455 hit_p99_ns=3968 \
                    hit_max_ns=4018 miss_p0_ns=7154090 miss_p50_ns=7154090";
        let s = PeriodicStats::parse(line).expect("should parse through the colour codes");
        assert_eq!(s.cache_hits, 76);
        assert_eq!(s.cache_misses, 1);
        assert_eq!(s.hit_p50_ns, 3455);
        assert_eq!(s.hit_p99_ns, 3968);
        assert_eq!(s.pool_members, 4);
        assert_eq!(s.miss_p50_ns, 7154090);
    }

    /// `hit_p0_ns` must not satisfy a search for `hit_p50_ns`, and
    /// `cache_hits` must not be read out of `cache_hit_rate`.
    #[test]
    fn field_names_are_not_matched_as_prefixes() {
        let line = "periodic stats cache_hit_rate=0.9870 cache_hits=76 hit_p0_ns=2983 hit_p50_ns=3455";
        let s = PeriodicStats::parse(line).unwrap();
        assert_eq!(s.cache_hits, 76);
        assert_eq!(s.hit_p50_ns, 3455);
    }

    /// The 2026-09-11 incident, reduced.
    ///
    /// One address, many bot-shaped user agents, one request each. Believing
    /// the claim produces a report naming a dozen AI crawlers that never
    /// visited, next to a genuine crawler that did.
    #[test]
    fn a_user_agent_rotator_is_not_a_dozen_crawlers() {
        let mut records = Vec::new();
        for (i, name) in [
            "ClaudeBot/1.0", "GPTBot/1.2", "Applebot/0.1", "PerplexityBot/1.0",
            "Bytespider", "GrokBot/1.0", "LinkedInBot/1.0", "Slackbot/1.0",
            "Discordbot/2.0", "Amzn-SearchBot/1.0", "ChatGPT-User/1.0",
            "facebookexternalhit/1.1",
        ]
        .iter()
        .enumerate()
        {
            records.push(rec(
                &format!("2026-09-11T05:47:{:02}Z", i),
                "34.91.241.0",
                "/fetch",
                404,
                &format!("Mozilla/5.0 (compatible; {name})"),
            ));
        }
        for i in 0..20 {
            records.push(rec(
                &format!("2026-09-11T05:48:{:02}Z", i),
                "34.91.241.0",
                "/.env",
                404,
                &format!("Mozilla/5.0 (filler-{i})"),
            ));
        }
        // A real crawler, one user agent from several addresses.
        for (i, ip) in ["18.205.91.101", "34.194.233.48", "52.203.152.231"].iter().enumerate() {
            records.push(rec(
                &format!("2026-09-11T05:50:{:02}Z", i),
                ip,
                "/gallery",
                200,
                "Mozilla/5.0 (compatible; Amazonbot/0.1; +https://developer.amazon.com/support/amazonbot)",
            ));
        }

        let s = TrafficSummary::from_records(&records);

        assert_eq!(s.forgers, vec!["34.91.241.0".to_string()]);
        assert_eq!(
            s.crawlers.len(),
            1,
            "only Amazonbot actually visited; got {:?}",
            s.crawlers.iter().map(|c| &c.user_agent).collect::<Vec<_>>()
        );
        assert!(s.crawlers[0].user_agent.contains("Amazonbot"));
        assert_eq!(s.crawlers[0].requests, 3);
        assert_eq!(s.crawlers[0].client_ips.len(), 3);
        assert_eq!(s.forged_bot_requests, 12, "the twelve bot-shaped forgeries");

        // And the rotator is surfaced on its own terms.
        let sus = s.suspicious(100);
        assert_eq!(sus.len(), 1);
        assert_eq!(sus[0].0, "34.91.241.0");
        assert!(sus[0].1.is_rotating_user_agents);
        assert!(!sus[0].1.probe_paths.is_empty());
    }

    /// A genuine crawler fleet is the inverse shape and must survive.
    #[test]
    fn many_addresses_one_user_agent_is_a_fleet_not_a_forger() {
        let ua = "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";
        let records: Vec<_> = (0..40)
            .map(|i| rec(&format!("2026-09-11T05:0{}:00Z", i % 10),
                         &format!("66.249.66.{i}"), "/", 200, ua))
            .collect();
        let s = TrafficSummary::from_records(&records);
        assert!(s.forgers.is_empty());
        assert_eq!(s.forged_bot_requests, 0);
        assert_eq!(s.crawlers.len(), 1);
        assert_eq!(s.crawlers[0].requests, 40);
    }

    /// Named lists go stale. These two self-identify and were both missed by
    /// a check that listed crawler names.
    #[test]
    fn generic_markers_catch_crawlers_no_list_would_name() {
        assert!(claims_to_be_bot("Mozilla/5.0 (compatible; CyberConvoyScout/1.0; +https://scout.cyberconvoy.co)"));
        assert!(claims_to_be_bot("Mozilla/5.0 (compatible; GenomeCrawlerd/1.0; +https://www.nokia.com/genomecrawler)"));
        assert!(!claims_to_be_bot(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_7_6) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0 Safari/537.36"
        ));
    }

    /// Volume alone must not raise a fault.
    ///
    /// The check's own load generator hit the site 178 times in an hour and
    /// was reported as an incident. A heavy client that is being served
    /// normally is a user, not an attacker.
    #[test]
    fn a_busy_client_being_served_is_not_suspicious() {
        let records: Vec<_> = (0..185)
            .map(|i| rec(&format!("2026-09-11T06:{:02}:00Z", i % 60),
                         "220.233.79.92", "/capabilities", 200, "curl/8.7.1"))
            .collect();
        let s = TrafficSummary::from_records(&records);
        assert_eq!(s.total_requests, 185);
        assert!(s.suspicious(100).is_empty(), "a served burst is not an incident");
    }

    /// The same volume, mostly refused, is.
    #[test]
    fn a_busy_client_being_refused_is_suspicious() {
        let records: Vec<_> = (0..185)
            .map(|i| rec(&format!("2026-09-11T06:{:02}:00Z", i % 60),
                         "203.0.113.9", "/nonexistent", 404, "curl/8.7.1"))
            .collect();
        let s = TrafficSummary::from_records(&records);
        let sus = s.suspicious(100);
        assert_eq!(sus.len(), 1);
        assert!(sus[0].1.error_ratio() >= 0.5);
    }

    #[test]
    fn probe_and_injection_shapes() {
        for p in ["/.env", "/@fs/root/.aws/credentials", "/wp-admin/setup.php",
                  "/static../etc/passwd", "/actuator", "/proxy"] {
            assert!(looks_like_probe(p), "{p} should read as a probe");
        }
        for p in ["/", "/capabilities", "/assets/css/style.css?v=713e7c6e"] {
            assert!(!looks_like_probe(p), "{p} is an ordinary path");
        }
        // Percent-encoded, which is how these actually arrive. Matching the
        // raw path alone reports clean against exactly the traffic this
        // exists to catch.
        assert!(looks_like_injection("/x?q=1%20UNION%20SELECT%20password"));
        assert!(looks_like_injection("/x?q=%3Cscript%3Ealert(1)%3C/script%3E"));
        assert!(looks_like_injection("/x?q=1+UNION+SELECT+password"));
        assert!(looks_like_probe("/static..%2Fetc%2Fpasswd"));
        // And unencoded still works.
        assert!(looks_like_injection("/search?q=<script>alert(1)</script>"));
        assert!(!looks_like_injection("/capabilities"));
    }

    #[test]
    fn since_filters_by_timestamp() {
        let text = [
            r#"{"timestamp":"2026-09-11T04:00:00Z","fields":{"message":"request"}}"#,
            r#"{"timestamp":"2026-09-11T06:00:00Z","fields":{"message":"request"}}"#,
        ]
        .join("\n");
        assert_eq!(parse_analytics(&text, "2026-09-11T05:00").count(), 1);
        assert_eq!(parse_analytics(&text, "").count(), 2);
    }
}
