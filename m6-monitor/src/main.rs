//! `m6-monitor`: one page that says how the fleet is.
//!
//! Runs at origin, polls every node's `/health` and `/perf` over the backbone,
//! and serves the digest. Two routes:
//!
//!   GET /          the page
//!   GET /digest    the same thing as JSON, for anything that is not a person
//!
//! It is an ordinary m6 service. It links `m6-core` and nothing else, gets its
//! server loop, routing, templating and config from there, and is configured
//! the way every other m6 service is. The interesting part is `digest.rs`,
//! which is the only module here allowed to hold an opinion about what a
//! number means.
//!
//! # Why this is not a script over ssh
//!
//! It used to be. `/health` and `/perf` are HTTP endpoints and the analytics
//! stream is a file m6 writes, so the data was always reachable without a
//! shell; what was missing was the host's own numbers, which are now on
//! `/perf`. Polling over the backbone rather than shelling in means the
//! monitor needs no credentials on the boxes, measures the path it actually
//! cares about, and fails the way a service fails rather than the way a script
//! does.

mod digest;
mod fleet;
mod poll;

use std::time::Duration;

use m6_core::prelude::*;

fn main() -> anyhow::Result<()> {
    App::new()
        .route_get("/", page)
        .route_get("/digest", digest_json)
        .run()?;
    Ok(())
}

/// Poll the fleet and build a digest, or explain why not.
fn collect(req: &Request) -> anyhow::Result<digest::Digest> {
    // The fleet lives in this service's own config, so it is reloaded by the
    // same hot reload as everything else.
    let config_path = std::env::args()
        .nth(2)
        .ok_or_else(|| anyhow::anyhow!("no config path in argv"))?;
    let fleet = fleet::Fleet::from_config(std::path::Path::new(&config_path))?;
    let token = fleet.perf_token();
    let readings = poll::fleet(
        &fleet.nodes,
        token.as_deref(),
        Duration::from_millis(fleet.timeout_ms),
    );
    let _ = req;
    Ok(digest::build(
        &readings,
        &digest::Thresholds::default(),
        now_iso8601(),
    ))
}

fn digest_json(req: &Request) -> Result<Response> {
    match collect(req) {
        Ok(d) => Ok(Response::json(serde_json::to_value(&d).unwrap_or(json!({})))),
        // 503 rather than 500: the monitor is up, the fleet view is not.
        Err(e) => Ok(Response::json_status(
            json!({"error": format!("{e:#}")}),
            503,
        )),
    }
}

fn page(req: &Request) -> Result<Response> {
    let d = match collect(req) {
        Ok(d) => d,
        Err(e) => {
            return Ok(Response::json_status(json!({"error": format!("{e:#}")}), 503));
        }
    };
    // 503 when something is actually broken, so a check pointed at this page
    // is useful without parsing it.
    let code = if d.level == digest::Level::Fault { 503 } else { 200 };
    Ok(Response::html(render(&d))
        .with_status(code)
        // A monitoring page must never be served from a cache, by us or by
        // anything between us and the reader.
        .header("Cache-Control", "no-store")
        .header("X-Robots-Tag", "noindex, nofollow"))
}

fn dur(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn bytes(n: u64) -> String {
    const U: [&str; 4] = ["B", "K", "M", "G"];
    let mut v = n as f64;
    for (i, u) in U.iter().enumerate() {
        if v < 1024.0 || i == U.len() - 1 {
            return format!("{v:.0}{u}");
        }
        v /= 1024.0;
    }
    unreachable!()
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    // An absent reading is a dash, never a zero. A zero is a measurement.
    v.map(|x| x.to_string()).unwrap_or_else(|| "-".to_string())
}

/// Rendered here rather than through a template on purpose.
///
/// This page has to work when the fleet does not, and a template is one more
/// thing that can be missing, mis-edited or fail to compile at exactly the
/// moment someone needs to read it. It is the same reasoning that keeps
/// `/health` free of a render step.
fn render(d: &digest::Digest) -> String {
    let mut h = String::with_capacity(8192);
    let (tone, word) = match d.level {
        digest::Level::Ok => ("#137547", "ALL CLEAR"),
        digest::Level::Warn => ("#b07d00", "WARNINGS"),
        digest::Level::Fault => ("#b3261e", "FAULTS"),
    };
    h.push_str(&format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>m6 fleet: {word}</title>\
         <style>\
         :root{{color-scheme:light dark}}\
         body{{font:14px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;margin:0;padding:24px;\
         background:Canvas;color:CanvasText}}\
         h1{{font-size:18px;margin:0 0 4px}}\
         .badge{{display:inline-block;padding:2px 10px;border-radius:3px;color:#fff;background:{tone};font-weight:700}}\
         .meta{{opacity:.7;margin:8px 0 20px}}\
         table{{border-collapse:collapse;width:100%;margin-bottom:24px}}\
         th,td{{text-align:right;padding:6px 10px;border-bottom:1px solid rgba(128,128,128,.3);white-space:nowrap}}\
         th:first-child,td:first-child{{text-align:left}}\
         th{{opacity:.7;font-weight:600}}\
         .wrap{{overflow-x:auto}}\
         .f{{padding:8px 12px;border-left:3px solid {tone};margin-bottom:6px;background:rgba(128,128,128,.08)}}\
         .f b{{font-family:inherit}}\
         .warn{{border-left-color:#b07d00}}\
         .dim{{opacity:.55}}\
         </style>\
         <h1>m6 fleet <span class=badge>{word}</span></h1>\
         <div class=meta>{} &middot; {} node(s)</div>",
        d.generated_at,
        d.nodes.len()
    ));

    if d.findings.is_empty() {
        h.push_str("<div class=f>Nothing to report.</div>");
    }
    for f in &d.findings {
        let cls = if f.level == digest::Level::Warn { "f warn" } else { "f" };
        h.push_str(&format!("<div class='{cls}'><b>{}</b> &middot; {}</div>", f.node, f.text));
    }

    h.push_str(
        "<div class=wrap><table><tr>\
         <th>node</th><th>role</th><th>status</th><th>rtt</th>\
         <th>reqs</th><th>hit rate</th><th>p50</th><th>p99</th>\
         <th>load</th><th>mem</th><th>disk</th><th>temp</th>\
         <th>svc up</th><th>host up</th></tr>",
    );
    for n in &d.nodes {
        let load = match (n.load_one, n.cpus) {
            (Some(l), Some(c)) => format!("{l:.2}/{c}"),
            _ => "-".to_string(),
        };
        let mem = match (n.memory_used, n.memory_total_bytes) {
            (Some(u), Some(t)) => format!("{:.0}% of {}", u * 100.0, bytes(t)),
            _ => "-".to_string(),
        };
        let disk = match (n.disk_used, n.disk_total_bytes) {
            (Some(u), Some(t)) => format!("{:.0}% of {}", u * 100.0, bytes(t)),
            _ => "-".to_string(),
        };
        h.push_str(&format!(
            "<tr><td>{}</td><td class=dim>{}</td><td>{}</td><td>{}</td>\
             <td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
             <td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            n.name,
            n.role,
            n.status,
            n.rtt_ms.map(|v| format!("{v:.1}ms")).unwrap_or_else(|| "-".into()),
            opt(n.requests_total),
            n.hit_rate.map(|v| format!("{v:.4}")).unwrap_or_else(|| "-".into()),
            n.hit_p50_ns.map(|v| format!("{v}ns")).unwrap_or_else(|| "-".into()),
            n.hit_p99_ns.map(|v| format!("{v}ns")).unwrap_or_else(|| "-".into()),
            load,
            mem,
            disk,
            n.thermal_max_c.map(|c| format!("{c:.0}C")).unwrap_or_else(|| "-".into()),
            n.uptime_s.map(dur).unwrap_or_else(|| "-".into()),
            n.host_uptime_s.map(dur).unwrap_or_else(|| "-".into()),
        ));
    }
    h.push_str("</table></div>");

    h.push_str("<div class=wrap><table><tr><th>node</th><th>backend pools</th></tr>");
    for n in &d.nodes {
        let pools = if n.pools.is_empty() {
            "<span class=dim>none (a cache node has no socket pools)</span>".to_string()
        } else {
            n.pools
                .iter()
                .map(|(name, a, t)| format!("{name} {a}/{t}"))
                .collect::<Vec<_>>()
                .join(" &middot; ")
        };
        h.push_str(&format!("<tr><td>{}</td><td style='text-align:left'>{}</td></tr>", n.name, pools));
    }
    h.push_str("</table></div>");
    h
}
