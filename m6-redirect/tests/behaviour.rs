//! m6-redirect's behaviour over its socket.
//!
//! The tests drive the binary the way the edge and the site's tool do: plain
//! HTTP/1.1 over the unix socket. They pin the redirect answers (301, 302,
//! 307, 308), the exact match (host case-insensitive, path exact, query
//! ignored), the 404 for an unknown rule, the list-replace POST and the
//! `POST /genid` reload.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: blocking clients drive the service from outside"
)]

use std::path::PathBuf;
use std::process::Command;

use m6_core::testkit::{binary, Service};

mod support;
use support::{get, head, post};

const RULES: &str = r#"{"items":[
  {"source_url":"example.com/r/cap7k9","target_url":"https://example.com/capabilities?utm_source=robert&utm_campaign=recruitmentremal","status_code":302},
  {"source_url":"example.com/r/permanent","target_url":"https://example.com/new","status_code":301},
  {"source_url":"example.com/r/temporary","target_url":"https://example.com/here","status_code":307},
  {"source_url":"example.com/r/other","target_url":"https://example.com/there","status_code":308}
]}"#;

struct Harness {
    _dir: tempfile::TempDir,
    sock: PathBuf,
    rules: PathBuf,
    svc: Service,
}

impl Harness {
    /// Start with the default rules file written and named in the config.
    fn start(extra_config: &str) -> Harness {
        Self::build(Some(RULES), extra_config)
    }

    /// Start with no rules file: the config names a path that does not exist.
    fn start_missing_rules() -> Harness {
        Self::build(None, "")
    }

    /// Start with a rules file the test supplies, or none when `rules` is
    /// `None`.
    fn build(rules: Option<&str>, extra_config: &str) -> Harness {
        let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
        let rules_path = dir.path().join("rules.json");
        if let Some(rules) = rules {
            std::fs::write(&rules_path, rules).expect("write the rules file");
        }
        let config = dir.path().join("redirect.conf");
        std::fs::write(
            &config,
            format!(
                "[redirect]\nrules_file = {:?}\n{extra_config}",
                rules_path.display().to_string()
            ),
        )
        .expect("write the config");
        std::fs::write(dir.path().join("site.toml"), "").unwrap();
        let sock = dir.path().join("redir.sock");

        let mut cmd = Command::new(binary("m6-redirect"));
        cmd.arg(dir.path())
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &sock);
        let mut svc = Service::spawn("m6-redirect", &mut cmd);
        svc.wait_for_path(&sock, m6_core::testkit::STARTUP_BOUND);
        svc.wait_for_started(m6_core::testkit::STARTUP_BOUND);

        Harness {
            _dir: dir,
            sock,
            rules: rules_path,
            svc,
        }
    }

    fn stop(&mut self) {
        self.svc.terminate(m6_core::testkit::STARTUP_BOUND);
    }
}

#[test]
fn the_four_redirect_statuses_answer_with_the_rule_target() {
    let mut h = Harness::start("");

    for (code, target) in [
        (
            302,
            "https://example.com/capabilities?utm_source=robert&utm_campaign=recruitmentremal",
        ),
        (301, "https://example.com/new"),
        (307, "https://example.com/here"),
        (308, "https://example.com/there"),
    ] {
        let path = match code {
            302 => "/r/cap7k9",
            301 => "/r/permanent",
            307 => "/r/temporary",
            _ => "/r/other",
        };
        let resp = get(&h.sock, "example.com", path);
        assert_eq!(resp.status, code, "GET {path}: {}", resp.raw);
        assert_eq!(
            resp.header("location").as_deref(),
            Some(target),
            "GET {path}: {}",
            resp.raw
        );
    }
    h.stop();
}

#[test]
fn the_redirect_is_marked_storable_with_the_shared_lifetime() {
    let mut h = Harness::start("max_age_secs = 3600\nstale_while_revalidate_secs = 60");
    let resp = get(&h.sock, "example.com", "/r/cap7k9");
    assert_eq!(resp.status, 302);
    assert_eq!(
        resp.header("cache-control").as_deref(),
        Some("public, s-maxage=3600, stale-while-revalidate=60"),
        "{}",
        resp.raw
    );
    h.stop();
}

#[test]
fn an_unregistered_path_answers_404_with_no_location() {
    let mut h = Harness::start("");
    let resp = get(&h.sock, "example.com", "/r/nonesuch");
    assert_eq!(resp.status, 404, "{}", resp.raw);
    assert!(resp.header("location").is_none(), "{}", resp.raw);
    h.stop();
}

#[test]
fn matching_is_exact_and_the_path_is_case_sensitive() {
    let mut h = Harness::start("");
    // A suffix does not match, a wrong path does not match.
    for path in ["/r/cap7k9x", "/r/cap7k9/", "/R/CAP7K9", "/cap7k9"] {
        let resp = get(&h.sock, "example.com", path);
        assert_eq!(resp.status, 404, "GET {path}: {}", resp.raw);
    }
    // The host is case-insensitive.
    let resp = get(&h.sock, "EXAMPLE.COM", "/r/cap7k9");
    assert_eq!(resp.status, 302, "{}", resp.raw);
    h.stop();
}

#[test]
fn the_query_is_ignored() {
    let mut h = Harness::start("");
    let resp = get(&h.sock, "example.com", "/r/cap7k9?utm_source=other");
    assert_eq!(resp.status, 302, "{}", resp.raw);
    assert!(
        resp.header("location")
            .unwrap()
            .contains("utm_source=robert"),
        "the stored UTM values win: {}",
        resp.raw
    );
    h.stop();
}

#[test]
fn a_head_answers_the_redirect_with_no_body() {
    let mut h = Harness::start("");
    let resp = head(&h.sock, "example.com", "/r/cap7k9");
    assert_eq!(resp.status, 302, "{}", resp.raw);
    assert!(resp.header("location").is_some(), "{}", resp.raw);
    h.stop();
}

#[test]
fn the_list_replace_writes_verbatim_and_swaps_the_table() {
    let mut h = Harness::start("");
    let body = r#"{"items":[{"source_url":"example.com/r/newcode","target_url":"https://example.com/fresh","status_code":302}]}"#;
    let resp = post(&h.sock, "/m6-redirect/rules", body);
    assert_eq!(resp.status, 204, "{}", resp.raw);

    // The file holds the body byte for byte.
    assert_eq!(std::fs::read(&h.rules).unwrap(), body.as_bytes());

    // The new rule answers at once, and the old rules are gone.
    let now = get(&h.sock, "example.com", "/r/newcode");
    assert_eq!(now.status, 302, "{}", now.raw);
    assert_eq!(
        now.header("location").as_deref(),
        Some("https://example.com/fresh"),
        "{}",
        now.raw
    );
    let old = get(&h.sock, "example.com", "/r/cap7k9");
    assert_eq!(old.status, 404, "{}", old.raw);
    h.stop();
}

#[test]
fn a_malformed_list_is_refused_and_changes_nothing() {
    let mut h = Harness::start("");

    let resp = post(&h.sock, "/m6-redirect/rules", "not json");
    assert_eq!(resp.status, 400, "{}", resp.raw);

    let resp = post(&h.sock, "/m6-redirect/rules", r#"{"items":"x"}"#);
    assert_eq!(resp.status, 400, "{}", resp.raw);

    // The table still serves the rules it started with.
    let still = get(&h.sock, "example.com", "/r/cap7k9");
    assert_eq!(still.status, 302, "{}", still.raw);
    h.stop();
}

#[test]
/// m6-redirect logs one access line per request it answers: what was
/// received (host, path, query) and where the user was sent (status and
/// Location). A 404 has no Location, and a 302 carries its target.
#[test]
fn a_redirect_logs_its_query_and_target_and_a_404_logs_no_location() {
    let mut h = Harness::start("");
    let hit = get(&h.sock, "example.com", "/r/cap7k9?ref=newsletter");
    assert_eq!(hit.status, 302, "{}", hit.raw);
    let miss = get(&h.sock, "example.com", "/r/nope");
    assert_eq!(miss.status, 404, "{}", miss.raw);

    // SIGTERM flushes the log, so the lines are in the captured output after
    // a clean stop.
    h.stop();
    let out = h.svc.output();
    assert!(
        out.contains("\"message\":\"redirect\""),
        "no redirect line\n--- output ---\n{out}"
    );
    assert!(out.contains("\"path\":\"/r/cap7k9\""), "{out}");
    assert!(out.contains("\"query\":\"ref=newsletter\""), "{out}");
    assert!(out.contains("\"status\":302"), "{out}");
    assert!(
        out.contains(
            "\"location\":\"https://example.com/capabilities?utm_source=robert&utm_campaign=recruitmentremal\""
        ),
        "{out}"
    );
    // The 404 has no Location on its line: the only location in the output is
    // the 302's.
    let location_lines = out.lines().filter(|l| l.contains("\"location\"")).count();
    assert_eq!(location_lines, 1, "only the 302 line carries a location:\n{out}");
    assert!(out.contains("\"status\":404"), "{out}");
}

fn the_reload_signal_rereads_the_file_and_swaps_the_table() {
    let mut h = Harness::start("");
    // A deploy can update the file directly, then send the backend's reload
    // signal. The service reads the file again and serves the new list.
    std::fs::write(
        &h.rules,
        r#"{"items":[{"source_url":"example.com/r/reloaded","target_url":"https://example.com/again","status_code":308}]}"#,
    )
    .unwrap();
    let resp = post(&h.sock, "/genid", "");
    assert_eq!(resp.status, 204, "{}", resp.raw);

    let now = get(&h.sock, "example.com", "/r/reloaded");
    assert_eq!(now.status, 308, "{}", now.raw);
    assert_eq!(
        now.header("location").as_deref(),
        Some("https://example.com/again"),
        "{}",
        now.raw
    );
    let old = get(&h.sock, "example.com", "/r/cap7k9");
    assert_eq!(old.status, 404, "{}", old.raw);
    h.stop();
}

#[test]
fn a_missing_rules_file_starts_empty_and_serves_404() {
    let mut h = Harness::start_missing_rules();
    let resp = get(&h.sock, "example.com", "/r/cap7k9");
    assert_eq!(resp.status, 404, "{}", resp.raw);

    let body = r#"{"items":[{"source_url":"example.com/r/newcode","target_url":"https://example.com/fresh","status_code":302}]}"#;
    let replaced = post(&h.sock, "/m6-redirect/rules", body);
    assert_eq!(replaced.status, 204, "{}", replaced.raw);
    let now = get(&h.sock, "example.com", "/r/newcode");
    assert_eq!(now.status, 302, "{}", now.raw);
    h.stop();
}
