use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

// ── Periodic-stats history ────────────────────────────────────────────────────

pub fn perf_blob(log_path: &Path, n: usize) -> Value {
    let history = read_periodic_stats(log_path, n);
    json!({ "history": history })
}

fn read_periodic_stats(log_path: &Path, n: usize) -> Vec<Value> {
    let text = match fs::read_to_string(log_path) {
        Ok(t) => t,
        Err(_) => return vec![],
    };

    let mut entries: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v["fields"]["message"].as_str() == Some("periodic stats"))
        .map(|v| {
            let f = &v["fields"];
            json!({
                "ts":             v["timestamp"],
                "requests":       f["requests"],
                "rps_avg":        f["rps_avg"],
                "rps_peak":       f["rps_peak"],
                "cache_hits":     f["cache_hits"],
                "cache_misses":   f["cache_misses"],
                "cache_hit_rate": f["cache_hit_rate"].as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(f["cache_hit_rate"].as_f64().unwrap_or(0.0)),
                "backend_errors": f["backend_errors"],
                "pool_members":   f["pool_members"],
                "hit_p0_ns":      f["hit_p0_ns"],
                "hit_p50_ns":     f["hit_p50_ns"],
                "hit_p99_ns":     f["hit_p99_ns"],
                "hit_max_ns":     f["hit_max_ns"],
                "miss_p0_ns":     f["miss_p0_ns"],
                "miss_p50_ns":    f["miss_p50_ns"],
                "miss_p99_ns":    f["miss_p99_ns"],
                "miss_max_ns":    f["miss_max_ns"],
            })
        })
        .collect();

    if entries.len() > n {
        entries = entries.split_off(entries.len() - n);
    }
    entries
}

// ── Per-route stats ───────────────────────────────────────────────────────────

pub fn routes_blob(log_path: &Path, sample: usize) -> Value {
    let entries = load_request_complete(log_path, Some(sample), None);
    aggregate_routes(&entries)
}

// ── Per-backend stats ─────────────────────────────────────────────────────────

/// Summary of all backends — used for the topology diagram.
/// Returns site config metadata plus per-backend stats from the last `sample_n` requests.
pub fn backends_summary_blob(
    log_path: &Path,
    site_toml: &Path,
    system_toml: &Path,
    sample_n: usize,
) -> Value {
    // ── Parse site.toml for topology metadata ──────────────────────────────
    let (site_name, backend_route_map) = parse_site_topology(site_toml);
    let bind = crate::server_bind(system_toml);

    // Build reverse map: path → backend name
    let mut path_to_backend: HashMap<String, String> = HashMap::new();
    for (backend, paths) in &backend_route_map {
        for p in paths {
            path_to_backend.insert(p.clone(), backend.clone());
        }
    }

    // ── Partition request-complete log entries by backend ─────────────────
    let entries = load_request_complete(log_path, Some(sample_n), None);

    let mut by_backend: HashMap<String, Vec<&Value>> = HashMap::new();
    for backend in backend_route_map.keys() {
        by_backend.entry(backend.clone()).or_default();
    }
    for entry in &entries {
        let path = entry["fields"]["path"].as_str().unwrap_or("");
        if let Some(backend) = path_to_backend.get(path) {
            by_backend.entry(backend.clone()).or_default().push(entry);
        }
    }

    // ── Build per-backend summary, preserving site.toml declaration order ─
    let mut backend_names: Vec<String> = backend_route_map.keys().cloned().collect();
    backend_names.sort(); // deterministic order

    let backends: Vec<Value> = backend_names
        .iter()
        .map(|name| {
            let ents = by_backend.get(name).map(|v| v.as_slice()).unwrap_or(&[]);
            let s = compute_stats(ents);
            let routes = backend_route_map.get(name).cloned().unwrap_or_default();
            json!({
                "name":           name,
                "routes":         routes,          // paths handled by this backend
                "requests":       s.requests,
                "cache_hits":     s.cache_hits,
                "cache_misses":   s.cache_misses,
                "hit_rate":       s.hit_rate,
                "miss_rate":      s.miss_rate,
                "hit_p50_ns":     s.hit_pcts[3],   // index 3 = p50
                "avg_latency_ns": s.avg_latency_ns,
            })
        })
        .collect();

    let window_secs = ts_diff_secs(
        entries.first().and_then(|v| v["timestamp"].as_str()),
        entries.last().and_then(|v| v["timestamp"].as_str()),
    );

    // Most recent periodic-stats entry from m6-http — authoritative cache/latency totals.
    let m6http_stats = read_periodic_stats(log_path, 1)
        .into_iter()
        .next()
        .unwrap_or(Value::Null);

    json!({
        "site_name":          site_name,
        "bind":               bind,
        "backends":           backends,
        "sample_n":           entries.len(),
        "sample_window_secs": window_secs,
        "m6http_stats":       m6http_stats,
    })
}

/// Parse site.toml and return (site_name, backend→[paths] map).
fn parse_site_topology(site_toml: &Path) -> (String, HashMap<String, Vec<String>>) {
    let empty = ("m6-http".to_string(), HashMap::new());
    let text = match fs::read_to_string(site_toml) {
        Ok(t) => t,
        Err(_) => return empty,
    };
    let parsed: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(_) => return empty,
    };

    let site_name = parsed
        .get("site")
        .and_then(|s| s.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("m6-http")
        .to_string();

    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    // Collect all declared backend names (preserves backends with zero traffic).
    if let Some(backends) = parsed.get("backend").and_then(|b| b.as_array()) {
        for b in backends {
            if let Some(name) = b.get("name").and_then(|n| n.as_str()) {
                map.entry(name.to_string()).or_default();
            }
        }
    }

    // Map routes → backends.
    if let Some(routes) = parsed.get("route").and_then(|r| r.as_array()) {
        for route in routes {
            let path = route
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            let backend = route
                .get("backend")
                .and_then(|b| b.as_str())
                .unwrap_or("")
                .to_string();
            if !path.is_empty() && !backend.is_empty() {
                map.entry(backend).or_default().push(path);
            }
        }
    }

    (site_name, map)
}

/// Aggregate stats for m6-http itself (all paths, no backend filter).
pub fn m6http_stats_blob(log_path: &Path, limit: usize) -> Value {
    let cap = limit.min(100_000);
    let entries = load_request_complete(log_path, Some(cap), None);
    build_stats_blob("m6-http", &entries)
}

/// Detailed stats for a single backend, limited to the last `limit` matching entries.
/// Pass `limit = usize::MAX` for "all-time" (capped at 100_000 for performance).
pub fn backend_stats_blob(
    log_path: &Path,
    site_toml: &Path,
    backend_name: &str,
    limit: usize,
) -> Value {
    let (_, backend_paths) = parse_site_topology(site_toml);
    let paths = match backend_paths.get(backend_name) {
        Some(p) => p.clone(),
        None => return json!({ "error": format!("unknown backend: {backend_name}") }),
    };

    let cap = limit.min(100_000);
    let entries = load_request_complete(log_path, Some(cap), Some(&paths));
    build_stats_blob(backend_name, &entries)
}

fn build_stats_blob(name: &str, entries: &[Value]) -> Value {
    let s = compute_stats(&entries.iter().collect::<Vec<_>>());

    let mut by_path: HashMap<String, RouteStats> = HashMap::new();
    for v in entries {
        let f = &v["fields"];
        let path = f["path"].as_str().unwrap_or("/").to_string();
        let cache_hit = f["cache_hit"].as_bool().unwrap_or(false);
        let latency = f["latency_ns"].as_u64().unwrap_or(0);
        let rs = by_path.entry(path).or_default();
        rs.requests += 1;
        rs.latency_sum += latency;
        if cache_hit {
            rs.cache_hits += 1;
        }
    }
    let mut routes: Vec<Value> = by_path
        .into_iter()
        .map(|(path, rs)| {
            let hit_rate = if rs.requests > 0 {
                rs.cache_hits as f64 / rs.requests as f64
            } else {
                0.0
            };
            let avg_lat = rs.latency_sum.checked_div(rs.requests).unwrap_or(0);
            json!({
                "path":           path,
                "requests":       rs.requests,
                "cache_hits":     rs.cache_hits,
                "cache_misses":   rs.requests - rs.cache_hits,
                "hit_rate":       (hit_rate * 1000.0).round() / 1000.0,
                "avg_latency_ns": avg_lat,
            })
        })
        .collect();
    routes.sort_by(|a, b| {
        b["requests"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["requests"].as_u64().unwrap_or(0))
    });

    let window_secs = ts_diff_secs(
        entries.first().and_then(|v| v["timestamp"].as_str()),
        entries.last().and_then(|v| v["timestamp"].as_str()),
    );

    json!({
        "backend":            name,
        "requests":           s.requests,
        "cache_hits":         s.cache_hits,
        "cache_misses":       s.cache_misses,
        "hit_rate":           s.hit_rate,
        "miss_rate":          s.miss_rate,
        "avg_latency_ns":     s.avg_latency_ns,
        "hit_latency_pcts":   s.hit_pcts,
        "miss_latency_pcts":  s.miss_pcts,
        "pct_labels":         ["p0","p10","p25","p50","p75","p90","p99","max"],
        "sample_requests":    entries.len(),
        "sample_window_secs": window_secs,
        "routes":             routes,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct RouteStats {
    requests: u64,
    cache_hits: u64,
    latency_sum: u64,
}

struct AggStats {
    requests: u64,
    cache_hits: u64,
    cache_misses: u64,
    hit_rate: f64,
    miss_rate: f64,
    avg_latency_ns: u64,
    hit_pcts: [u64; 8], // p0,p10,p25,p50,p75,p90,p99,max
    miss_pcts: [u64; 8],
}

fn compute_stats(entries: &[&Value]) -> AggStats {
    let mut requests = 0u64;
    let mut cache_hits = 0u64;
    let mut latency_sum = 0u64;
    let mut hit_lats: Vec<u64> = Vec::new();
    let mut miss_lats: Vec<u64> = Vec::new();

    for v in entries {
        let f = &v["fields"];
        let cache_hit = f["cache_hit"].as_bool().unwrap_or(false);
        let latency = f["latency_ns"].as_u64().unwrap_or(0);
        requests += 1;
        latency_sum += latency;
        if cache_hit {
            cache_hits += 1;
            if latency > 0 {
                hit_lats.push(latency);
            }
        } else {
            if latency > 0 {
                miss_lats.push(latency);
            }
        }
    }

    let cache_misses = requests.saturating_sub(cache_hits);
    let hit_rate = if requests > 0 {
        cache_hits as f64 / requests as f64
    } else {
        0.0
    };
    let miss_rate = if requests > 0 {
        cache_misses as f64 / requests as f64
    } else {
        0.0
    };
    let avg_latency_ns = latency_sum.checked_div(requests).unwrap_or(0);

    hit_lats.sort_unstable();
    miss_lats.sort_unstable();

    AggStats {
        requests,
        cache_hits,
        cache_misses,
        hit_rate: (hit_rate * 1000.0).round() / 1000.0,
        miss_rate: (miss_rate * 1000.0).round() / 1000.0,
        avg_latency_ns,
        hit_pcts: eight_pcts(&hit_lats),
        miss_pcts: eight_pcts(&miss_lats),
    }
}

/// Compute [p0,p10,p25,p50,p75,p90,p99,max] from a sorted slice.
fn eight_pcts(sorted: &[u64]) -> [u64; 8] {
    if sorted.is_empty() {
        return [0; 8];
    }
    let n = sorted.len() - 1;
    let p = |pct: usize| sorted[n * pct / 100];
    [
        p(0),
        p(10),
        p(25),
        p(50),
        p(75),
        p(90),
        p(99),
        sorted[sorted.len() - 1],
    ]
}

/// Read "request complete" entries from the log.
/// - `limit`: keep at most this many (most recent first is preserved; None = no cap).
/// - `path_filter`: if Some, only include entries whose path is in the set.
fn load_request_complete(
    log_path: &Path,
    limit: Option<usize>,
    path_filter: Option<&[String]>,
) -> Vec<Value> {
    let text = match fs::read_to_string(log_path) {
        Ok(t) => t,
        Err(_) => return vec![],
    };

    let filter_set: Option<std::collections::HashSet<&str>> =
        path_filter.map(|paths| paths.iter().map(|s| s.as_str()).collect());

    let all: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| {
            let msg = v["fields"]["message"].as_str().unwrap_or("");
            msg == "request complete" || msg.starts_with("request complete")
        })
        .filter(|v| {
            if let Some(ref set) = filter_set {
                let path = v["fields"]["path"].as_str().unwrap_or("");
                set.contains(path)
            } else {
                true
            }
        })
        .collect();

    if let Some(n) = limit {
        if all.len() > n {
            return all[all.len() - n..].to_vec();
        }
    }
    all
}

fn aggregate_routes(entries: &[Value]) -> Value {
    let sample_requests = entries.len();
    let first_ts = entries.first().and_then(|v| v["timestamp"].as_str());
    let last_ts = entries.last().and_then(|v| v["timestamp"].as_str());
    let window_secs = ts_diff_secs(first_ts, last_ts);

    struct Rs {
        requests: u64,
        cache_hits: u64,
        latency_sum: u64,
    }
    let mut by_path: HashMap<String, Rs> = HashMap::new();

    for v in entries {
        let f = &v["fields"];
        let path = f["path"].as_str().unwrap_or("/").to_string();
        let cache_hit = f["cache_hit"].as_bool().unwrap_or(false);
        let latency = f["latency_ns"].as_u64().unwrap_or(0);
        let s = by_path.entry(path).or_insert(Rs {
            requests: 0,
            cache_hits: 0,
            latency_sum: 0,
        });
        s.requests += 1;
        s.latency_sum += latency;
        if cache_hit {
            s.cache_hits += 1;
        }
    }

    let mut routes: Vec<Value> = by_path
        .into_iter()
        .map(|(path, s)| {
            let misses = s.requests - s.cache_hits;
            let hit_rate = if s.requests > 0 {
                s.cache_hits as f64 / s.requests as f64
            } else {
                0.0
            };
            let avg_lat = s.latency_sum.checked_div(s.requests).unwrap_or(0);
            json!({
                "path":           path,
                "requests":       s.requests,
                "cache_hits":     s.cache_hits,
                "cache_misses":   misses,
                "hit_rate":       (hit_rate * 1000.0).round() / 1000.0,
                "avg_latency_ns": avg_lat,
            })
        })
        .collect();

    routes.sort_by(|a, b| {
        b["requests"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["requests"].as_u64().unwrap_or(0))
    });
    json!({ "sample_requests": sample_requests, "sample_window_secs": window_secs, "routes": routes })
}

/// Parse two RFC-3339-ish timestamps and return their difference in seconds.
fn ts_diff_secs(first: Option<&str>, last: Option<&str>) -> Option<u64> {
    let parse = |s: &str| -> Option<u64> {
        let s = s.trim_end_matches('Z').get(..19)?;
        let parts: Vec<&str> = s.splitn(2, 'T').collect();
        if parts.len() != 2 {
            return None;
        }
        let date: Vec<u32> = parts[0]
            .splitn(3, '-')
            .filter_map(|x| x.parse().ok())
            .collect();
        let time: Vec<u32> = parts[1]
            .splitn(3, ':')
            .filter_map(|x| x.parse().ok())
            .collect();
        if date.len() < 3 || time.len() < 3 {
            return None;
        }
        let y = date[0] as u64;
        let mo = date[1] as u64;
        let d = date[2] as u64;
        let h = time[0] as u64;
        let m = time[1] as u64;
        let s_ = time[2] as u64;
        let years = y.saturating_sub(1970);
        let days = years * 365 + years / 4 + (mo.saturating_sub(1)) * 30 + d;
        Some(days * 86400 + h * 3600 + m * 60 + s_)
    };
    let t0 = parse(first?)?;
    let t1 = parse(last?)?;
    Some(t1.saturating_sub(t0))
}
