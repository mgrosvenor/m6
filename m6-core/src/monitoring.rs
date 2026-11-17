//! The `/health` endpoint and the two documents a monitor reads: what they
//! carry, and why so little.
//!
//! `/health` is answered inside m6-http before routing, the cache and any
//! backend, so it costs a JSON serialisation and nothing else. It exists so
//! that an uptime monitor has something better to hit than a real page.
//!
//! The metrics and traffic documents are built by the same process and written
//! on its operational log lines, one line each, with `M6_TYPE` set to `perf` or
//! `traffic`. A monitor reads them from mon's journal. This module holds their
//! types because the node that writes a document and the monitor that reads it
//! must agree on the shape.
//!
//! # Why not just monitor `/`
//!
//! Monitoring the homepage conflates three questions that have different
//! answers and different fixes:
//!
//! 1. Is this node's m6-http process alive and accepting TLS?
//! 2. Are this node's backends present?
//! 3. Does the site render?
//!
//! Rendering a homepage is template work proportional to what is on it: an
//! image-heavy index costs milliseconds of it, roughly 43 us per image of
//! manifest work, and a monitor that sends `Cache-Control: no-cache` forces
//! every check to pay the whole bill. That is a large, variable answer to what
//! should be a small, constant question.
//!
//! It also cannot tell you *which* node is unwell. Checks against the apex go
//! wherever GeoDNS sends the checking region, so one dead node can hide
//! behind the healthy ones. `/health` is per-node by construction: point the
//! monitor at `origin.`/`edge-a.`/`edge-b.` and the answer names the node that
//! produced it.
//!
//! # What `/health` answers for
//!
//! `/health` answers for the m6-http process and for the dependencies whose
//! failure the process already observes. It probes nothing: every answer is a
//! flag read and a JSON serialisation.
//!
//! A cache node's origin is one such dependency. Its flag is the follower's own
//! down bit, set where a heartbeat or a forwarded client request already
//! observes the origin failing and cleared by the next heartbeat answer. While
//! it is set `/health` answers 503 `{"status":"degraded","node":"<name>"}`.
//! While it is clear `/health` answers 200 `{"status":"ok","node":"<name>"}`.
//!
//! So `/health` has two answers plus an absence. A process that is gone, or
//! whose listener refuses, answers nothing, and `m6-monitor` reads that as
//! `unreachable`. The two answers and the monitor's four states were designed
//! to fit.
//!
//! Proving the origin reachable inside the check would put a network round trip
//! on the most frequently hit path on the box, and a warm cache keeps serving
//! while the origin is unreachable. The flag costs one branch.
//!
//! The degraded body names the node and never the dependency. `/health` is
//! public and unsecured, so a dependency name tells a stranger which part of
//! the node to wait out. The warn and info lines the process logs on the change
//! carry that detail.
//!
//! A socket pool keeps no per-member health state, so a dead member cannot
//! change the answer. A request that connects to a dead member takes the next
//! member, and that is where the failure is found.
//!
//! # Why the figures are not on `/health`
//!
//! `/health` answers `ok` or `degraded`. The metrics document carries counters
//! and latency percentiles. They are separate surfaces rather than one answer
//! that grows richer for authorised callers, and the reason is cost.
//!
//! **A health check has to be constant-cost by construction.** It is the most
//! frequently hit path on the box, it runs when the machine is already in
//! trouble, and it is the thing a monitor uses to decide whether the node is
//! alive. Percentiles mean sorting a 4096-sample reservoir and serialising a
//! larger payload. Put that behind a conditional on the same path and the
//! expensive branch is one misconfigured header away from being taken on
//! every check, on every node, forever. Keeping the figures on their own line
//! makes the cheap answer cheap because there is no other answer it can give.
//!
//! **The figures are not public, and not out of modesty about traffic volume.**
//! Live latency and error counters are a feedback channel for whoever is
//! attacking you. An attacker probing for a resource-exhaustion path is
//! normally blind: they cannot tell whether a request is expensive or whether
//! their load is landing. Publishing hit and miss percentiles tells them
//! exactly which requests are cheap and which are expensive, and the ratio
//! between a cache hit and a backend miss runs to three orders of magnitude,
//! so the ranking is unambiguous. `backend_errors_total` then confirms in real
//! time when they have found something that hurts. That turns a blind probe into a
//! tuning loop. Keeping the document off the public listener also means an
//! anonymous scrape loop can never reach the sort.
//!
//! **`/health` exposes no version or build string.** It is unauthenticated, and a
//! version on a public URL tells a scanner which vulnerabilities are worth trying.
//! That has not changed.
//!
//! It was very nearly changed on 2026-09-20, and the reason it was not is worth
//! keeping. Issue #105 wanted the running binary's identity visible, and the
//! first attempt put a bare `hash` on `/health` on the argument that a hash is
//! not a version and so discloses nothing. The disclosure argument holds. The
//! usefulness argument does not: **a hash on its own tells a reader nothing at
//! all.** It could be a hash of anything. An identity is a hash keyed to the
//! name and version of the thing it identifies, and the place those already live
//! is the metrics document. Owner's decision. See `BuildId`.
//!
//! The metrics document does report the release, from 2026-09-16. This reverses
//! "no version at any tier", so the reasoning is recorded rather than left as a
//! silent edit.
//!
//! The disclosure argument applies to an anonymous reader. The metrics document
//! is not on a public route. It is written to the operational log, which the
//! node's own monitor receives over the journal, and it answers no request. An
//! attacker who can read that log already reads live latency percentiles and
//! error counters, which is a far better tuning signal than a version string. So
//! the marginal disclosure is small, and it is bounded by access to the log we
//! already treat as sensitive.
//!
//! Against that: with no version anywhere, "every node runs the release we think
//! it does" is an invariant nothing can check without ssh. Four written records
//! can disagree about one fleet while every node serves something none of them
//! names, and nothing in the system is able to say so. An operator cannot act on
//! a fleet they cannot observe, and a silent version turns a deploy defect into
//! months of plausible-looking bookkeeping. The version goes in the metrics
//! document, not on `/health`, which keeps the public surface exactly as it was.
//!
//! The document is read from the log rather than fetched over a route, so there
//! is no door to configure and none to forget.

use serde::{Deserialize, Serialize};

use crate::telemetry::StatsSnapshot;

/// One socket-backed backend pool's size.
///
/// A node built before the pool lost its health state sent an `active` field
/// beside the count. A monitor of that build cannot read a newer node's report,
/// because the field it requires is gone, so the monitor is upgraded before the
/// nodes it watches. A newer monitor reads an older node's report unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolHealth {
    pub name: String,
    /// Members the pool holds. The pool keeps no per-member health state, so
    /// every member stays in rotation.
    pub total: usize,
}

/// The public payload. Field order here is the field order on the wire.
///
/// The status, the node and the process index when the node has one. It is an
/// unauthenticated public URL, so everything on it is published to anyone who
/// asks, and a monitor needs exactly those things from it: whether this node
/// can serve, which node answered, and which of the node's processes did.
///
/// It deliberately does not carry `uptime_s`, the name and occupancy of every
/// socket pool, or the names of the URL backends. That would tell an anonymous
/// caller the internal service topology (`m6-html`, `m6-file`, `render-contact`,
/// `render-reports`), how many workers back each one, when the process last
/// restarted, and, by polling, exactly when a deploy or a crash happened and
/// whether a pool was losing members. None of that helps a monitor decide
/// whether the node can serve, and all of it helps someone deciding what to
/// aim at. It lives in the metrics document, which is written to the log.
///
/// The degraded answer names no dependency for the same reason. The dependency
/// is in the warn and info lines the process logs on the change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthReport {
    /// Which of a node's `reuse_port` processes answered, 0 to N-1. Absent
    /// unless `[server] process_index` is set, so an existing node's body is
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_index: Option<u32>,
    /// `"ok"` with 200 while the process is alive and every dependency it
    /// reads answers. `"degraded"` with 503 while the process is alive and
    /// serving its public and cached pages with a dependency failing.
    ///
    /// `String` rather than `&'static str` because an aggregator reads this
    /// back off the wire and cannot produce a `'static` one.
    pub status: String,
    /// This node's identity, e.g. `"origin"`. The reason to have the endpoint
    /// at all: the answer says which node produced it.
    pub node: String,
}

/// What is running: the binary's name, its version, and a hash of its bytes.
///
/// **A hash alone is not an identity.** That was the first shape of #105 and it
/// was wrong: an opaque number on its own could be a hash of anything, and a
/// reader who sees two of them differ learns that something differs, not what.
/// Keyed to the name and the version it becomes the answer to a question an
/// operator actually asks, which is "what is this node running".
///
/// The three carry different information and none is redundant:
///
/// - `name` says WHICH binary. A node runs several, and "the node is on 1.10.0"
///   has never been one fact. A fleet can have m6-http and its renderers built
///   from different trees, and before this nothing could express that.
/// - `version` says which release it claims to be. It is what an operator reads
///   and what release notes are written against.
/// - `hash` says which BUILD it actually is. Rust is not byte-reproducible, so
///   one tag built twice gives two binaries reporting one version. Two nodes
///   can then agree on a version and run different bytes, and only the hash
///   shows it.
///
/// `hash` and not `md5`: the algorithm is how the value is produced, not what it
/// means, and naming the field after it makes changing it a wire break for every
/// reader. It IS md5 today, because that is the number the surrounding tooling
/// already compares: a deployment records md5 per node and refuses a promotion
/// when two nodes disagree. A second hash of the same bytes under another
/// algorithm would give an operator two numbers and answer nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildId {
    /// The binary's own name, e.g. `"m6-http"`, from its crate at compile time.
    pub name: String,
    /// The m6 release this was built against. Same value, and the same
    /// reasoning, as the `version` field this replaced.
    pub version: String,
    /// Hex md5 of the running executable. Empty when it could not be read, which
    /// the monitor renders as unknown and counts as drift rather than agreement.
    pub hash: String,
}

/// This process's own [`BuildId`], computed once.
///
/// Computed once and cached for the life of the process, which is correct and
/// not merely cheap: a deploy replaces the file on disk while this process keeps
/// running the bytes it started with, and the bytes it started with are the
/// honest answer to "what is serving". Re-reading per request would report the
/// NEW binary while still running the old one, which is exactly the lie this is
/// here to prevent.
///
/// The name comes from the running executable rather than from a compile-time
/// constant, because the constant available to a library is m6-core's own name
/// and the answer wanted is the binary's. The version is m6-core's, which is the
/// m6 release: for the workspace binaries that is their own version, and for a
/// service linking core from a git tag it is that tag, which is the more useful
/// answer for a service whose own version says nothing about which m6 it is
/// built against.
pub fn build_id() -> &'static BuildId {
    static BUILD: std::sync::OnceLock<BuildId> = std::sync::OnceLock::new();
    BUILD.get_or_init(|| BuildId {
        name: std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        hash: executable_md5(),
    })
}

/// Hex md5 of this process's own executable, or empty if it cannot be read.
///
/// Read in chunks rather than with `fs::read`, which would allocate the whole
/// binary: m6-http is tens of megabytes and this runs on a 1 cpu node with the
/// event loop about to start.
///
/// Empty on any failure, which is a real possibility under confinement and must
/// never take the metrics document down: a node that cannot say what it is
/// running is still a node that can report its latency. The monitor reads an
/// empty hash as unknown and counts it as drift rather than as agreement.
fn executable_md5() -> String {
    use md5::{Digest, Md5};
    use std::io::Read;

    let Ok(path) = std::env::current_exe() else {
        return String::new();
    };
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut hasher = Md5::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // A partial read is worse than none: it produces a
            // plausible-looking hash of a prefix, which compares unequal
            // across two nodes running identical binaries and reports drift
            // that is not there.
            Err(_) => return String::new(),
        }
    }
    format!("{:x}", hasher.finalize())
}

impl HealthReport {
    /// The `ok` answer: 200, the process is alive and every dependency it
    /// reads answers.
    ///
    /// A socket pool keeps no per-member health state, so this process cannot
    /// judge one, and `config` refuses a pool with no members at start. A dead
    /// member is found when a request connects to it, and that request takes
    /// the next member.
    ///
    /// Deliberately never derived from performance. A latency spike is a
    /// signal for a human or a dashboard to act on, not grounds for a node
    /// to declare itself down and be pulled from rotation while it is
    /// still serving every request correctly. That is also why the numbers
    /// live in the metrics document and not here.
    pub fn build(node: &str) -> (u16, HealthReport) {
        (
            200,
            HealthReport {
                process_index: None,
                status: "ok".to_string(),
                node: node.to_string(),
            },
        )
    }

    /// The `degraded` answer: 503, the process is alive and serving its public
    /// and cached pages while a dependency it reads is failing.
    ///
    /// The status carries the answer so a reader that treats a non-200 as
    /// degraded sees it without parsing the body, which is how `m6-monitor`
    /// already reads a node.
    ///
    /// The body names the node and never the dependency or a reason. `/health`
    /// is public and unsecured, so naming the failing dependency would tell a
    /// stranger which part of the node to wait out or to aim at. That detail is
    /// in the warn and info lines the process logs on the change.
    pub fn degraded(node: &str) -> (u16, HealthReport) {
        (
            503,
            HealthReport {
                process_index: None,
                status: "degraded".to_string(),
                node: node.to_string(),
            },
        )
    }

    /// Serialise to a body plus headers.
    ///
    /// `no-store`, not `no-cache`: a stale health answer is worse than no
    /// answer, and this must never be stored by us, by a downstream cache, or
    /// by the monitor itself.
    pub fn into_response(self, code: u16) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let body = serde_json::to_vec(&self)
            // A serialisation failure here is not a reason to fail the health
            // check with an unparseable body; fall back to something a
            // monitor can still read.
            .unwrap_or_else(|_| br#"{"status":"ok"}"#.to_vec());
        let headers = vec![
            (
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ),
            ("Cache-Control".to_string(), "no-store".to_string()),
            // Nothing here is meant for a browser to render or a crawler to
            // index, and it is a public URL.
            ("X-Robots-Tag".to_string(), "noindex, nofollow".to_string()),
        ];
        (code, headers, body)
    }
}

/// The metrics document: the numbers, plus the detail `/health` must not leak.
///
/// Carries `node` and `uptime_s` so a dashboard scraping several nodes can
/// attribute a sample without correlating two requests, and so a counter reset
/// is distinguishable from a quiet node (uptime going backwards means the
/// process restarted, not that traffic stopped).
///
/// `pools` and `url_backends` moved here from `/health`. They are genuinely
/// useful for diagnosing a degraded node, which is why they are kept rather
/// than dropped, and they are kept out of `/health` for the same reason the
/// latency percentiles are: they describe the shape of the thing being served.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfReport {
    /// Which of a node's `reuse_port` processes answered, 0 to N-1. Absent
    /// unless `[server] process_index` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_index: Option<u32>,
    pub node: String,
    /// What this node is running: name, version and build hash.
    ///
    /// [`BuildId`] says why the three fields differ and why the hash is md5.
    /// The version stays because without one the only way to learn what a node
    /// runs was to ssh in and ask the binary, so "every node runs the pinned
    /// release" was an invariant nothing could check. Four written records
    /// can disagree about one fleet with none of them matching it: a deploy
    /// repository pinning one tag, its captured config naming that tag with an
    /// artefact hash matching nothing running, a release log naming an older
    /// tag, and every node serving a fourth. No node has to be faulty for that.
    /// Nothing can observe the truth, so the records rot without anyone being
    /// wrong on purpose.
    ///
    /// `serde(default)` so a node older than this change deserialises to an empty
    /// `BuildId` instead of making the whole payload unparseable to an
    /// aggregator. The monitor renders that as "too old to say" rather than as
    /// agreement.
    #[serde(default)]
    pub build: BuildId,
    /// This process's uptime. `host.uptime_s` is the machine's, and the two
    /// differing is how a service restart is told apart from a reboot.
    pub uptime_s: u64,
    /// Socket-backed pools. Empty on a cache node, which has none.
    pub pools: Vec<PoolHealth>,
    /// Names of URL backends. Presence only: proving one reachable would mean
    /// a network round trip on the accounting tick, which would stall the
    /// serving loop that builds the document.
    pub url_backends: Vec<String>,
    /// The TLS chain this process loaded, how long each certificate has
    /// left, and each certificate's SHA-256 fingerprint. Leaf first, so
    /// `tls[0]` is the certificate an *ACME* client renews. Empty on a process
    /// serving no TLS.
    ///
    /// The fingerprint is what tells two certificates with one expiry apart,
    /// so a fleet can be checked for serving one certificate rather than for
    /// expiring on one day. [`crate::tls::Fingerprint`] says what is hashed
    /// and why. It is the DER a client already receives in the handshake.
    ///
    /// From the loaded material rather than from a file on disk, which is the
    /// whole value of reporting it here: a renewal that wrote a new certificate
    /// and never restarted the server leaves the two facts disagreeing, and the
    /// one that matters is what is being served.
    ///
    /// `serde(default)` so a node older than this field stays parseable to an
    /// aggregator, in the shape `build` already uses. An empty list from an old
    /// node and an empty list from a plaintext process are the same payload, so
    /// a reader that needs to tell them apart reads `build.version` too.
    #[serde(default)]
    pub tls: Vec<crate::tls::TlsCertificate>,
    /// Certificates in the loaded chain whose `notAfter` could not be read.
    ///
    /// Without this, `tls` being short and `tls` being complete are the same
    /// payload, and an aggregator taking the soonest expiry of what it got
    /// would call a chain healthy on the strength of an intermediate while
    /// the leaf is unaccounted for.
    #[serde(default)]
    pub tls_unreadable: usize,
    /// Whether the log is emitting.
    ///
    /// `None` means a node older than this field.
    #[serde(default)]
    pub logging: Option<LoggingHealth>,
    pub metrics: StatsSnapshot,
    /// The machine underneath: load, memory, disk, temperature, uptime.
    ///
    /// Here rather than in a document of its own because an aggregator wants
    /// one document per node, and because these are the numbers you want *next
    /// to* the latency figures: a p99 that moved is a different conversation
    /// depending on whether load also moved. Every field is optional, and a
    /// field a platform cannot answer is absent rather than zero.
    pub host: crate::platform::host::HostSnapshot,
}

/// Outcome of a metrics document build.
pub enum PerfOutcome {
    /// A traffic document. The traffic line is written from `TrafficReport`
    /// directly, so nothing builds this variant now.
    ///
    /// The report is built from the window of events `m6-http` logged itself,
    /// which allocates a record per retained event on a cache miss. It is kept
    /// out of the metrics document for the same reason the metrics are kept
    /// off `/health`: the cost profiles differ, and a reader that wants the
    /// traffic summary asks for it by name.
    /// Boxed: `TrafficReport` is far larger than every other variant here, so
    /// carrying it inline made the whole enum that size even when the outcome
    /// was a one-word error.
    Traffic(Box<TrafficReport>),
    /// No document. The answer is a 404, with no credential challenge.
    Disabled,
    /// A refusal: no credential, or one that does not verify.
    Unauthorised,
    /// A refusal: the credential verifies but lacks the required claim.
    Forbidden,
    /// Boxed for the same reason as `Traffic`: `PerfReport` is 608 bytes
    /// against the 24 of the largest error variant, and an inline copy made
    /// every `PerfOutcome` that size.
    Ok(Box<PerfReport>),
}

/// What a report is about: the node, and the live facts to report on it.
pub struct PerfSubject<'a> {
    pub node: &'a str,
    pub uptime_s: u64,
    pub pools: Vec<PoolHealth>,
    pub url_backends: Vec<String>,
    pub host_path: &'a std::path::Path,
    /// This process's `[server] process_index`, if it has one.
    pub process_index: Option<u32>,
}

impl PerfReport {
    /// The report itself, for the caller that writes the metrics line.
    /// Nothing here checks access.
    pub fn build_authorised(
        subject: PerfSubject<'_>,
        snapshot: impl FnOnce() -> StatsSnapshot,
    ) -> PerfOutcome {
        let PerfSubject {
            process_index,
            node,
            uptime_s,
            pools,
            url_backends,
            host_path,
        } = subject;
        // `snapshot` is a closure so the reservoir sort happens once, here,
        // when the document is built.
        // Read at build time, like the reservoir sort: an ordinary request
        // never causes a /proc read either.
        PerfOutcome::Ok(Box::new(PerfReport {
            process_index,
            node: node.to_string(),
            build: build_id().clone(),
            uptime_s,
            pools,
            url_backends,
            // Read from the registry the TLS loader wrote, not from
            // the certificate file: no path is opened to answer this,
            // so building a document cannot be made to do disk work
            // by a request.
            tls: crate::tls::loaded(),
            tls_unreadable: crate::tls::unreadable(),
            logging: Some(LoggingHealth::read()),
            metrics: snapshot(),
            host: crate::platform::host::snapshot(host_path),
        }))
    }
}

/// The headers a metrics answer carries.
///
/// One definition, because the 404 a disabled gate gives and a serialised
/// document carry the same three headers, and a second copy is a second thing
/// to keep in step with it.
pub fn perf_headers() -> Vec<(String, String)> {
    vec![
        (
            "Content-Type".to_string(),
            "application/json; charset=utf-8".to_string(),
        ),
        ("Cache-Control".to_string(), "no-store".to_string()),
        ("X-Robots-Tag".to_string(), "noindex, nofollow".to_string()),
    ]
}

impl PerfOutcome {
    pub fn into_response(self) -> (u16, Vec<(String, String)>, Vec<u8>) {
        match self {
            // The token check's refusal is one definition, in `http`: the same
            // bytes whichever route asked.
            PerfOutcome::Unauthorised => crate::http::token_refusal(false),
            PerfOutcome::Forbidden => crate::http::token_refusal(true),
            other => {
                let headers = perf_headers();
                match other {
                    PerfOutcome::Disabled => (404, headers, br#"{"error":"not found"}"#.to_vec()),
                    PerfOutcome::Ok(report) => {
                        let body = serde_json::to_vec(&report)
                            .unwrap_or_else(|_| br#"{"error":"serialisation failed"}"#.to_vec());
                        (200, headers, body)
                    }
                    PerfOutcome::Traffic(report) => {
                        let body = serde_json::to_vec(&report)
                            .unwrap_or_else(|_| br#"{"error":"serialisation failed"}"#.to_vec());
                        (200, headers, body)
                    }
                    PerfOutcome::Unauthorised | PerfOutcome::Forbidden => unreachable!(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn health_is_ok_and_names_the_node() {
        let (code, report) = HealthReport::build("origin");
        assert_eq!(code, 200);
        assert_eq!(report.status, "ok");
        assert_eq!(report.node, "origin");
    }

    /// A degraded node answers 503 and the body carries the status and the
    /// node. It names no dependency and no reason: `/health` is public and
    /// unsecured, so that detail belongs in the process log.
    #[test]
    fn degraded_is_503_and_the_body_names_only_the_node() {
        let (code, report) = HealthReport::degraded("edge-a");
        assert_eq!(code, 503);
        assert_eq!(report.status, "degraded");
        assert_eq!(report.node, "edge-a");
        let (code, _, body) = report.into_response(code);
        assert_eq!(code, 503);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"status":"degraded","node":"edge-a"}"#
        );
    }

    #[test]
    fn response_is_never_stored_and_never_indexed() {
        let (code, report) = HealthReport::build("origin");
        let (code, headers, body) = report.into_response(code);
        assert_eq!(code, 200);
        let get = |k: &str| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("Cache-Control"), Some("no-store"));
        assert_eq!(get("X-Robots-Tag"), Some("noindex, nofollow"));
        assert_eq!(get("Content-Type"), Some("application/json; charset=utf-8"));

        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["node"], "origin");
    }

    /// An empty snapshot. The collector that fills one in lives in m6-http;
    /// these tests are about the document's shape, not about how the numbers
    /// are gathered.
    fn snap() -> StatsSnapshot {
        StatsSnapshot::default()
    }

    #[test]
    fn a_disabled_outcome_is_a_404_with_no_door() {
        // Off by default, and it does not advertise a door that cannot be
        // opened: 404, not 401.
        let (code, _, body) = PerfOutcome::Disabled.into_response();
        assert_eq!(code, 404);
        assert!(!String::from_utf8_lossy(&body).contains("unauthorised"));
    }

    /// The build identity reaches the wire, whole. An aggregator cannot report
    /// drift it never receives, and each of the three parts answers a different
    /// question: which binary, which release it claims to be, which build it
    /// actually is.
    #[test]
    fn the_report_carries_the_running_build_identity() {
        let out = PerfReport::build_authorised(
            PerfSubject {
                process_index: None,
                node: "origin",
                uptime_s: 5,
                pools: vec![],
                url_backends: vec![],
                host_path: Path::new("/"),
            },
            snap,
        );
        let (code, _, body) = out.into_response();
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["build"]["version"].as_str(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert!(!v["build"]["version"].as_str().unwrap().is_empty());

        // The name is the RUNNING binary's, not m6-core's. A library cannot
        // know the binary's name at compile time, so this reads the executable,
        // and a regression here would silently label every service "m6-core".
        let name = v["build"]["name"].as_str().expect("a name");
        assert!(!name.is_empty());
        assert_ne!(
            name, "m6-core",
            "the name must come from the running executable, not from the \
             library's own CARGO_PKG_NAME: every service would report m6-core"
        );

        // The hash is the md5 of this very binary. Checked against a hash taken
        // here rather than against a constant, which would pin whatever the
        // code produced on the day and prove only that it has not changed.
        // This is the property that makes the field worth having: it must equal
        // `md5sum` of the artefact.
        use md5::{Digest, Md5};
        let exe = std::env::current_exe().expect("a test binary has a path");
        let expected = format!("{:x}", Md5::digest(std::fs::read(&exe).unwrap()));
        assert_eq!(v["build"]["hash"].as_str(), Some(expected.as_str()));
        assert_eq!(expected.len(), 32, "hex md5 is 32 characters");
    }

    /// Two builds of one tag report the same version and different hashes. That
    /// is the whole reason the hash is there, so it is asserted rather than
    /// assumed: a version comparison cannot see this and a hash comparison can.
    ///
    /// Simulated by hashing two different byte strings, because the real case
    /// needs two compilations of one source and Rust gives no way to force that
    /// in a unit test. What is being pinned is the CLAIM: same version, different
    /// bytes, and only the hash distinguishes them.
    #[test]
    fn one_version_two_builds_differ_only_in_the_hash() {
        use md5::{Digest, Md5};
        let a = BuildId {
            name: "m6-http".into(),
            version: "1.10.0".into(),
            hash: format!("{:x}", Md5::digest(b"build one")),
        };
        let b = BuildId {
            hash: format!("{:x}", Md5::digest(b"build two")),
            ..a.clone()
        };

        assert_eq!(a.version, b.version, "same tag");
        assert_eq!(a.name, b.name, "same binary");
        assert_ne!(a.hash, b.hash, "different bytes");
        assert_ne!(
            a, b,
            "a fleet holding these two is NOT uniform, and comparing versions \
             alone would call it uniform."
        );
    }

    /// A payload without the field still parses, because the fleet is upgraded one
    /// node at a time and an aggregator that cannot read an older node learns
    /// nothing about the node it most needs to ask about.
    #[test]
    fn a_metrics_document_without_a_build_still_parses() {
        // A real payload with the field taken out, rather than a hand-written
        // fixture: a fixture only proves the fixture parses, and the first attempt
        // at one failed on unrelated required fields of StatsSnapshot.
        let out = PerfReport::build_authorised(
            PerfSubject {
                process_index: None,
                node: "origin",
                uptime_s: 5,
                pools: vec![],
                url_backends: vec![],
                host_path: Path::new("/"),
            },
            snap,
        );
        let (_, _, body) = out.into_response();
        let mut v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.as_object_mut().unwrap().remove("build").is_some());
        let p: PerfReport = serde_json::from_value(v).expect("older node must parse");
        assert_eq!(p.build, BuildId::default());
        assert_eq!(
            p.build.version, "",
            "and it reads as unknown, not as agreement"
        );
    }

    fn metrics_document_json() -> serde_json::Value {
        let out = PerfReport::build_authorised(
            PerfSubject {
                process_index: None,
                node: "origin",
                uptime_s: 5,
                pools: vec![],
                url_backends: vec![],
                host_path: Path::new("/"),
            },
            snap,
        );
        let (_, _, body) = out.into_response();
        serde_json::from_slice(&body).unwrap()
    }

    #[test]
    fn the_document_carries_logging_state() {
        let v = metrics_document_json();
        let p: PerfReport = serde_json::from_value(v.clone()).unwrap();
        assert!(p.logging.is_some());
        assert!(v["logging"].get("events_total").is_some());
    }

    #[test]
    fn a_metrics_document_from_a_node_older_than_logging_still_parses() {
        let mut v = metrics_document_json();
        assert!(v.as_object_mut().unwrap().remove("logging").is_some());
        let p: PerfReport = serde_json::from_value(v).expect("older node must parse");
        assert!(p.logging.is_none());
        let l: LoggingHealth =
            serde_json::from_str(r#"{"events_total":3,"seconds_since_last":1}"#).unwrap();
        assert_eq!(l.events_total, 3);
    }

    #[test]
    fn the_report_builds_with_no_credential() {
        let out = PerfReport::build_authorised(
            PerfSubject {
                process_index: None,
                node: "origin",
                uptime_s: 5,
                pools: vec![],
                url_backends: vec![],
                host_path: Path::new("/"),
            },
            snap,
        );
        let (code, _, body) = out.into_response();
        assert_eq!(code, 200);
        let p: PerfReport = serde_json::from_slice(&body).expect("a report");
        assert_eq!(p.node, "origin");
    }

    #[test]
    fn forbidden_is_a_403_with_no_store_and_no_www_authenticate() {
        let (code, headers, body) = PerfOutcome::Forbidden.into_response();
        assert_eq!(code, 403);
        assert_eq!(body, br#"{"error":"forbidden"}"#);
        let has = |name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
        assert!(has("X-Robots-Tag"));
        assert!(!has("WWW-Authenticate"));
        assert!(headers
            .iter()
            .any(|(n, v)| n == "Cache-Control" && v == "no-store"));
    }

    #[test]
    fn an_unauthorised_outcome_is_a_401_with_a_scheme_hint() {
        let (code, headers, body) = PerfOutcome::Unauthorised.into_response();
        assert_eq!(code, 401);
        assert_eq!(body, br#"{"error":"unauthorised"}"#);
        assert!(headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("WWW-Authenticate") && v == "Bearer"));
    }

    #[test]
    fn a_built_report_carries_the_metrics() {
        let out = PerfReport::build_authorised(
            PerfSubject {
                process_index: None,
                node: "origin",
                uptime_s: 11,
                pools: vec![PoolHealth {
                    name: "m6-html".to_string(),
                    total: 1,
                }],
                url_backends: vec!["origin".to_string()],
                host_path: Path::new("/"),
            },
            snap,
        );
        let (code, _, body) = out.into_response();
        assert_eq!(code, 200);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["node"], "origin");
        assert_eq!(parsed["uptime_s"], 11);
        assert_eq!(parsed["metrics"]["requests_total"], 0);
        assert!(parsed["metrics"]["hit_samples"].is_number());
        // The detail that /health must not publish to anyone lives here.
        assert_eq!(parsed["pools"][0]["name"], "m6-html");
        assert_eq!(parsed["url_backends"][0], "origin");
        // And the machine underneath, so an aggregator gets a node in one
        // request and can read latency next to the load that produced it.
        assert!(
            parsed["host"].is_object(),
            "the metrics document carries the host snapshot"
        );
        assert!(parsed["host"]["cpus"].as_u64().unwrap_or(0) >= 1);
        assert!(parsed["host"]["disk"]["total_bytes"].as_u64().unwrap_or(0) > 0);
        // Absent, not zero, where the platform cannot answer.
        assert!(parsed["host"].get("thermal").is_some());
    }

    /// The public payload is exactly two fields, and this asserts the set
    /// rather than an allowlist.
    ///
    /// An allowlist is the wrong shape for this test: it passes when a field
    /// is removed and, more to the point, it passed for as long as `/health`
    /// was publishing `uptime_s`, every pool name and occupancy, and the URL
    /// backend names, because those were on the list. Naming the exact set
    /// means anything added to this struct has to be added here too, in a
    /// test whose name says why that is a decision and not a formality.
    #[test]
    fn public_payload_is_exactly_status_and_node() {
        for (code, report) in [
            HealthReport::build("origin"),
            HealthReport::degraded("edge-a"),
        ] {
            let (_, _, body) = report.into_response(code);
            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let object = parsed.as_object().expect("object");

            let mut keys: Vec<&str> = object.keys().map(|k| k.as_str()).collect();
            keys.sort_unstable();
            assert!(
                !object.contains_key("host"),
                "/health must not publish load, memory or disk either: it is the \
                 same class of thing as the pool topology that was moved to the \
                 metrics document"
            );
            assert_eq!(
                keys,
                ["node", "status"],
                "/health is unauthenticated and public: every field here is \
                 published to anyone who asks. Topology, worker counts, uptime \
                 and the failing dependency belong in the metrics document or in \
                 the process log."
            );
        }
    }
}

// ── The traffic document ─────────────────────────────────────────────────────

/// What the traffic document carries: the node's own view of who has been
/// asking it for things, and whether its logging is alive.
///
/// This is the document that lets a fleet report be assembled without ssh. The
/// alternative was shipping the analytics log to a central box, and the size
/// says not to: a busy node's log runs to tens of megabytes, one request is
/// about 350 bytes, and the summary a human reads is about 1KB. Roughly
/// 1000:1. A second copy of the log would also land on a box that is not
/// backed up, and inherit a retention problem the original already has.
///
/// So the node summarises its own log and publishes the summary. The raw
/// NDJSON never moves, which is right for what it is: forensic material, read
/// by hand when something is being investigated, which is rare.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficReport {
    pub node: String,
    /// The window summarised, in minutes.
    ///
    /// A node that keeps its window in memory reports the span that window
    /// really holds, which is shorter than the window when a full ring
    /// overwrote events still inside it.
    pub window_minutes: u64,
    /// Events the window overwrote while they were still inside the window
    /// this report covers, counted by minute.
    ///
    /// Non-zero means the summary is missing those events: the totals are for
    /// `window_minutes`, not for a full hour. Zero for a report built from a
    /// log. `serde(default)`, so a node older than the field reads as zero
    /// drops instead of making the whole report unparseable.
    #[serde(default)]
    pub events_dropped: u64,
    pub total_requests: u64,
    pub status: std::collections::BTreeMap<u16, u64>,
    /// Clients worth a human's attention. Already graded: see
    /// `telemetry::ClientSummary::is_notable`.
    pub notable: Vec<NotableClient>,
    /// Top talkers by request count, whatever they are.
    ///
    /// Separate from `notable` on purpose. Volume is not suspicion, and the
    /// rule that conflated them fired on the operator's own address; but "who
    /// is asking for the most" is still the first question about an hour of
    /// traffic, and the answer is usually a crawler or a person.
    pub heavy_hitters: Vec<HeavyHitter>,
    pub crawlers: Vec<CrawlerReport>,
    /// Bot-shaped requests discarded as forged, and the addresses that sent
    /// them. Reported rather than silently dropped, because "no crawlers" and
    /// "886 forged crawler requests" are very different quiet hours.
    pub forged_bot_requests: u64,
    pub forgers: Vec<String>,
    /// Single refused probes: recorded, not escalated. One 404 to
    /// `/.git/config` is internet weather.
    pub probe_noise: Vec<String>,
    pub logging: LoggingHealth,
    /// Deliberate firewall blocks and whether they are still being hit.
    ///
    /// `None` when no collector is installed on the node, which is different
    /// from a node with no blocks. Read from a file written by a privileged
    /// timer, because `nft list ruleset` needs CAP_NET_ADMIN and the process
    /// answering public requests must not have it.
    #[serde(default)]
    pub firewall: Option<crate::firewall::FirewallState>,
}

/// Whether this process's log is still emitting.
///
/// Replaces counting log targets out of `journalctl` over ssh. A log level set
/// at start can silence everything, and when it does the process looks
/// entirely healthy from outside. It does not look healthy from inside, which
/// is where this is measured.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoggingHealth {
    pub events_total: u64,
    /// `None` means nothing has ever been emitted, which is not the same as
    /// "emitted zero seconds ago" and must not be reported as healthy.
    pub seconds_since_last: Option<u64>,
}

impl LoggingHealth {
    pub fn read() -> Self {
        Self {
            events_total: crate::log::pulse().events(),
            seconds_since_last: crate::log::pulse().seconds_since_last(),
        }
    }

    /// A healthy m6-http emits `periodic stats` every ten seconds. Past this,
    /// on a process that is up and serving, the main layer has gone quiet.
    pub const QUIET_SECONDS: u64 = 40;

    pub fn is_blind(&self) -> bool {
        match self.seconds_since_last {
            None => true,
            Some(s) => s > Self::QUIET_SECONDS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotableClient {
    pub ip: String,
    pub requests: u64,
    pub distinct_user_agents: usize,
    /// A sample of the agents this client presented. Empty on a node too old
    /// to report them, which is why the count above stays: a reader has to be
    /// able to tell "presented none we recorded" from "this node cannot say".
    #[serde(default)]
    pub user_agents: Vec<String>,
    pub status: std::collections::BTreeMap<u16, u64>,
    pub probe_paths: Vec<String>,
    pub injection_paths: Vec<String>,
    pub rotating_user_agents: bool,
    pub first_seen: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeavyHitter {
    pub ip: String,
    pub requests: u64,
    pub top_path: String,
    /// Renamed from `user_agents`, which held this count while the field a
    /// reader wanted did not exist.
    ///
    /// The WIRE name stays `user_agents`, because the count has been published
    /// under it since this struct existed and an aggregator reads the traffic
    /// document from nodes on other versions. Giving the strings that key
    /// instead would make a payload from any node on the other side of this
    /// change a type error rather than a partly readable report, which is the
    /// treatment `firewall` and `PerfReport.tls` are documented as avoiding.
    #[serde(rename = "user_agents")]
    pub distinct_user_agents: usize,
    /// A heavy hitter carries the COUNT and no sample, deliberately.
    ///
    /// The agents are on [`NotableClient`], which is the list this change
    /// exists for: a notable client is one the report is accusing of
    /// something, and what it claimed to be is evidence. A heavy hitter is
    /// merely loud, and the loudest source on a node is routinely the
    /// monitor's own polling.
    ///
    /// A sample was added here first and carried in every payload for up to
    /// eight clients per node while nothing read it. Rendering it instead
    /// would have buried the section: thirty-two agent lines per node, mostly
    /// describing traffic nobody is asking about.
    /// Share of responses that were 4xx or 5xx, so a loud client that is
    /// being served is distinguishable from one that is being refused.
    pub error_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlerReport {
    pub user_agent: String,
    pub requests: u64,
    pub client_ips: Vec<String>,
    pub paths: Vec<String>,
    /// When the counted requests began and ended.
    ///
    /// `serde(default)` so a node older than these fields stays parseable to
    /// an aggregator, in the shape `PerfReport::build` already uses. An
    /// aggregator renders an empty string as "no time reported" rather than
    /// as the epoch.
    #[serde(default)]
    pub first_seen: String,
    #[serde(default)]
    pub last_seen: String,
}

impl TrafficReport {
    /// Summarise records that are already parsed.
    ///
    /// The traffic window keeps its records in memory and never reads the log,
    /// so the report a window produces and the report the same lines would
    /// produce from the file run through this one body. A caller with records
    /// from a file leaves [`events_dropped`](Self::events_dropped) at zero,
    /// because a log keeps every event in its window.
    pub fn from_records(
        node: &str,
        records: &[crate::telemetry::AnalyticsRecord],
        window_minutes: u64,
    ) -> Self {
        let summary = crate::telemetry::TrafficSummary::from_records(records);

        let notable: Vec<NotableClient> = summary
            .clients
            .iter()
            .filter(|(_, c)| c.is_notable())
            .map(|(ip, c)| NotableClient {
                ip: ip.clone(),
                requests: c.requests,
                distinct_user_agents: c.distinct_user_agents,
                user_agents: c.user_agents.clone(),
                status: c.status.clone(),
                probe_paths: c.probe_paths.iter().map(|(p, _)| p.clone()).collect(),
                injection_paths: c.injection_paths.iter().map(|(p, _)| p.clone()).collect(),
                rotating_user_agents: c.is_rotating_user_agents,
                first_seen: c.first_seen.clone(),
                last_seen: c.last_seen.clone(),
            })
            .collect();

        let probe_noise: Vec<String> = summary
            .clients
            .iter()
            .filter(|(_, c)| !c.is_notable() && !c.probe_paths.is_empty())
            .map(|(ip, c)| format!("{ip} {}", c.probe_paths[0].0))
            .collect();

        let heavy_hitters: Vec<HeavyHitter> = summary
            .clients
            .iter()
            .take(8)
            .map(|(ip, c)| HeavyHitter {
                ip: ip.clone(),
                requests: c.requests,
                top_path: c.paths.first().map(|(p, _)| p.clone()).unwrap_or_default(),
                distinct_user_agents: c.distinct_user_agents,
                error_ratio: c.error_ratio(),
            })
            .collect();

        Self {
            node: node.to_string(),
            window_minutes,
            events_dropped: 0,
            heavy_hitters,
            total_requests: summary.total_requests,
            status: summary.status.clone(),
            notable,
            crawlers: summary
                .crawlers
                .iter()
                .map(|c| CrawlerReport {
                    user_agent: c.user_agent.clone(),
                    requests: c.requests,
                    client_ips: c.client_ips.clone(),
                    paths: c.paths.iter().map(|(p, _)| p.clone()).collect(),
                    first_seen: c.first_seen.clone(),
                    last_seen: c.last_seen.clone(),
                })
                .collect(),
            forged_bot_requests: summary.forged_bot_requests,
            forgers: summary.forgers.clone(),
            probe_noise,
            logging: LoggingHealth::read(),
            firewall: None,
        }
    }
}

#[cfg(test)]
mod traffic_tests {
    use super::*;

    fn row(ts: &str, ip: &str, path: &str, status: u16, ua: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","level":"INFO","fields":{{"message":"request","node":"origin","path":"{path}","status":{status},"client_ip":"{ip}","user_agent":"{ua}"}}}}"#
        )
    }

    #[test]
    fn summarises_a_stream_into_something_small() {
        let mut lines = vec![
            row(
                "2026-09-11T08:00:00Z",
                "1.2.3.4",
                "/",
                200,
                "Mozilla/5.0 Chrome/131",
            ),
            row(
                "2026-09-11T08:00:01Z",
                "5.6.7.8",
                "/robots.txt",
                200,
                "Mozilla/5.0 (compatible; ClaudeBot/1.0; +claudebot@anthropic.com)",
            ),
            row(
                "2026-09-11T08:00:02Z",
                "9.9.9.9",
                "/.git/config",
                404,
                "curl/8",
            ),
        ];
        // A real scan: three distinct probe paths from one address.
        for (i, p) in ["/.env", "/wp-admin/setup.php", "/@fs/etc/passwd"]
            .iter()
            .enumerate()
        {
            lines.push(row(
                &format!("2026-09-11T08:01:{:02}Z", i),
                "203.0.113.5",
                p,
                404,
                "curl/8",
            ));
        }
        let ndjson = lines.join("\n");
        let records: Vec<_> = crate::telemetry::parse_analytics(&ndjson, "").collect();
        let r = TrafficReport::from_records("origin", &records, 60);

        assert_eq!(r.total_requests, 6);
        assert_eq!(r.crawlers.len(), 1);
        assert!(r.crawlers[0].user_agent.contains("ClaudeBot"));
        assert_eq!(r.notable.len(), 1, "the scanner, not the single 404");
        assert_eq!(r.notable[0].ip, "203.0.113.5");
        assert_eq!(r.probe_noise, vec!["9.9.9.9 /.git/config".to_string()]);
        assert_eq!(r.status.get(&200), Some(&2));
        assert_eq!(r.status.get(&404), Some(&4));

        // The whole point: what goes over the wire is small.
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.len() < 2000, "summary was {} bytes", json.len());
    }

    /// Never having logged is not the same as having logged just now.
    #[test]
    fn logging_health_distinguishes_never_from_recently() {
        let never = LoggingHealth {
            events_total: 0,
            seconds_since_last: None,
        };
        assert!(never.is_blind());

        let alive = LoggingHealth {
            events_total: 5000,
            seconds_since_last: Some(3),
        };
        assert!(!alive.is_blind());

        // Silenced: the process is up and the main layer stopped.
        let quiet = LoggingHealth {
            events_total: 5000,
            seconds_since_last: Some(600),
        };
        assert!(
            quiet.is_blind(),
            "ten minutes of silence from a 10s heartbeat"
        );
    }

    /// A traffic document from a node on another version still parses.
    ///
    /// The aggregator reads this document from the journal, and a rollout has
    /// nodes on both sides of a field change at once. A node that publishes
    /// `user_agents` as the COUNT must stay readable here, and the count must
    /// keep that wire name so an aggregator older than the sample reads it
    /// too. Both directions, because a type error on one field loses the whole
    /// report: notable clients, logging health and firewall state with it.
    #[test]
    fn a_traffic_payload_without_the_agent_sample_still_parses() {
        let old = r#"{
            "node":"origin","window_minutes":60,"total_requests":12,"status":{"200":12},
            "notable":[{"ip":"203.0.113.9","requests":7,"distinct_user_agents":3,
                "status":{"404":7},"probe_paths":["/.env"],"injection_paths":[],
                "rotating_user_agents":false,
                "first_seen":"2026-10-04T01:20:54Z","last_seen":"2026-10-04T01:20:58Z"}],
            "heavy_hitters":[{"ip":"203.0.113.9","requests":7,"top_path":"/.env",
                "user_agents":3,"error_ratio":1.0}],
            "crawlers":[],"forged_bot_requests":0,"forgers":[],"probe_noise":[],
            "logging":{"events_total":100,"seconds_since_last":1}
        }"#;
        let r: TrafficReport = serde_json::from_str(old).expect("an older payload must parse");
        assert_eq!(r.heavy_hitters[0].distinct_user_agents, 3);
        assert!(
            r.notable[0].user_agents.is_empty(),
            "no sample from a node that cannot send one"
        );

        // And the count still goes out under the name an older reader expects.
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["heavy_hitters"][0]["user_agents"], 3);
    }

    /// `process_index` is in the body only when the process has one, so a node
    /// that does not set it answers as before. Break: always serialised, which
    /// changes every existing `/health` body.
    #[test]
    fn process_index_is_reported_only_when_set() {
        let (_, plain) = HealthReport::build("edge");
        let body = String::from_utf8(plain.into_response(200).2).unwrap();
        assert!(!body.contains("process_index"), "{body}");

        let (_, mut with) = HealthReport::build("edge");
        with.process_index = Some(2);
        let body = String::from_utf8(with.into_response(200).2).unwrap();
        assert!(body.contains("\"process_index\":2"), "{body}");
    }
}

/// The periodic metrics line carries the whole metrics document, so the
/// document's size is a log-budget fact: a line past `[limits.log_buffer] max`
/// is refused whole, and a refused metrics line is a node that stops reporting.
///
/// This builds the document with every section at its maximum cardinality and
/// measures it. The number is recorded in `docs/m6-log.md` and
/// `docs/m6-http.md`, so a reader who has to size a log budget does not have
/// to derive it.
#[cfg(test)]
mod perf_line_size_tests {
    use super::*;
    use crate::telemetry::{ChannelSnapshot, HandshakeStats, LoopTriggerSnapshot, StatsSnapshot};
    use crate::tls::{Fingerprint, TlsCertificate};

    fn handshake() -> HandshakeStats {
        HandshakeStats {
            samples: 1024,
            p50_ns: 1_234_567,
            p99_ns: 9_876_543,
            total: u64::MAX,
            mean_ns: 1_345_678,
            min_ns: 123_456,
            max_ns: 12_345_678,
        }
    }

    fn channel(version: &str, iface: &str) -> ChannelSnapshot {
        ChannelSnapshot {
            channel: format!("{version}/{iface}"),
            version: version.to_string(),
            iface: iface.to_string(),
            requests: u64::MAX,
            hits: u64::MAX,
            misses: u64::MAX,
            backend_errors: u64::MAX,
            hit_samples: 4096,
            hit_p50_ns: 1_234_567_890,
            hit_p99_ns: 1_234_567_890,
            miss_samples: 4096,
            miss_p50_ns: 1_234_567_890,
            miss_p99_ns: 1_234_567_890,
            handshake_full: handshake(),
            handshake_resumed: handshake(),
        }
    }

    /// Every section at its maximum: 500 status codes, six channels, sixteen
    /// event-loop triggers, three certificates, three pools, three URL
    /// backends, three named backend errors, and the largest integer values.
    fn maximal_report() -> PerfReport {
        let mut metrics = StatsSnapshot {
            requests_total: u64::MAX,
            cache_hits_total: u64::MAX,
            cache_misses_total: u64::MAX,
            cache_hits_valid_total: u64::MAX,
            cache_hits_invalid_total: u64::MAX,
            cache_misses_fetched_total: u64::MAX,
            cache_misses_answered_locally_total: u64::MAX,
            backend_errors_total: u64::MAX,
            monitor_requests_total: u64::MAX,
            monitor_samples: 4096,
            monitor_p50_ns: 1_234_567_890,
            monitor_p99_ns: 1_234_567_890,
            rps_peak: u64::MAX,
            hit_samples: 4096,
            hit_p50_ns: 1_234_567_890,
            hit_p99_ns: 1_234_567_890,
            hit_max_ns: 1_234_567_890,
            miss_samples: 4096,
            miss_p50_ns: 1_234_567_890,
            miss_p99_ns: 1_234_567_890,
            miss_max_ns: 1_234_567_890,
            ..StatsSnapshot::default()
        };
        for code in 100..=599u16 {
            metrics.status_counts.insert(code, u64::MAX);
        }
        for version in ["http/1.1", "http/2", "http/3"] {
            for iface in ["external", "internal"] {
                metrics.channels.push(channel(version, iface));
            }
        }
        for n in 0..16 {
            metrics.event_loop.push(LoopTriggerSnapshot {
                name: format!("connection_{n}_maximum"),
                count: u64::MAX,
                wait_min_ns: 1_234_567_890,
                wait_max_ns: 1_234_567_890,
                wait_mean_ns: 1_234_567_890,
                work_min_ns: 1_234_567_890,
                work_max_ns: 1_234_567_890,
                work_mean_ns: 1_234_567_890,
            });
        }
        for name in ["render-contact", "render-analytics", "m6-html"] {
            metrics
                .backend_errors_by_name
                .insert(name.to_string(), u64::MAX);
        }

        let certificate = TlsCertificate {
            depth: 2,
            not_after_unix: 1_800_000_000,
            expires_in_seconds: 30_000_000,
            fingerprint: Some(Fingerprint::of(b"a certificate's DER bytes")),
        };

        PerfReport {
            process_index: Some(15),
            node: "edge-b-with-a-long-name".to_string(),
            build: BuildId {
                name: "m6-http".to_string(),
                version: "1.15.0".to_string(),
                hash: "d41d8cd98f00b204e9800998ecf8427e".to_string(),
            },
            uptime_s: u64::MAX,
            pools: ["m6-html", "m6-file", "render-cms"]
                .iter()
                .map(|name| PoolHealth {
                    name: name.to_string(),
                    total: 4,
                })
                .collect(),
            url_backends: ["origin-that-has-a-long-name", "second-url-backend", "third"]
                .iter()
                .map(|name| name.to_string())
                .collect(),
            tls: vec![certificate, certificate, certificate],
            tls_unreadable: 0,
            logging: Some(LoggingHealth {
                events_total: u64::MAX,
                seconds_since_last: Some(u64::MAX),
            }),
            metrics,
            host: crate::platform::host::snapshot(std::path::Path::new("/")),
        }
    }

    #[test]
    fn the_maximal_document_is_well_under_the_log_buffer_maximum() {
        let document = super::tests_support::perf_document_of(&maximal_report());
        let bytes = serde_json::to_vec(&document).expect("the document serialises");
        // Measured at 20,477 bytes on 2026-11-17, with the sections above at
        // their maximum. The buffer's maximum is 1 MiB, so the margin is about
        // fifty times. A field that pushes this past a quarter of a megabyte
        // is a field that should not be in a log line.
        assert!(
            bytes.len() < 256 * 1024,
            "the maximal metrics document is {} bytes, past the quarter-megabyte \
             budget this test keeps; the log buffer refuses a line past 1 MiB, \
             and a refused metrics line is a node that stops reporting",
            bytes.len()
        );
        let host = serde_json::to_vec(&document["host"])
            .map(|v| v.len())
            .unwrap_or(0);
        let metrics = serde_json::to_vec(&document["metrics"])
            .map(|v| v.len())
            .unwrap_or(0);
        eprintln!(
            "maximal metrics document: {} bytes (host {}, metrics {})",
            bytes.len(),
            host,
            metrics
        );
    }
}

/// Helpers the size test shares with the document builders.
#[cfg(test)]
mod tests_support {
    use super::*;

    /// The report as the metrics line serialises it, with no generation or disk
    /// block: those are m6-http's own and are added to the value there.
    pub fn perf_document_of(report: &PerfReport) -> serde_json::Value {
        serde_json::to_value(report).unwrap_or_else(|_| serde_json::json!({"error": "x"}))
    }
}
