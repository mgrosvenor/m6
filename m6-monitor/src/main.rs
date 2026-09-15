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

mod check;
mod digest;
mod fleet;
mod poll;

use std::time::Duration;

use m6_core::prelude::*;

fn main() -> anyhow::Result<()> {
    // `m6-monitor --check <config>` prints the hourly health check and exits,
    // which is what `tools/health-check.py` used to do over ssh. Same code
    // path as the page, so the two cannot disagree.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check") {
        let config = args
            .iter()
            .skip(1)
            .find(|a| !a.starts_with("--"))
            .ok_or_else(|| anyhow::anyhow!("usage: m6-monitor --check <config-path>"))?;
        return run_check(std::path::Path::new(config));
    }

    App::with_global(build_state)
        .route_get("/", page)
        .route_get("/digest", digest_json)
        .route_get("/check", check_text)
        .run()?;
    Ok(())
}

/// How often the background poller refreshes the fleet view.
///
/// Every request used to poll the whole fleet itself, which meant load on the
/// production nodes scaled with how often anyone LOOKED at the monitor, two
/// requests moments apart could disagree because they came from different polls,
/// and a plain `/digest` fetch took 2.9 to 3.6 seconds. A monitoring endpoint
/// should answer instantly from a recent reading.
const DEFAULT_POLL_SECS: u64 = 30;

/// A completed fleet poll, and when it completed.
struct Snapshot {
    digest: digest::Digest,
    readings: Vec<poll::NodeReading>,
    at: std::time::Instant,
    at_utc: String,
}

/// What every route reads. Nothing here polls.
struct MonitorState {
    /// `None` until the first poll completes. Reported as such rather than
    /// served as zeroes: "not measured yet" and "measured as zero" are different
    /// claims and this file already makes that distinction for sample counts.
    latest: std::sync::Arc<std::sync::RwLock<Option<Snapshot>>>,
    interval: Duration,
}

impl MonitorState {
    /// Age of the current snapshot, and whether it is stale enough to distrust.
    ///
    /// Stale at three missed intervals. A monitor serving an old reading without
    /// saying so is worse than one that is plainly down, because it looks like
    /// current information.
    fn age(&self, snap: &Snapshot) -> (f64, bool) {
        let age = snap.at.elapsed().as_secs_f64();
        (age, age > self.interval.as_secs_f64() * 3.0)
    }
}

/// Build the shared state and start the poller.
fn build_state(ctx: &AppContext) -> Result<MonitorState> {
    let interval = Duration::from_secs(
        ctx.config
            .get("poll_interval_s")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_POLL_SECS),
    );
    let latest: std::sync::Arc<std::sync::RwLock<Option<Snapshot>>> =
        std::sync::Arc::new(std::sync::RwLock::new(None));

    // `config_path` from the AppContext rather than argv. The old code read
    // `std::env::args().nth(2)` inside the request handler, which quietly assumed
    // an argument position.
    let path = ctx.config_path.to_path_buf();
    let writer = std::sync::Arc::clone(&latest);
    std::thread::spawn(move || loop {
        match collect_from(&path) {
            Ok((digest, readings)) => {
                let snap = Snapshot {
                    digest,
                    readings,
                    at: std::time::Instant::now(),
                    at_utc: m6_core::util::now_iso8601(),
                };
                match writer.write() {
                    Ok(mut g) => *g = Some(snap),
                    Err(e) => tracing::error!(error = %e, "snapshot lock poisoned"),
                }
            }
            // The previous snapshot is KEPT on failure, and its age keeps
            // growing, which every response reports. Replacing it with an error
            // would throw away the last known good reading; hiding the failure
            // would present a stale one as current.
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "fleet poll failed"),
        }
        std::thread::sleep(interval);
    });

    Ok(MonitorState { latest, interval })
}

/// Poll the fleet, print the check, exit 1 on faults.
fn run_check(config_path: &std::path::Path) -> anyhow::Result<()> {
    let (d, readings) = collect_from(config_path)?;
    print!("{}", check::render(&d, &readings));
    if d.level == digest::Level::Fault {
        std::process::exit(1);
    }
    Ok(())
}

/// Poll the fleet and build a digest, or explain why not.
fn collect_from(
    config_path: &std::path::Path,
) -> anyhow::Result<(digest::Digest, Vec<poll::NodeReading>)> {
    let fleet = fleet::Fleet::from_config(config_path)?;
    let token = fleet.perf_token();
    let readings = poll::fleet(
        &fleet.nodes,
        token.as_deref(),
        Duration::from_millis(fleet.timeout_ms),
        &fleet.header_check,
    );
    let d = digest::build(&readings, &digest::Thresholds::default(), now_iso8601());
    Ok((d, readings))
}

/// The digest from the latest snapshot, for the HTML page.
///
/// Clones the digest because the page holds it across the render while the lock
/// must not be. A digest is small; blocking the poller for the length of an HTML
/// build would not be.
fn snapshot_digest(st: &MonitorState) -> anyhow::Result<digest::Digest> {
    let guard = st
        .latest
        .read()
        .map_err(|e| anyhow::anyhow!("snapshot unreadable: {e}"))?;
    let snap = guard
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no fleet poll has completed yet"))?;
    Ok(snap.digest.clone())
}

/// The whole A-to-G report, as text, from the latest snapshot.
///
/// This is what `--check` printed, and the only reason that flag existed: the
/// report had no route out. With this it is reachable the way everything else is,
/// over an ssh-forwarded socket:
///
/// ```text
/// ssh -fN -L /tmp/m6mon.sock:/run/m6/m6-monitor.sock root@<build-host>
/// curl --unix-socket /tmp/m6mon.sock http://localhost/check
/// ```
fn check_text(_req: &Request, st: &MonitorState) -> Result<Response> {
    let guard = match st.latest.read() {
        Ok(g) => g,
        Err(e) => return Ok(Response::text(&format!("snapshot unreadable: {e}")).with_status(503)),
    };
    let Some(snap) = guard.as_ref() else {
        return Ok(Response::text("no fleet poll has completed yet\n").with_status(503));
    };
    let (age, stale) = st.age(snap);
    let mut out = String::new();
    // The age goes FIRST, before any figure, so it cannot be read past.
    if stale {
        out.push_str(&format!(
            "STALE: this reading is {age:.0}s old and the poll interval is {}s. \
             The poller is failing; the figures below are not current.\n\n",
            st.interval.as_secs()
        ));
    } else {
        out.push_str(&format!("polled {age:.1}s ago ({})\n", snap.at_utc));
    }
    out.push_str(&check::render(&snap.digest, &snap.readings));
    Ok(Response::text(&out))
}

fn digest_json(_req: &Request, st: &MonitorState) -> Result<Response> {
    let guard = match st.latest.read() {
        Ok(g) => g,
        // 503 rather than 500: the monitor is up, the fleet view is not.
        Err(e) => {
            return Ok(Response::json_status(
                json!({"error": format!("snapshot unreadable: {e}")}),
                503,
            ))
        }
    };
    let Some(snap) = guard.as_ref() else {
        return Ok(Response::json_status(
            json!({"error": "no fleet poll has completed yet"}),
            503,
        ));
    };
    let (age, stale) = st.age(snap);
    let mut v = serde_json::to_value(&snap.digest).unwrap_or(json!({}));
    // Age is part of the reading, not metadata. A consumer that cannot tell a
    // 3-second-old digest from a 40-minute-old one is guessing.
    if let Some(obj) = v.as_object_mut() {
        obj.insert("snapshot_age_s".into(), json!((age * 10.0).round() / 10.0));
        obj.insert("snapshot_at".into(), json!(snap.at_utc));
        obj.insert("poll_interval_s".into(), json!(st.interval.as_secs()));
        obj.insert("stale".into(), json!(stale));
    }
    Ok(Response::json(v))
}

fn page(_req: &Request, st: &MonitorState) -> Result<Response> {
    let d = match snapshot_digest(st) {
        Ok(d) => d,
        Err(e) => {
            return Ok(Response::json_status(
                json!({"error": format!("{e:#}")}),
                503,
            ));
        }
    };
    // 503 when something is actually broken, so a check pointed at this page
    // is useful without parsing it.
    let code = if d.level == digest::Level::Fault {
        503
    } else {
        200
    };
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
        let cls = if f.level == digest::Level::Warn {
            "f warn"
        } else {
            "f"
        };
        h.push_str(&format!(
            "<div class='{cls}'><b>{}</b> &middot; {}</div>",
            f.node, f.text
        ));
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
            n.rtt_ms
                .map(|v| format!("{v:.1}ms"))
                .unwrap_or_else(|| "-".into()),
            opt(n.requests_total),
            n.hit_rate
                .map(|v| format!("{v:.4}"))
                .unwrap_or_else(|| "-".into()),
            n.hit_p50_ns
                .map(|v| format!("{v}ns"))
                .unwrap_or_else(|| "-".into()),
            n.hit_p99_ns
                .map(|v| format!("{v}ns"))
                .unwrap_or_else(|| "-".into()),
            load,
            mem,
            disk,
            n.thermal_max_c
                .map(|c| format!("{c:.0}C"))
                .unwrap_or_else(|| "-".into()),
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
        h.push_str(&format!(
            "<tr><td>{}</td><td style='text-align:left'>{}</td></tr>",
            n.name, pools
        ));
    }
    h.push_str("</table></div>");
    h
}
