//! m6-http's side of the monitoring endpoints.
//!
//! The endpoints themselves, and the shape of what they publish, are
//! `m6_core::monitoring`. They are an m6 feature rather than this binary's:
//! every deployment serves them, and an aggregator that reads them needs the
//! same types the server writes. One definition, shared, rather than a
//! consumer re-deriving the format by looking at a sample.
//!
//! What remains here is the part that is genuinely about this binary: which
//! backend name a monitoring response is tagged with, and keeping those
//! responses out of the site traffic counters.

use std::net::IpAddr;

use crate::auth::{self, PublicKey, Refusal};
use crate::config::HealthConfig;

pub use m6_core::monitoring::{
    metrics_authorised, HealthReport, PerfAccess, PerfOutcome, PerfReport, PerfSubject, PoolHealth,
};

/// Backend name tagged onto a `/health` response.
pub const HEALTH_BACKEND: &str = "health";
/// Backend name tagged onto a `/perf` response.
pub const PERF_BACKEND: &str = "perf";

/// Whether a completed response came from the monitoring endpoints.
///
/// These must not be counted as site traffic. Both answer from local state
/// and return `Ready`, so the cache-miss accounting on the serving paths
/// would otherwise record every health check as a cache miss -- inflating
/// requests_total with monitor polling, depressing the hit rate, and
/// poisoning the miss latency percentiles with ~10us samples that never
/// touched a backend.
///
/// That matters most for the signal the endpoint exists to give: if a
/// 30-second monitor contributes to requests_total, then "the request count
/// stopped moving" can no longer detect a traffic stall, because the monitor
/// keeps it moving on its own.
pub fn is_monitoring_endpoint(backend: &str) -> bool {
    backend == HEALTH_BACKEND || backend == PERF_BACKEND
}

#[cfg(test)]
mod token_file_tests {
    use crate::config::HealthConfig;

    /// `--dump-config` serialises the whole Config. The resolved token must
    /// never appear in that output, or the secret ends up in any log or
    /// paste of a config dump.
    #[test]
    fn resolved_token_is_never_serialised() {
        let health = HealthConfig {
            enabled: true,
            path: "/health".to_string(),
            perf_path: "/perf".to_string(),
            traffic_path: "/traffic".to_string(),
            traffic_cache_s: 60,
            metrics_token_file: Some("/etc/m6/perf-token".to_string()),
            metrics_token: Some("super-secret-value".to_string()),
            metrics_require: None,
            metrics_peers: None,
            metrics_peer_ips: Vec::new(),
        };
        let dumped = serde_json::to_string(&health).expect("serialisable");
        assert!(
            !dumped.contains("super-secret-value"),
            "token leaked into serialised config: {dumped}"
        );
        // The path is configuration and should still be visible.
        assert!(dumped.contains("/etc/m6/perf-token"));
    }

    /// The file indirection has to be enforced, not advisory: a token written
    /// inline in site.toml must not be honoured, because site.toml is
    /// committed and shipped to every node.
    #[test]
    fn inline_token_in_toml_is_ignored() {
        let parsed: HealthConfig =
            toml::from_str("metrics_token = \"inline-secret\"").expect("parses");
        assert_eq!(
            parsed.metrics_token, None,
            "an inline token must not be accepted from config"
        );
    }
}

#[cfg(test)]
mod monitoring_exclusion_tests {
    use super::*;

    /// Regression guard. The cache-miss accounting on the serving paths fires
    /// on any `Ready` outcome, and /health and /perf both return Ready. Left
    /// uncounted for, every health check was recorded as a cache miss: 10
    /// checks moved requests_total by 11 on the live origin.
    ///
    /// That defeats the counter's purpose. A monitor polling every 30s would
    /// keep requests_total rising through a total traffic stall, so "the
    /// number stopped moving" would never fire.
    #[test]
    fn monitoring_endpoints_are_excluded_from_traffic_stats() {
        assert!(is_monitoring_endpoint(HEALTH_BACKEND));
        assert!(is_monitoring_endpoint(PERF_BACKEND));
        assert!(is_monitoring_endpoint("health"));
        assert!(is_monitoring_endpoint("perf"));
    }

    /// Real backends must still be counted. An over-broad match here would
    /// silently stop recording actual site traffic, which is a worse failure
    /// than the one above and much harder to notice.
    #[test]
    fn real_backends_are_still_counted() {
        for backend in [
            "cache",
            "m6-html",
            "m6-file",
            "render-contact",
            "render-analytics",
            "origin",
            "method-check",
            "",
        ] {
            assert!(
                !is_monitoring_endpoint(backend),
                "{backend} is real traffic and must be counted"
            );
        }
    }
}

// ── /traffic ─────────────────────────────────────────────────────────────────

use std::sync::Mutex;
use std::time::{Duration, Instant};

use m6_core::monitoring::TrafficReport;

/// Last summary and when it was built.
///
/// One node, one cache. Behind a Mutex because building a summary reads a file
/// and there is no reason to let two scrapes do it at once; the lock is held
/// across the read deliberately, so a second caller waits for the first
/// answer rather than starting a second 47MB tail.
static CACHE: Mutex<Option<(Instant, TrafficReport)>> = Mutex::new(None);

/// Where the privileged collector leaves `nft -j list ruleset` output.
const FIREWALL_JSON: &str = "/var/lib/m6/firewall.json";

/// Read the last `window_minutes` of the analytics log and summarise it.
///
/// The newest rotated file is read as well, so the window survives a rotation.
/// It is read only when the live tail did not fill the budget and the file was written inside the window, and a gzip file only when it is small (see `m6_core::log::read_recent_counted`).
/// This runs inline on the event loop, so the work is bounded.
///
/// The tail is bounded rather than reading the whole file: an hour of traffic
/// is at most a few hundred KB and the file is tens of MB, so reading it all
/// would be almost entirely wasted work. 24MB is a wide margin over the
/// busiest hour observed (370KB, during a 890-request probe).
fn build_report(node: &str, path: &str, window_minutes: u64) -> anyhow::Result<TrafficReport> {
    const TAIL_BYTES: usize = 24 * 1024 * 1024;

    // The newest rotated file is read before the live one, so the summary
    // spans a rotation. A mid-record start is expected after a cut, and
    // `ndjson::read_str` skips what it cannot parse, so it costs nothing.
    let since_time =
        std::time::SystemTime::now() - Duration::from_secs(window_minutes.saturating_mul(60));
    let buf = m6_core::log::read_recent(std::path::Path::new(path), TAIL_BYTES, since_time)?;

    let since = m6_core::util::iso8601_minutes_ago(window_minutes);
    let mut report = TrafficReport::build(node, &buf, &since, window_minutes);
    // Written by the m6-firewall-stats timer. A node without the timer
    // reports no firewall state rather than an empty one.
    report.firewall =
        m6_core::firewall::FirewallState::from_file(std::path::Path::new(FIREWALL_JSON))
            .unwrap_or(None);
    Ok(report)
}

/// Serve `/traffic`: this node's summary of its own traffic.
///
/// Same token as `/perf`, and the same order of operations: authorise first,
/// then work. An anonymous caller never causes a file read.
///
/// The cache is what makes a file read safe to expose at all. Without it a
/// monitor polling every thirty seconds makes every node re-read its analytics
/// tail every thirty seconds, forever, to produce an answer that changes
/// slowly. With it the read happens at most once per TTL however often the
/// endpoint is scraped. The lock is held across the read deliberately: a
/// second caller arriving mid-read waits for the first answer rather than
/// starting a second tail.
pub fn traffic(
    node: &str,
    log_path: &str,
    window_minutes: u64,
    cache_for: Duration,
    headers: &[(String, String)],
    configured_token: Option<&str>,
) -> PerfOutcome {
    if let Some(refused) = (PerfAccess {
        headers,
        configured_token,
    })
    .refusal()
    {
        return refused;
    }
    traffic_authorised(node, log_path, window_minutes, cache_for)
}

/// The `/traffic` summary for a caller some gate has already authorised.
///
/// Everything `traffic` does after its token check: the cached summary, or one
/// bounded read of the analytics tail.
pub fn traffic_authorised(
    node: &str,
    log_path: &str,
    window_minutes: u64,
    cache_for: Duration,
) -> PerfOutcome {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((built, report)) = cache.as_ref() {
        if built.elapsed() < cache_for {
            return PerfOutcome::Traffic(Box::new(report.clone()));
        }
    }
    match build_report(node, log_path, window_minutes) {
        Ok(r) => {
            *cache = Some((Instant::now(), r.clone()));
            PerfOutcome::Traffic(Box::new(r))
        }
        Err(e) => PerfOutcome::TrafficError(format!("{e}")),
    }
}

/// The verdict of the metrics gate for one request.
pub enum Gate {
    /// Serve the report.
    Open,
    /// Answer with this and do nothing else.
    Refused(PerfOutcome),
}

/// Decide whether `/perf` or `/traffic` may be served to this request.
///
/// Two rules, in this order:
///
/// 1. `metrics_peers`, when set: the request must not have arrived on an
///    internal h2c listener (there the client address is a forwarded claim),
///    and its peer address must be listed. Otherwise `Disabled`, the 404 an
///    unconfigured endpoint gives, before any token is looked at.
/// 2. `metrics_require`, when set: an API token that passes the claim check
///    (`Unauthorised` for none or a bad one, `Forbidden` for the wrong claim).
///    When unset, the legacy `metrics_token_file` gate.
///
/// The address rule allocates nothing: one parse of the peer address and a
/// scan of a short slice, and only on these two paths.
pub fn metrics_gate(
    h: &HealthConfig,
    key: Option<&PublicKey>,
    headers: &[(String, String)],
    client_ip: &str,
    from_internal: bool,
) -> Gate {
    if h.metrics_peers.is_some() {
        let listed = !from_internal
            && client_ip
                .parse::<IpAddr>()
                .is_ok_and(|ip| h.metrics_peer_ips.contains(&ip.to_canonical()));
        if !listed {
            return Gate::Refused(PerfOutcome::Disabled);
        }
    }
    match h.metrics_require.as_deref() {
        Some(require) => {
            let Some(key) = key else {
                return Gate::Refused(PerfOutcome::Unauthorised);
            };
            match auth::authorise(key, headers, require) {
                Ok(_) => Gate::Open,
                Err(Refusal::Insufficient) => Gate::Refused(PerfOutcome::Forbidden),
                Err(Refusal::NoToken | Refusal::Invalid(_)) => {
                    Gate::Refused(PerfOutcome::Unauthorised)
                }
            }
        }
        None => match (PerfAccess {
            headers,
            configured_token: h.metrics_token.as_deref(),
        })
        .refusal()
        {
            Some(refused) => Gate::Refused(refused),
            None => Gate::Open,
        },
    }
}

#[cfg(test)]
mod traffic_endpoint_tests {
    use super::*;
    use std::io::Write;

    fn row(ts: &str, ip: &str, path: &str, status: u16, ua: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","level":"INFO","fields":{{"message":"request","node":"origin","path":"{path}","status":{status},"client_ip":"{ip}","user_agent":"{ua}"}}}}"#
        )
    }

    fn auth(v: &str) -> Vec<(String, String)> {
        vec![("Authorization".to_string(), v.to_string())]
    }

    /// Authorisation happens before any file is opened. An anonymous caller
    /// must not be able to make a node read its analytics log, which is the
    /// whole reason the expensive endpoints are gated.
    #[test]
    fn an_unauthorised_caller_never_reaches_the_log() {
        let out = traffic(
            "origin",
            "/definitely/not/a/file",
            60,
            Duration::from_secs(60),
            &auth("Bearer wrong"),
            Some("right"),
        );
        // Unauthorised, not TrafficError: it never tried to open the path.
        assert!(matches!(out, PerfOutcome::Unauthorised));
    }

    #[test]
    fn no_token_configured_means_the_endpoint_does_not_exist() {
        let out = traffic("origin", "/nope", 60, Duration::from_secs(60), &[], None);
        assert!(matches!(out, PerfOutcome::Disabled));
        let (code, _, _) = out.into_response();
        assert_eq!(code, 404, "404 not 401: it must not advertise a door");
    }

    /// An unreadable log is reported, not silently served as a quiet hour.
    /// This codebase has made that mistake: an absent file at the expected
    /// path looked exactly like a feature switched off.
    #[test]
    fn an_unreadable_log_is_a_503_with_a_reason() {
        let out = traffic(
            "origin",
            "/definitely/not/a/file",
            60,
            Duration::from_secs(0),
            &auth("Bearer tok"),
            Some("tok"),
        );
        let (code, _, body) = out.into_response();
        assert_eq!(code, 503);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(!v["error"].as_str().unwrap_or("").is_empty());
    }

    #[test]
    fn summarises_a_real_log_and_serves_it() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let now = m6_core::util::now_iso8601();
        let ts = now.trim_end_matches('Z');
        writeln!(
            f,
            "{}",
            row(&format!("{ts}.100Z"), "1.2.3.4", "/", 200, "Chrome/131")
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            row(
                &format!("{ts}.200Z"),
                "5.6.7.8",
                "/robots.txt",
                200,
                "Mozilla/5.0 (compatible; ClaudeBot/1.0; +claudebot@anthropic.com)"
            )
        )
        .unwrap();
        f.flush().unwrap();

        let out = traffic(
            "origin",
            f.path().to_str().unwrap(),
            60,
            Duration::from_secs(0),
            &auth("Bearer tok"),
            Some("tok"),
        );
        let (code, headers, body) = out.into_response();
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["node"], "origin");
        assert_eq!(v["total_requests"], 2);
        assert!(v["crawlers"][0]["user_agent"]
            .as_str()
            .unwrap()
            .contains("ClaudeBot"));
        assert!(v["logging"].is_object(), "logging health travels with it");
        // Never cached by anything in between.
        assert!(headers
            .iter()
            .any(|(k, val)| k.eq_ignore_ascii_case("cache-control") && val == "no-store"));
    }

    /// After an hourly rotation the live file is nearly empty, and the
    /// monitor's sixty minute sections must not shrink with it.
    #[test]
    fn the_window_spans_the_newest_rotated_file() {
        let dir = tempfile::tempdir().unwrap();
        let now = m6_core::util::now_iso8601();
        let ts = now.trim_end_matches('Z');
        let live = dir.path().join("analytics.ndjson");
        std::fs::write(
            &live,
            format!(
                "{}\n",
                row(&format!("{ts}.300Z"), "1.2.3.4", "/c", 200, "x")
            ),
        )
        .unwrap();
        let rotated = format!(
            "{}\n{}\n",
            row(&format!("{ts}.100Z"), "1.2.3.4", "/a", 200, "x"),
            row(&format!("{ts}.200Z"), "1.2.3.4", "/b", 200, "x")
        );
        // Plain here. The gzip path is covered by `m6_core::log`'s tests.
        std::fs::write(dir.path().join("analytics.ndjson-2026010101"), rotated).unwrap();
        let out = traffic(
            "origin",
            live.to_str().unwrap(),
            60,
            Duration::from_secs(0),
            &auth("Bearer tok"),
            Some("tok"),
        );
        let (code, _, body) = out.into_response();
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["total_requests"], 3);
    }
}

#[cfg(test)]
mod metrics_gate_tests {
    use super::*;
    use jsonwebtoken::{Algorithm, EncodingKey, Header};

    fn keypair() -> (tempfile::TempDir, PublicKey, String) {
        let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("ec keypair");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pub.pem");
        std::fs::write(&path, kp.public_key_pem()).unwrap();
        (
            dir,
            PublicKey::from_pem_file(&path).unwrap(),
            kp.serialize_pem(),
        )
    }

    fn token(private_pem: &str, roles: &[&str]) -> Vec<(String, String)> {
        #[derive(serde::Serialize)]
        struct C {
            exp: u64,
            roles: Vec<String>,
        }
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let claims = C {
            exp,
            roles: roles.iter().map(|r| r.to_string()).collect(),
        };
        let key = EncodingKey::from_ec_pem(private_pem.as_bytes()).unwrap();
        let t = jsonwebtoken::encode(&Header::new(Algorithm::ES256), &claims, &key).unwrap();
        vec![("authorization".to_string(), format!("Bearer {t}"))]
    }

    fn health(peers: Option<&[&str]>, require: Option<&str>, legacy: Option<&str>) -> HealthConfig {
        HealthConfig {
            metrics_require: require.map(str::to_string),
            metrics_peers: peers.map(|p| p.iter().map(|s| s.to_string()).collect()),
            metrics_peer_ips: peers
                .unwrap_or(&[])
                .iter()
                .map(|s| s.parse::<IpAddr>().unwrap().to_canonical())
                .collect(),
            metrics_token: legacy.map(str::to_string),
            ..HealthConfig::default()
        }
    }

    fn is_disabled(g: &Gate) -> bool {
        matches!(g, Gate::Refused(PerfOutcome::Disabled))
    }

    #[test]
    fn an_unlisted_peer_is_404_before_any_token_check() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"), None);
        let good = token(&private, &["monitor"]);
        for ip in ["203.0.113.7", "2001:db8::1", "10.0.0.8"] {
            let g = metrics_gate(&h, Some(&key), &good, ip, false);
            assert!(is_disabled(&g), "{ip} must get 404 even with a valid token");
        }
        // No token and an unlisted peer is still 404, not 401: a stranger
        // learns nothing about the credential.
        assert!(is_disabled(&metrics_gate(
            &h,
            Some(&key),
            &[],
            "203.0.113.7",
            false
        )));
    }

    #[test]
    fn a_listed_peer_passes_the_address_rule() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"), None);
        let good = token(&private, &["monitor"]);
        assert!(matches!(
            metrics_gate(&h, Some(&key), &good, "10.0.0.9", false),
            Gate::Open
        ));
        // An IPv4-mapped IPv6 peer is the same host, so the comparison is canonical.
        assert!(matches!(
            metrics_gate(&h, Some(&key), &good, "::ffff:10.0.0.9", false),
            Gate::Open
        ));
    }

    #[test]
    fn an_unparseable_client_address_is_refused() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"), None);
        let good = token(&private, &["monitor"]);
        for ip in ["", "unknown", "10.0.0.9:443", "10.0.0.9, 1.2.3.4"] {
            assert!(
                is_disabled(&metrics_gate(&h, Some(&key), &good, ip, false)),
                "{ip:?}"
            );
        }
    }

    #[test]
    fn a_listed_peer_on_an_internal_listener_is_refused() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"), None);
        let good = token(&private, &["monitor"]);
        assert!(is_disabled(&metrics_gate(
            &h,
            Some(&key),
            &good,
            "10.0.0.9",
            true
        )));
    }

    #[test]
    fn a_listed_peer_with_a_valid_token_but_wrong_role_is_403() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"), None);
        let reader = token(&private, &["reader"]);
        assert!(matches!(
            metrics_gate(&h, Some(&key), &reader, "10.0.0.9", false),
            Gate::Refused(PerfOutcome::Forbidden)
        ));
    }

    #[test]
    fn metrics_require_without_a_loaded_key_fails_closed() {
        let h = health(None, Some("role:monitor"), None);
        assert!(matches!(
            metrics_gate(&h, None, &[], "10.0.0.9", false),
            Gate::Refused(PerfOutcome::Unauthorised)
        ));
    }

    #[test]
    fn metrics_require_refuses_the_legacy_bearer_string() {
        let (_d, key, _) = keypair();
        let h = health(None, Some("role:monitor"), Some("legacy-secret"));
        let legacy = vec![(
            "authorization".to_string(),
            "Bearer legacy-secret".to_string(),
        )];
        assert!(matches!(
            metrics_gate(&h, Some(&key), &legacy, "10.0.0.9", false),
            Gate::Refused(PerfOutcome::Unauthorised)
        ));
    }

    #[test]
    fn without_new_keys_the_gate_is_todays() {
        let h = health(None, None, Some("right"));
        let ok = vec![("authorization".to_string(), "Bearer right".to_string())];
        let bad = vec![("authorization".to_string(), "Bearer wrong".to_string())];
        // Any peer, internal or not: there is no address rule.
        for (ip, internal) in [("203.0.113.7", false), ("10.0.0.9", true)] {
            assert!(matches!(
                metrics_gate(&h, None, &ok, ip, internal),
                Gate::Open
            ));
            assert!(matches!(
                metrics_gate(&h, None, &bad, ip, internal),
                Gate::Refused(PerfOutcome::Unauthorised)
            ));
        }
        let none = health(None, None, None);
        assert!(is_disabled(&metrics_gate(
            &none, None, &ok, "10.0.0.9", false
        )));
    }

    #[test]
    fn traffic_authorised_reads_the_log_without_a_token_check() {
        // The path does not exist, so a 503 proves the read was attempted and
        // no token was asked for.
        let out = traffic_authorised(
            "origin",
            "/definitely/not/a/file/for/the/gate",
            60,
            Duration::from_secs(0),
        );
        assert!(matches!(out, PerfOutcome::TrafficError(_)));
    }
}
