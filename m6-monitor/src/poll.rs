//! Fetching one node's view of itself.
//!
//! Two requests per node: `/health` for the verdict, `/perf` for everything
//! else. They are separate endpoints on purpose (see `m6_core::monitoring`),
//! and an aggregator is exactly the caller that wants both.

use std::time::{Duration, Instant};

use m6_core::monitoring::{HealthReport, PerfReport, TrafficReport};

use crate::fleet::Node;

/// What one node answered, and what it cost to ask.
#[derive(Debug, Clone)]
pub struct NodeReading {
    pub name: String,
    pub role: String,
    pub url: String,
    /// `/health`: present unless the node could not be reached at all.
    pub health: Option<HealthReport>,
    /// HTTP status from `/health`. 503 is a degraded node answering honestly,
    /// which is different from no answer.
    pub health_status: Option<u16>,
    /// `/perf`: absent when no token is configured, when the node refused the
    /// one we have, or when it is switched off there.
    pub perf: Option<PerfReport>,
    /// Why `/perf` is absent, when it is. An aggregator that silently shows
    /// nothing for a node is indistinguishable from a healthy quiet node.
    pub perf_error: Option<String>,
    /// Round trip for `/health`, measured from here, **including connect and
    /// TLS**.
    ///
    /// This is a network measurement, not the node's own latency. It is the
    /// cost of reaching the node from wherever the monitor runs, and over a
    /// long link the handshake dominates it: from the build host in Sydney,
    /// syd answers in 27ms and lon in 828ms, and almost all of that difference
    /// is the handshake rather than anything either node did. Do not compare
    /// it with the loopback TTFB, which measures the opposite thing.
    pub rtt: Option<Duration>,
    /// `/traffic`: this node's summary of its own traffic and whether its
    /// logging is alive. Absent for the same reasons `/perf` can be.
    pub traffic: Option<TrafficReport>,
    pub traffic_error: Option<String>,
    /// Set when the node could not be reached at all.
    pub unreachable: Option<String>,
}

impl NodeReading {
    pub fn is_up(&self) -> bool {
        self.unreachable.is_none()
    }

    /// Whether the node says it can serve. A node that cannot be reached is
    /// not "ok" and is not "degraded" either; it is unknown, and saying so is
    /// the honest answer.
    pub fn status(&self) -> &str {
        match (&self.unreachable, &self.health) {
            (Some(_), _) => "unreachable",
            (None, Some(h)) => h.status.as_str(),
            (None, None) => "unknown",
        }
    }
}

/// One agent per node, reused for both of that node's requests.
///
/// It was one agent per *request*, which meant a cold TLS handshake for each.
/// Measured from the build host: 26ms to syd, 603ms to chi, 828ms to lon, and
/// that is the handshake, not the node. Two requests per node paid it twice.
///
/// Not shared across nodes, because ureq pools per host anyway and a shared
/// agent would serialise nothing useful while making the failure of one node
/// harder to attribute.
fn agent_for(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(timeout)
        // A monitor that follows redirects can be walked somewhere else by a
        // node it is supposed to be observing.
        .redirects(0)
        .build()
}

fn get(
    agent: &ureq::Agent,
    url: &str,
    token: Option<&str>,
) -> anyhow::Result<(u16, String)> {
    let mut req = agent.get(url);
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.call() {
        Ok(resp) => {
            let code = resp.status();
            Ok((code, resp.into_string()?))
        }
        // ureq treats 4xx and 5xx as errors. They are answers, and a 503 from
        // /health is the single most informative answer this tool can get.
        Err(ureq::Error::Status(code, resp)) => Ok((code, resp.into_string().unwrap_or_default())),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}

/// Poll one node.
pub fn node(n: &Node, fleet_token: Option<&str>, timeout: Duration) -> NodeReading {
    // Per-node token first: the nodes do not share one.
    let token = n.perf_token(fleet_token);
    let token = token.as_deref();
    let mut reading = NodeReading {
        name: n.name.clone(),
        role: n.role.clone(),
        url: n.url.clone(),
        health: None,
        health_status: None,
        perf: None,
        perf_error: None,
        traffic: None,
        traffic_error: None,
        rtt: None,
        unreachable: None,
    };

    let agent = agent_for(timeout);

    // First request on this agent, so the RTT below includes connect and TLS.
    // That is deliberate and is what `rtt` documents: it is the cost of
    // reaching the node, which is the thing a fleet view wants. `/perf` below
    // reuses the connection and is not timed.
    let started = Instant::now();
    match get(&agent, &format!("{}/health", n.url.trim_end_matches('/')), None) {
        Ok((code, body)) => {
            reading.rtt = Some(started.elapsed());
            reading.health_status = Some(code);
            match serde_json::from_str::<HealthReport>(&body) {
                Ok(h) => reading.health = Some(h),
                Err(e) => {
                    reading.unreachable =
                        Some(format!("/health answered {code} but did not parse: {e}"))
                }
            }
        }
        Err(e) => {
            reading.unreachable = Some(format!("{e}"));
            return reading;
        }
    }

    let token = match token {
        Some(t) => t,
        None => {
            // Both gated endpoints are out of reach, and both must say so.
            // A blank where a summary should be is indistinguishable from a
            // quiet hour, and the standing order is explicit that a node
            // whose logging cannot be checked is not "all clear".
            reading.perf_error = Some("no perf token configured".to_string());
            reading.traffic_error = Some("no perf token configured".to_string());
            return reading;
        }
    };

    let base = n.url.trim_end_matches('/');

    match get(&agent, &format!("{base}/perf"), Some(token)) {
        Ok((200, body)) => match serde_json::from_str::<PerfReport>(&body) {
            Ok(p) => reading.perf = Some(p),
            Err(e) => reading.perf_error = Some(format!("unparseable: {e}")),
        },
        Ok((401, _)) => {
            reading.perf_error = Some("401: token rejected by this node".to_string())
        }
        Ok((404, _)) => {
            reading.perf_error = Some("404: /perf not enabled on this node".to_string())
        }
        Ok((code, _)) => reading.perf_error = Some(format!("unexpected status {code}")),
        Err(e) => reading.perf_error = Some(format!("{e}")),
    }

    match get(&agent, &format!("{base}/traffic"), Some(token)) {
        Ok((200, body)) => match serde_json::from_str::<TrafficReport>(&body) {
            Ok(t) => reading.traffic = Some(t),
            Err(e) => reading.traffic_error = Some(format!("unparseable: {e}")),
        },
        Ok((404, _)) => {
            // An older node without the endpoint. Worth saying, because a
            // silent absence is indistinguishable from a quiet hour.
            reading.traffic_error =
                Some("404: /traffic not available on this node".to_string())
        }
        Ok((503, body)) => {
            let why = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["error"].as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "unreadable".to_string());
            reading.traffic_error = Some(format!("503: {why}"))
        }
        Ok((code, _)) => reading.traffic_error = Some(format!("unexpected status {code}")),
        Err(e) => reading.traffic_error = Some(format!("{e}")),
    }

    reading
}

/// Poll every node, one thread each.
///
/// In parallel because a fleet report should cost one timeout, not one per
/// node: three nodes behind a five second timeout is fifteen seconds of a
/// human waiting, and the slow case is exactly when someone is watching.
pub fn fleet(nodes: &[Node], token: Option<&str>, timeout: Duration) -> Vec<NodeReading> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = nodes
            .iter()
            .map(|n| scope.spawn(move || node(n, token, timeout)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| NodeReading {
                    name: "?".to_string(),
                    role: String::new(),
                    url: String::new(),
                    health: None,
                    health_status: None,
                    perf: None,
                    perf_error: None,
                    traffic: None,
                    traffic_error: None,
                    rtt: None,
                    unreachable: Some("poll thread panicked".to_string()),
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(unreachable: Option<&str>, status: Option<&str>) -> NodeReading {
        NodeReading {
            name: "syd".into(),
            role: "origin".into(),
            url: "http://x".into(),
            health: status.map(|s| HealthReport {
                status: s.to_string(),
                node: "sydney".into(),
            }),
            health_status: None,
            perf: None,
            perf_error: None,
            traffic: None,
            traffic_error: None,
            rtt: None,
            unreachable: unreachable.map(|s| s.to_string()),
        }
    }

    /// A node that cannot be reached is not healthy and is not degraded.
    /// Collapsing "no answer" into either one is how a dead node gets reported
    /// as fine, or a working fleet gets reported as broken.
    #[test]
    fn unreachable_is_its_own_state() {
        assert_eq!(reading(Some("connection refused"), None).status(), "unreachable");
        assert!(!reading(Some("timeout"), None).is_up());
        assert_eq!(reading(None, Some("ok")).status(), "ok");
        assert_eq!(reading(None, Some("degraded")).status(), "degraded");
        assert_eq!(reading(None, None).status(), "unknown");
    }
}
