//! `m6-monitor`: one page that says how the fleet is.
//!
//! Runs at origin, polls every node's `/health` and `/perf` over the backbone,
//! and serves the digest. These routes:
//!
//!   GET /          the page
//!   GET /digest    the same thing as JSON, for anything that is not a person
//!   GET /check     the hourly health check, as text
//!   GET /history   the poll history as paged JSON Lines, when `history_file` is set
//!   GET /crawlers  the crawler sightings as paged JSON Lines, likewise
//!
//! Every route answers on the unix socket only. The monitor has no listener and
//! no token code. Access control is the `require` rule of the m6-http placed in
//! front of the socket. See `history.rs` for the history and a front example.
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
mod history;
mod poll;
mod tunnel;

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
        .route_get("/history", history_ndjson)
        .route_get("/crawlers", crawlers_ndjson)
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
    history: history::HistoryHandle,
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
    let interval = Duration::from_secs(fleet::poll_interval_s(|k| {
        ctx.config.get(k).and_then(|v| v.as_u64())
    }));
    let latest: std::sync::Arc<std::sync::RwLock<Option<Snapshot>>> =
        std::sync::Arc::new(std::sync::RwLock::new(None));

    // `config_path` from the AppContext rather than argv. The old code read
    // `std::env::args().nth(2)` inside the request handler, which quietly assumed
    // an argument position.
    let path = ctx.config_path.to_path_buf();
    let writer = std::sync::Arc::clone(&latest);
    // A bad key or a file from a newer release stops the start here (exit 2).
    let (recorder, handle) = history::start(history::config_from(
        ctx.config.get("monitor"),
        ctx.site_dir,
    )?)?;
    let poller_handle = handle.clone();
    std::thread::spawn(move || loop {
        match collect_from(&path, Some((&recorder, &poller_handle))) {
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

    Ok(MonitorState {
        latest,
        interval,
        history: handle,
    })
}

/// Poll the fleet, print the check, exit 1 on faults.
fn run_check(config_path: &std::path::Path) -> anyhow::Result<()> {
    let (d, readings) = collect_from(config_path, None)?;
    print!("{}", check::render(&d, &readings));
    if d.level == digest::Level::Fault {
        std::process::exit(1);
    }
    Ok(())
}

/// Poll the fleet and build a digest, or explain why not.
///
/// With a recorder, the poll is also handed to the history writer. The history
/// finding is added before the poll is recorded, so the stored verdict equals
/// the one `/check` shows.
fn collect_from(
    config_path: &std::path::Path,
    history: Option<(&history::Recorder, &history::HistoryHandle)>,
) -> anyhow::Result<(digest::Digest, Vec<poll::NodeReading>)> {
    let fleet = fleet::Fleet::from_config(config_path)?;
    let token = fleet.perf_token();
    let mut readings = poll::fleet(
        &fleet.nodes,
        token.as_deref(),
        Duration::from_millis(fleet.timeout_ms),
        &fleet.header_check,
    );
    // Sequential with the HTTPS polls on purpose: the round trip is not taken
    // while this host is doing its own TLS work.
    let tunnels = tunnel::fleet(&fleet, Duration::from_millis(fleet.timeout_ms));
    for (r, t) in readings.iter_mut().zip(tunnels) {
        r.tunnel = t;
    }
    let thresholds = digest::Thresholds {
        tunnel_handshake_age_s: fleet.tunnel_handshake_max_age_s,
        log_reopen_age_s: fleet.log_reopen_max_age_s,
        ..Default::default()
    };
    let at_ms = unix_ms();
    let mut d = digest::build(&readings, &thresholds, now_iso8601());
    if let Some((recorder, handle)) = history {
        let h = handle.digest();
        if let Some(f) = history::finding(&h) {
            d.add_finding(f);
        }
        d.history = Some(h);
        recorder.record(history::batch(at_ms, &readings, &d));
    }
    Ok((d, readings))
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A plain-text answer that no cache may keep.
fn plain(status: u16, text: &str) -> Response {
    Response::text(text)
        .with_status(status)
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
}

fn history_ndjson(req: &Request, st: &MonitorState) -> Result<Response> {
    paged(req, st, history::health_page)
}

fn crawlers_ndjson(req: &Request, st: &MonitorState) -> Result<Response> {
    paged(req, st, history::crawl_page)
}

/// The shared read path of `/history` and `/crawlers`.
///
/// One page reads inside one read transaction (see `history::fill_page`), in
/// chunks of 256 rows, and stops reading when the page is full.
fn paged<T: history::Keyed>(
    req: &Request,
    st: &MonitorState,
    read: history::PageFn<T>,
) -> Result<Response> {
    use m6_core::ndjson::{Cursor, PageLimits};

    let Some(path) = st.history.file() else {
        return Ok(plain(
            503,
            "history is off: set [monitor] history_file to keep one\n",
        ));
    };
    let range = match m6_core::time_range::TimeRange::from_request(
        req,
        m6_core::time_range::DEFAULT_MAX_SPAN,
    ) {
        Ok(r) => r,
        Err(e) => return Ok(e.to_response()),
    };
    let after = match Cursor::from_query(req.query()) {
        Ok(None) => None,
        Ok(Some(c)) => match history::parse_cursor(c.as_str()) {
            Some(k) => Some(k),
            None => return Ok(m6_core::ndjson::CursorError::Malformed.to_response()),
        },
        Err(e) => return Ok(e.to_response()),
    };
    let node = match history::node_param(req.query()) {
        Ok(n) => n,
        Err(msg) => return Ok(Response::refusal(&msg)),
    };
    let unavailable =
        |e: &dyn std::fmt::Display| plain(503, &format!("history is unavailable: {e}\n"));
    let conn = match m6_core::sqlite::open_reader(path) {
        Ok(c) => c,
        Err(e) => return Ok(unavailable(&e)),
    };
    let node_id = match &node {
        None => None,
        Some(name) => match history::node_id(&conn, name) {
            Ok(Some(id)) => Some(id),
            Ok(None) => return Ok(Response::refusal(&"`node` names no node in the history")),
            Err(e) => return Ok(unavailable(&e)),
        },
    };
    let range = (range.from_unix_ms(), range.to_unix_ms());
    let (page, next) =
        match history::fill_page(&conn, range, node_id, after, PageLimits::default(), read) {
            Ok(p) => p,
            Err(history::PageError::Db(e)) => return Ok(unavailable(&e)),
            Err(history::PageError::Build(e)) => return Err(e.into()),
        };
    drop(conn);
    Ok(page.into_response(req.query(), next.as_ref()))
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

/// The whole A-to-H report, as text, from the latest snapshot.
///
/// This is what `--check` printed, and the only reason that flag existed: the
/// report had no route out. With this it is reachable the way everything else is,
/// over an ssh-forwarded socket:
///
/// ```text
/// ssh -fN -L /tmp/m6mon.sock:/run/m6/m6-monitor.sock user@example.com
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
            m6_core::util::html_escape(&f.node),
            m6_core::util::html_escape(&f.text)
        ));
    }

    h.push_str(
        "<div class=wrap><table><tr>\
         <th>node</th><th>role</th><th>status</th><th>rtt</th>\
         <th>reqs</th><th>hit rate</th><th>p50</th><th>p99</th>\
         <th>load</th><th>mem</th><th>disk</th><th>temp</th>\
         <th>svc up</th><th>host up</th><th>tunnel</th></tr>",
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
             <td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            m6_core::util::html_escape(&n.name),
            m6_core::util::html_escape(&n.role),
            m6_core::util::html_escape(&n.status),
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
            // The same wording as /check section H.
            match &n.tunnel {
                Some(t) => m6_core::util::html_escape(&format!(
                    "{} handshake {}",
                    t.rtt_text(),
                    t.handshake_text()
                )),
                None => "<span class=dim>no tunnel</span>".to_string(),
            },
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
            m6_core::util::html_escape(&n.name),
            pools
        ));
    }
    h.push_str("</table></div>");
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_digest(name: &str, tunnel: Option<digest::TunnelDigest>) -> digest::NodeDigest {
        let r = poll::NodeReading {
            name: name.into(),
            role: "cache".into(),
            url: "http://edge-a.example.com".into(),
            health: None,
            health_status: None,
            perf: None,
            perf_error: None,
            traffic: None,
            traffic_error: None,
            header_checks: Vec::new(),
            rtt: None,
            unreachable: None,
            tunnel: None,
        };
        let mut d = digest::build(&[r], &digest::Thresholds::default(), "now".into());
        let mut n = d.nodes.remove(0);
        n.tunnel = tunnel;
        n
    }

    /// The page carries the tunnel column in the wording of /check section H,
    /// and says "no tunnel" for a node without one.
    #[test]
    fn the_page_has_a_tunnel_column_and_says_no_tunnel_without_one() {
        let t = digest::TunnelDigest {
            address: "192.0.2.2:443".into(),
            rtt_ms: Some(0.4),
            rtt_samples_ms: vec![0.4, 0.5, 0.6],
            probe: digest::ProbeState::Connected,
            probe_error: None,
            handshake: digest::HandshakeState::Ok,
            handshake_age_s: Some(42),
            handshake_detail: None,
        };
        let mut d = digest::build(&[], &digest::Thresholds::default(), "now".into());
        d.nodes = vec![node_digest("edge-a", Some(t)), node_digest("edge-b", None)];
        let html = render(&d);
        assert!(html.contains("<th>tunnel</th>"), "{html}");
        assert!(
            html.contains("rtt 0.40ms (n 3, connected) handshake 42s ago"),
            "{html}"
        );
        assert!(html.contains("no tunnel"), "{html}");
    }

    /// Node name, role and status come from config and from the nodes, and
    /// reach the page as HTML.
    #[test]
    fn the_page_escapes_node_name_role_and_status() {
        let mut d = digest::build(&[], &digest::Thresholds::default(), "now".into());
        let mut n = node_digest("<i>edge</i>", None);
        n.role = "<b>cache</b>".into();
        n.status = "<u>ok</u>".into();
        d.nodes = vec![n];
        d.findings.push(digest::Finding {
            level: digest::Level::Fault,
            node: "<s>monitor</s>".into(),
            text: "file </p>/x is old".into(),
        });
        let html = render(&d);
        assert!(!html.contains("<s>monitor"), "{html}");
        assert!(html.contains("file &lt;/p&gt;/x is old"), "{html}");
        assert!(!html.contains("<i>edge"), "{html}");
        assert!(!html.contains("<b>cache"), "{html}");
        assert!(!html.contains("<u>ok"), "{html}");
        assert!(html.contains("&lt;i&gt;edge&lt;/i&gt;"), "{html}");
        assert!(html.contains("&lt;b&gt;cache&lt;/b&gt;"), "{html}");
        assert!(html.contains("&lt;u&gt;ok&lt;/u&gt;"), "{html}");
    }
}
