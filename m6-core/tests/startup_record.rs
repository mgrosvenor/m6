//! The start-up and shutdown recovery record, tested through the interface an
//! application uses.
//!
//! Every test writes a real record into a file Sink and reads the JSONL back,
//! so the assertions are on the bytes an operator and a monitor see. No test
//! inspects a private builder.

use std::path::{Path, PathBuf};

use m6_core::startup::{
    self, ConfigRole, ConfigTrace, EnvDecl, FlagDecl, HostReport, Invocation, LimitsReport, Node,
    NodeSource, Record, RunId, Setting, Source,
};
use m6_core::Secret;
use serde_json::Value;

fn write(record: &Record) -> (Vec<Value>, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("record.jsonl");
    let mut sink = m6_core::log::Sink::file(&path).expect("a file sink");
    record.write(&mut sink);
    sink.finish().expect("the sink flushes");
    let text = std::fs::read_to_string(&path).expect("the record is a text file");
    let lines = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is JSON"))
        .collect();
    (lines, text)
}

fn field<'a>(line: &'a Value, key: &str) -> &'a Value {
    // The operational log wraps a line's own fields in `fields`. `type`,
    // `level`, `timestamp` and `target` are the line envelope and stay at the
    // top level, which is the same shape every other m6 log line has.
    let at = match key {
        "type" | "level" | "timestamp" | "target" => line,
        _ => line
            .get("fields")
            .unwrap_or_else(|| panic!("line has no fields: {line}")),
    };
    at.get(key)
        .unwrap_or_else(|| panic!("line has no {key}: {line}"))
}

fn part(line: &Value) -> &str {
    field(line, "part").as_str().expect("part is a string")
}

fn settings<'a>(lines: &'a [Value], section: &str) -> &'a Value {
    let line = lines
        .iter()
        .find(|line| part(line) == "config_section" && field(line, "section") == section)
        .unwrap_or_else(|| panic!("no config_section line for {section}"));
    field(line, "settings")
}

fn setting<'a>(lines: &'a [Value], section: &str, key: &str) -> &'a Value {
    settings(lines, section)
        .get(key)
        .unwrap_or_else(|| panic!("section {section} has no key {key}"))
}

const CONFIG: &str = r#"
[log]
level = "debug"

[params_cache]
size = 512

[server]
socket_mode = "0700"

[[route]]
path = "/"
template = "index.html"

[[route]]
path = "/about"
template = "about.html"
cache = "no-store"

[limits.log_buffer]
start = "32KiB"
max = "64KiB"

[limits.log_field]
max = "16KiB"
"#;

fn fixture(dir: &Path, body: &str) -> (PathBuf, m6_core::config::RendererConfig, ConfigTrace) {
    let path = dir.join("m6-service.conf");
    std::fs::write(&path, body).expect("write the config");
    let (config, trace) =
        m6_core::config::load_traced(&path, dir).expect("the fixture config loads");
    (path, config, trace)
}

fn record_from(
    name: &str,
    dir: &Path,
    config: &m6_core::config::RendererConfig,
    trace: &ConfigTrace,
    argv: &[String],
    flags: &[FlagDecl],
    env: &[EnvDecl],
) -> Record {
    let mut trace = trace.clone();
    let invocation = Invocation::read(argv, flags, env, &mut trace);
    let mut sections = config.startup_sections(dir, &trace);
    for section in &mut sections {
        section.file = Some("m6-service.conf".to_string());
    }
    let limits = LimitsReport::read(&config.limits, &trace);
    Record::new(name, argv.to_vec())
        .invocation(invocation, trace.clone())
        .sections(sections)
        .host(HostReport::unknown("a test host knows nothing"))
        .limits(limits)
}

#[test]
fn the_record_is_several_self_contained_startup_lines() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);
    let argv = vec![
        "m6-service".to_string(),
        "--log-level".to_string(),
        "warn".to_string(),
    ];
    let flags = [FlagDecl {
        name: "--log-level",
        takes_value: true,
        setting: Some("log.level"),
        secret: false,
    }];
    let mut record = record_from(
        "m6-service",
        dir.path(),
        &config,
        &trace,
        &argv,
        &flags,
        &[],
    );
    // Apply the flag the way a service does, so the record shows the level in
    // use rather than the level the table happened to carry.
    let mut sections = record.recorded_sections().to_vec();
    sections
        .iter_mut()
        .find(|s| s.path == "log")
        .unwrap()
        .set("level", Setting::new("warn", Source::Flag));
    record = record.sections(sections);

    let (lines, _) = write(&record);

    assert!(
        lines.len() > 4,
        "the record is several lines: {}",
        lines.len()
    );
    for line in &lines {
        assert_eq!(field(line, "type"), "startup", "every line is type startup");
        assert_eq!(
            field(line, "app"),
            "m6-service",
            "every line is self contained"
        );
        assert!(field(line, "version").as_str().unwrap().contains('-'));
        assert!(field(line, "run_id").as_str().unwrap().contains('-'));
        assert_eq!(line.get("level").unwrap(), "INFO");
    }

    let parts: Vec<&str> = lines.iter().map(part).collect();
    assert_eq!(parts.first(), Some(&"identity"));
    assert_eq!(parts.get(1), Some(&"invocation"));
    assert_eq!(parts.last(), Some(&"complete"));
    assert_eq!(
        parts.iter().filter(|p| **p == "platform").count(),
        1,
        "one platform line"
    );
    assert!(parts.contains(&"config_file"));
    assert!(parts.iter().filter(|p| **p == "config_section").count() >= 5);

    let seqs: Vec<u64> = lines
        .iter()
        .map(|line| field(line, "seq").as_u64().unwrap())
        .collect();
    assert_eq!(seqs, (1..=lines.len() as u64).collect::<Vec<_>>());

    // The completeness line counts itself, so a reader that received every
    // line can prove it and a reader that missed one knows.
    let complete = lines.last().unwrap();
    assert_eq!(
        field(complete, "lines").as_u64().unwrap(),
        lines.len() as u64
    );

    // The identity line carries the same data as --version-full.
    let identity = &lines[0];
    let crates = field(identity, "crates").as_array().unwrap();
    assert!(crates
        .iter()
        .any(|c| c.as_str().unwrap().starts_with("m6-core - ")));
    assert!(field(identity, "tag").as_str().unwrap().starts_with('v'));
}

#[test]
fn the_run_id_groups_one_record_and_not_two() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);
    let argv = vec!["m6-service".to_string()];

    let first = record_from("m6-service", dir.path(), &config, &trace, &argv, &[], &[])
        .with_run_id(RunId::new(1_700_000_000_000, 42));
    let second = record_from("m6-service", dir.path(), &config, &trace, &argv, &[], &[])
        .with_run_id(RunId::new(1_700_000_000_001, 43));

    let (a, _) = write(&first);
    let (b, _) = write(&second);
    let ids: std::collections::BTreeSet<String> = a
        .iter()
        .map(|line| field(line, "run_id").as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 1, "one record has one run id: {ids:?}");
    assert_eq!(ids.iter().next().unwrap(), "1700000000000-42");
    let other: std::collections::BTreeSet<String> = b
        .iter()
        .map(|line| field(line, "run_id").as_str().unwrap().to_string())
        .collect();
    assert_eq!(other.iter().next().unwrap(), "1700000000001-43");
    assert!(ids.is_disjoint(&other), "two runs never share an id");
}

#[test]
fn no_secret_value_appears_in_any_line() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets.toml");
    std::fs::write(
        &secrets,
        r#"
[smtp]
password = "hunter2-must-never-be-logged"
username = "mailer"

[analytics]
api_token = "tok-must-never-be-logged"
"#,
    )
    .unwrap();
    let body = format!(
        "secrets_file = {:?}\n\n[server]\nsocket_mode = \"0660\"\n",
        secrets.display()
    );
    let (_, config, trace) = fixture(dir.path(), &body);

    let record = record_from(
        "m6-service",
        dir.path(),
        &config,
        &trace,
        &["m6-service".to_string()],
        &[],
        &[],
    );
    let (lines, text) = write(&record);

    for secret in ["hunter2-must-never-be-logged", "tok-must-never-be-logged"] {
        assert!(
            !text.contains(secret),
            "the secret {secret:?} must appear in no line:\n{text}"
        );
    }
    // The path is a setting and is logged. The value is not.
    assert_eq!(
        setting(&lines, "smtp", "password")["value"],
        "<redacted>",
        "a value the secrets file supplied is redacted"
    );
    assert_eq!(
        setting(&lines, "smtp", "password")["source"]
            .as_str()
            .unwrap(),
        "file"
    );
    assert_eq!(
        setting(&lines, "smtp", "username")["value"]
            .as_str()
            .unwrap(),
        "<redacted>",
        "every value the secrets file supplies is redacted, because the file's role is the type"
    );

    // Redaction is by type as well: a field declared Secret<T> cannot be
    // written even when the value is in plain sight.
    #[derive(serde::Serialize)]
    struct Typed {
        token: Secret<String>,
    }
    let typed = serde_json::to_value(Typed {
        token: Secret::new("typed-secret-must-never-be-logged".to_string()),
    })
    .unwrap();
    let sections = startup::sections_from_value("typed", &typed, &ConfigTrace::new());
    let section = sections.first().unwrap();
    assert_eq!(section.settings["token"].value, "<redacted>");
    let rendered = serde_json::to_string(&typed).unwrap();
    assert!(!rendered.contains("typed-secret-must-never-be-logged"));
}

#[test]
fn every_setting_carries_its_source() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);
    let flags = [FlagDecl {
        name: "--log-level",
        takes_value: true,
        setting: Some("log.level"),
        secret: false,
    }];
    let env = [EnvDecl {
        name: "M6_SOCKET_OVERRIDE",
        why: "replaces the service's socket path",
        setting: Some("server.socket_path"),
    }];
    let socket = dir.path().join("override.sock");
    // The environment is process-wide, so this test owns it while it runs.
    std::env::set_var("M6_SOCKET_OVERRIDE", &socket);
    let argv = vec![
        "m6-service".to_string(),
        "--log-level".to_string(),
        "warn".to_string(),
    ];
    let mut trace = trace;
    let invocation = Invocation::read(&argv, &flags, &env, &mut trace);
    let socket_value = invocation.env("M6_SOCKET_OVERRIDE").unwrap().to_string();
    let mut sections = config.startup_sections(dir.path(), &trace);
    sections
        .iter_mut()
        .find(|s| s.path == "log")
        .unwrap()
        .set("level", Setting::new("warn", trace.source("log.level")));
    sections
        .iter_mut()
        .find(|s| s.path == "server")
        .unwrap()
        .set(
            "socket_path",
            Setting::new(socket_value.clone(), trace.source("server.socket_path")),
        );
    let record = Record::new("m6-service", argv)
        .invocation(invocation, trace.clone())
        .sections(sections)
        .host(HostReport::unknown("none"))
        .limits(LimitsReport::read(&config.limits, &trace));
    std::env::remove_var("M6_SOCKET_OVERRIDE");

    let (lines, _) = write(&record);
    // file
    assert_eq!(setting(&lines, "server", "socket_mode")["value"], "0700");
    assert_eq!(setting(&lines, "server", "socket_mode")["source"], "file");
    // default, present rather than omitted
    assert_eq!(
        setting(&lines, "minification", "text/html"),
        &serde_json::json!({"value": true, "source": "default"})
    );
    // env
    assert_eq!(
        setting(&lines, "server", "socket_path"),
        &serde_json::json!({"value": socket.display().to_string(), "source": "env"})
    );
    // flag
    assert_eq!(
        setting(&lines, "log", "level"),
        &serde_json::json!({"value": "warn", "source": "flag"})
    );

    // The invocation line names every declared variable and flag, including
    // the unset ones, so "not set" and "never read" look different.
    let invocation_line = lines.iter().find(|l| part(l) == "invocation").unwrap();
    let environment = field(invocation_line, "environment").as_array().unwrap();
    let declared = environment
        .iter()
        .find(|e| e["name"] == "M6_SOCKET_OVERRIDE")
        .unwrap();
    assert_eq!(declared["value"], socket.display().to_string());
    let flags = field(invocation_line, "flags").as_array().unwrap();
    assert!(flags
        .iter()
        .any(|f| f["name"] == "--log-level" && f["value"] == "warn"));
}

#[test]
fn the_config_files_are_hashed_as_read() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets.toml");
    std::fs::write(&secrets, "[smtp]\npassword = \"x\"\n").unwrap();
    let body = format!(
        "secrets_file = {:?}\n[server]\nsocket_mode = \"0660\"\n",
        secrets.display()
    );
    let (config_path, config, trace) = fixture(dir.path(), &body);
    let record = record_from(
        "m6-service",
        dir.path(),
        &config,
        &trace,
        &["m6-service".to_string()],
        &[],
        &[],
    );
    let (lines, _) = write(&record);
    let files: Vec<&Value> = lines.iter().filter(|l| part(l) == "config_file").collect();
    assert_eq!(files.len(), 2);
    let config_text = std::fs::read_to_string(&config_path).unwrap();
    assert_eq!(field(files[0], "role").as_str().unwrap(), "config");
    assert_eq!(
        field(files[0], "sha256").as_str().unwrap(),
        startup::sha256(&config_text)
    );
    assert_eq!(
        field(files[0], "path").as_str().unwrap(),
        config_path.display().to_string()
    );
    assert_eq!(field(files[1], "role").as_str().unwrap(), "secrets");
    let secrets_text = std::fs::read_to_string(&secrets).unwrap();
    assert_eq!(
        field(files[1], "sha256").as_str().unwrap(),
        startup::sha256(&secrets_text)
    );
}

#[test]
fn a_config_rebuilt_from_the_record_loads_with_the_same_effective_settings() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);
    let record = record_from(
        "m6-service",
        dir.path(),
        &config,
        &trace,
        &["m6-service".to_string()],
        &[],
        &[],
    );

    // The record's settings, written back as a config file, with the secrets
    // file it never carried left out. This is the operator's recovery path.
    let limits = LimitsReport::read(&config.limits, &trace);
    let rebuilt = rebuild_config(record.recorded_sections(), &limits, &config);
    let rebuilt_path = dir.path().join("rebuilt.conf");
    std::fs::write(&rebuilt_path, toml::to_string(&rebuilt).unwrap()).unwrap();

    let original = config.effective_json(dir.path());
    let (reloaded, _) =
        m6_core::config::load_traced(&rebuilt_path, dir.path()).expect("the rebuilt config loads");
    let effective = reloaded.effective_json(dir.path());

    assert_eq!(
        effective, original,
        "a config rebuilt from the record has the same effective settings, defaults included"
    );

    // The rebuilt config also keeps a value the original file set, rather than
    // falling back to the default that happened to equal it.
    let rebuilt_text = std::fs::read_to_string(&rebuilt_path).unwrap();
    assert!(rebuilt_text.contains("socket_mode"), "{rebuilt_text}");
    assert!(rebuilt_text.contains("size = 512"), "{rebuilt_text}");
}

#[test]
fn a_host_that_knows_nothing_still_makes_a_complete_record() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);
    let record = record_from(
        "m6-service",
        dir.path(),
        &config,
        &trace,
        &["m6-service".to_string()],
        &[],
        &[],
    );
    let (lines, _) = write(&record);
    assert_eq!(
        field(lines.last().unwrap(), "lines").as_u64().unwrap(),
        lines.len() as u64
    );
    let platform = lines.iter().find(|l| part(l) == "platform").unwrap();
    assert_eq!(
        field(platform, "host")["unknown"],
        "a test host knows nothing",
        "an unknown fact carries its reason instead of being omitted"
    );
    // Every limit is still reported, so the line is complete with nothing
    // known about the host at all.
    let limits = field(platform, "limits").as_object().unwrap();
    assert!(limits.contains_key("journal_line"));
    assert_eq!(limits["journal_line"]["source"], "default");
}

#[test]
fn a_shutdown_line_shares_the_run_id_and_carries_uptime_and_reason() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shutdown.jsonl");
    let mut sink = m6_core::log::Sink::file(&path).unwrap();
    let run = RunId::new(1_700_000_000_000, 42);
    startup::write_shutdown_into(
        &mut sink,
        run,
        "m6-service",
        "1.16.0-rc.1+abc",
        &Node::named("origin"),
        1_234,
        "signal",
    );
    sink.finish().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let line: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(field(&line, "type"), "shutdown");
    assert_eq!(field(&line, "run_id"), "1700000000000-42");
    assert_eq!(field(&line, "app"), "m6-service");
    assert_eq!(field(&line, "version"), "1.16.0-rc.1+abc");
    assert_eq!(field(&line, "node"), "origin");
    assert_eq!(field(&line, "uptime_ms"), 1_234);
    assert_eq!(field(&line, "reason"), "signal");
}

/// A host report whose platform facts name `name`.
fn host_named(name: &str) -> HostReport {
    HostReport::new(serde_json::json!({
        "hostname": {"known": name},
        "kernel": {"known": {"release": "6.11.0-1", "version": "#1"}},
    }))
}

/// The record's node is read once from the platform facts' hostname, and every
/// part, the shutdown line included, states that same value.
#[test]
fn every_part_of_a_record_states_the_one_node_read_from_the_platform_facts() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);
    let record = record_from(
        "m6-service",
        dir.path(),
        &config,
        &trace,
        &["m6-service".to_string()],
        &[],
        &[],
    )
    .host(host_named("origin"));

    let (lines, _) = write(&record);

    let platform = lines.iter().find(|l| part(l) == "platform").unwrap();
    let hostname = field(platform, "host")["hostname"]["known"]
        .as_str()
        .expect("the platform line names the host");
    assert_eq!(hostname, "origin");
    let parts: std::collections::BTreeSet<&str> = lines.iter().map(part).collect();
    let expected: std::collections::BTreeSet<&str> = [
        "identity",
        "invocation",
        "config_file",
        "config_section",
        "platform",
        "complete",
    ]
    .into_iter()
    .collect();
    assert_eq!(parts, expected, "every part is held");
    for line in &lines {
        assert_eq!(
            field(line, "node"),
            hostname,
            "every part carries the platform facts' hostname: {line}"
        );
        assert_eq!(
            field(line, "node_source"),
            "hostname",
            "the application named no node, so the source says the host did: {line}"
        );
        assert!(
            line["fields"].get("node_unknown").is_none(),
            "a named node carries no reason: {line}"
        );
    }
    assert_eq!(record.node(), &Node::host("origin"));

    // The shutdown line carries the same node the start-up lines did. The
    // process's own shutdown reads it from the run's identity, which is fixed
    // by the record's first write.
    let path = dir.path().join("shutdown.jsonl");
    let mut sink = m6_core::log::Sink::file(&path).unwrap();
    startup::write_shutdown_into(
        &mut sink,
        record.run_id(),
        record.app(),
        &m6_core::build_info::identity().full(),
        record.node(),
        32,
        "signal",
    );
    sink.finish().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let shutdown: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(field(&shutdown, "type"), "shutdown");
    assert_eq!(
        field(&shutdown, "node"),
        field(&lines[0], "node"),
        "the shutdown line carries the node the start-up lines carried"
    );
    assert_eq!(field(&shutdown, "node"), "origin");
    assert_eq!(field(&shutdown, "node_source"), "hostname");
}

/// A host that did not name itself still writes the key, with an empty value
/// and the reason, so a reader can tell it from a record written before the
/// field existed.
#[test]
fn a_host_the_platform_could_not_name_still_writes_the_node_key() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);

    // The complete facts with one unknown fact, which is what a real host that
    // hides its name produces, and the facts themselves unknown.
    let records = [
        (
            host_facts_with_hostname(serde_json::json!({"unknown": "cannot read the host name"})),
            "cannot read the host name",
        ),
        (
            HostReport::unknown("a test host knows nothing"),
            "a test host knows nothing",
        ),
    ];
    for (index, (host, reason)) in records.into_iter().enumerate() {
        let record = record_from(
            "m6-service",
            dir.path(),
            &config,
            &trace,
            &["m6-service".to_string()],
            &[],
            &[],
        )
        .host(host);
        let (lines, text) = write(&record);
        assert_eq!(record.node(), &Node::unknown(reason));
        for line in &lines {
            let fields = line["fields"].as_object().expect("a line has fields");
            assert!(
                fields.contains_key("node"),
                "the node key is present even when the host is unnamed: {text}"
            );
            assert_eq!(
                fields["node"], "",
                "an unnamed node is the empty string, not a name: {text}"
            );
            assert_eq!(
                fields["node_unknown"], reason,
                "the line says why the node is unnamed: {text}"
            );
            assert_eq!(
                fields["node_source"], "none",
                "an unnamed node has no source: {text}"
            );
        }
        let path = dir.path().join(format!("shutdown-{index}.jsonl"));
        let mut sink = m6_core::log::Sink::file(&path).unwrap();
        startup::write_shutdown_into(
            &mut sink,
            record.run_id(),
            record.app(),
            &m6_core::build_info::identity().full(),
            record.node(),
            32,
            "signal",
        );
        sink.finish().unwrap();
        let shutdown: Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(field(&shutdown, "node"), "");
        assert_eq!(field(&shutdown, "node_unknown"), reason);
        assert_eq!(field(&shutdown, "node_source"), "none");
    }
}

/// A complete host-facts value whose only interesting fact is `hostname`.
fn host_facts_with_hostname(hostname: Value) -> HostReport {
    HostReport::new(serde_json::json!({
        "hostname": hostname,
        "kernel": {"known": {"release": "6.11.0-1", "version": "#1"}},
    }))
}

/// The application's configured node name is the node every line states, and
/// the hostname is only the fallback, marked as such. Two services on one host
/// that name themselves differently therefore stay two nodes, which is exactly
/// the case a hostname alone merges.
#[test]
fn two_services_on_one_host_report_two_configured_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);
    let mut reported = Vec::new();
    for name in ["edge-a", "edge-b"] {
        let record = record_from(
            "m6-service",
            dir.path(),
            &config,
            &trace,
            &["m6-service".to_string()],
            &[],
            &[],
        )
        .host(host_named("one-host"))
        .node_name(name);
        assert_eq!(record.node(), &Node::named(name));
        assert_eq!(
            record.node().source(),
            NodeSource::Configured,
            "a configured name is marked configured"
        );

        let (lines, text) = write(&record);
        for line in &lines {
            assert_eq!(
                field(line, "node"),
                name,
                "every part states the configured name: {text}"
            );
            assert_eq!(
                field(line, "node_source"),
                "configured",
                "the source says the application named it: {text}"
            );
            assert!(
                line["fields"].get("node_unknown").is_none(),
                "a configured node carries no reason: {text}"
            );
        }

        // The platform line still reports the one machine both services run
        // on. Only the identity differs, which is what keeps them apart.
        let platform = lines.iter().find(|l| part(l) == "platform").unwrap();
        assert_eq!(
            field(platform, "host")["hostname"]["known"],
            "one-host",
            "the host facts are the machine's: {text}"
        );

        let path = dir.path().join(format!("shutdown-{name}.jsonl"));
        let mut sink = m6_core::log::Sink::file(&path).unwrap();
        startup::write_shutdown_into(
            &mut sink,
            record.run_id(),
            record.app(),
            &m6_core::build_info::identity().full(),
            record.node(),
            32,
            "signal",
        );
        sink.finish().unwrap();
        let shutdown: Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(field(&shutdown, "node"), name, "the shutdown line agrees");
        assert_eq!(field(&shutdown, "node_source"), "configured");

        reported.push(field(&lines[0], "node").as_str().unwrap().to_string());
    }
    assert_eq!(
        reported,
        vec!["edge-a".to_string(), "edge-b".to_string()],
        "one host, two nodes"
    );
}

#[test]
fn a_table_of_routes_is_one_line_per_route_not_one_long_line() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);
    let record = record_from(
        "m6-service",
        dir.path(),
        &config,
        &trace,
        &["m6-service".to_string()],
        &[],
        &[],
    );
    let (lines, text) = write(&record);
    let routes: Vec<&Value> = lines
        .iter()
        .filter(|l| part(l) == "config_section")
        .filter(|l| field(l, "section").as_str().unwrap().starts_with("route"))
        .collect();
    assert_eq!(routes.len(), 2, "one section per route");
    assert_eq!(field(routes[0], "section"), "route[0]");
    assert_eq!(field(routes[1], "section"), "route[1]");
    // The longest line is far below the 64 KiB journal line limit. The
    // measurement is recorded in the report and held here as a floor.
    let longest = text.lines().map(str::len).max().unwrap();
    assert!(
        longest < m6_core::limits::JOURNAL_LINE.start,
        "the longest line is {longest} B"
    );
}

#[test]
fn a_setting_that_came_from_the_config_file_says_so_even_when_it_equals_the_default() {
    // size = 512 equals no default here, but the point is that membership in
    // the file decides the label, not equality with a default value.
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);
    let sections = config.startup_sections(dir.path(), &trace);
    let section = sections.iter().find(|s| s.path == "params_cache").unwrap();
    assert_eq!(section.settings["size"].value, 512);
    assert_eq!(section.settings["size"].source, Source::File);
}

#[test]
fn a_secret_file_path_is_a_setting_while_its_contents_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets.toml");
    std::fs::write(&secrets, "[smtp]\npassword = \"never-logged\"\n").unwrap();
    let body = format!(
        "secrets_file = {:?}\n[server]\nsocket_mode = \"0660\"\n",
        secrets.display()
    );
    let (_, config, trace) = fixture(dir.path(), &body);
    let record = record_from(
        "m6-service",
        dir.path(),
        &config,
        &trace,
        &["m6-service".to_string()],
        &[],
        &[],
    );
    let (lines, text) = write(&record);
    let files: Vec<&Value> = lines.iter().filter(|l| part(l) == "config_file").collect();
    assert_eq!(field(files[1], "role").as_str().unwrap(), "secrets");
    assert!(
        text.contains(&secrets.display().to_string()),
        "the secrets file's path is logged"
    );
    assert!(!text.contains("never-logged"));
}

#[test]
fn the_role_words_are_stable() {
    assert_eq!(ConfigRole::Config.text(), "config");
    assert_eq!(ConfigRole::Secrets.text(), "secrets");
    assert_eq!(ConfigRole::Site.text(), "site");
    assert_eq!(Source::File.text(), "file");
    assert_eq!(Source::Default.text(), "default");
    assert_eq!(Source::Env.text(), "env");
    assert_eq!(Source::Flag.text(), "flag");
}

/// The size measurement, over a rendered production or staging tree.
///
/// Ignored by default because it reads a tree outside this repository. Run it
/// with the two paths and it prints the record's longest line, its part, and
/// the line count:
///
/// ```text
/// M6_MEASURE_APP=m6-file \
/// M6_MEASURE_CONF=<tree>/configs/m6-file.conf \
/// M6_MEASURE_SITE_DIR=<tree>/apps/file \
/// M6_MEASURE_SITE_TOML=<tree>/site.toml \
/// cargo test -p m6-core --test startup_record measured_record -- --ignored --nocapture
/// ```
///
/// The assertion is the owner's rule rather than a recorded number: every line
/// stays under `[limits] journal_line`, and the count line says how many there
/// are. The number printed is the evidence.
#[test]
#[ignore = "a measurement over a rendered tree outside this repository"]
fn measured_record_from_a_rendered_tree() {
    let Ok(conf) = std::env::var("M6_MEASURE_CONF") else {
        return;
    };
    let conf = PathBuf::from(conf);
    let site_dir = PathBuf::from(std::env::var("M6_MEASURE_SITE_DIR").expect("site dir"));
    let app = std::env::var("M6_MEASURE_APP").unwrap_or_else(|_| "m6-file".to_string());
    let site_toml =
        PathBuf::from(std::env::var("M6_MEASURE_SITE_TOML").expect("the site.toml path"));

    // A rendered tree can predate a key removal this base refuses. Strip only
    // `[log] format`, the one such key met so far, and say so in the output.
    let scratch = tempfile::tempdir().expect("tempdir for a stripped copy");
    let mut load_path = conf.clone();
    if std::env::var("M6_MEASURE_STRIP_LOG_FORMAT").as_deref() == Ok("1") {
        let text = std::fs::read_to_string(&conf).expect("the config is readable");
        let stripped = strip_log_format(&text);
        if stripped != text {
            let path = scratch.path().join("measure.conf");
            std::fs::write(&path, &stripped).expect("write the stripped config");
            load_path = path;
            println!("stripped [log] format from the rendered config");
        }
    }
    let (config, mut trace) =
        m6_core::config::load_traced(&load_path, &site_dir).expect("the rendered config loads");
    let site_text = std::fs::read_to_string(&site_toml).expect("site.toml is readable");
    let site_value: toml::Value = toml::from_str(&site_text).expect("site.toml parses");
    trace.add_config_file(&site_toml, ConfigRole::Site, &site_text, &site_value);

    let mut sections = config.startup_sections(&site_dir, &trace);
    for section in &mut sections {
        section.file = Some(load_path.display().to_string());
    }
    let mut from_site = startup::sections_from_toml("", &site_value, &trace);
    for section in &mut from_site {
        section.file = Some(site_toml.display().to_string());
    }
    sections.append(&mut from_site);

    let argv = vec![
        app.clone(),
        site_dir.display().to_string(),
        load_path.display().to_string(),
    ];
    let record = Record::new(app, argv.clone())
        .trace(trace.clone())
        .sections(sections)
        .host(HostReport::decided())
        .limits(LimitsReport::read(&config.limits, &trace));
    let (lines, text) = write(&record);

    let mut sizes: Vec<(usize, String, String)> = text
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).unwrap();
            let part = value["fields"]["part"].as_str().unwrap_or("?").to_string();
            let section = value["fields"]["section"]
                .as_str()
                .unwrap_or("")
                .to_string();
            (line.len(), part, section)
        })
        .collect();
    sizes.sort_by_key(|a| std::cmp::Reverse(a.0));
    println!("config: {}", conf.display());
    println!("site.toml: {}", site_toml.display());
    println!("lines: {}", lines.len());
    println!("bytes: {}", text.len());
    println!(
        "longest line: {} B, part={}, section={}",
        sizes[0].0, sizes[0].1, sizes[0].2
    );
    for (bytes, part, section) in sizes.iter().take(10) {
        println!("  {bytes:>6} B  {part} {section}");
    }
    let longest = sizes[0].0;
    assert!(
        longest < m6_core::limits::JOURNAL_LINE.start,
        "every line stays under [limits] journal_line, the longest is {longest} B"
    );
    assert_eq!(
        field(lines.last().unwrap(), "lines").as_u64().unwrap(),
        lines.len() as u64
    );
}

#[test]
fn a_secret_flag_value_never_reaches_a_line() {
    let separate = ["--password", "pw-must-never-be-logged"];
    a_secret_flag_is_redacted(&separate, &["--password", "<redacted>"]);
    let inline = ["--password=pw-must-never-be-logged"];
    a_secret_flag_is_redacted(&inline, &["--password=<redacted>"]);
}

/// Read `args` after the program name and check no line carries the secret,
/// the argv reads `expected` and the flag's value is redacted.
fn a_secret_flag_is_redacted(args: &[&str], expected: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);
    let flags = [FlagDecl {
        name: "--password",
        takes_value: true,
        setting: None,
        secret: true,
    }];
    let argv: Vec<String> = std::iter::once("m6-service")
        .chain(args.iter().copied())
        .map(str::to_string)
        .collect();
    let mut trace = trace;
    let invocation = Invocation::read(&argv, &flags, &[], &mut trace);
    let record = Record::new("m6-service", argv)
        .invocation(invocation, trace.clone())
        .sections(config.startup_sections(dir.path(), &trace))
        .host(HostReport::unknown("none"));
    let (lines, text) = write(&record);
    assert!(
        !text.contains("pw-must-never-be-logged"),
        "a secret flag's value is in no line:\n{text}"
    );
    let invocation = lines.iter().find(|l| part(l) == "invocation").unwrap();
    let argv: Vec<&str> = field(invocation, "argv").as_array().unwrap()[1..]
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(
        argv, expected,
        "the argv entry is redacted, so the reader still sees the flag"
    );
    assert_eq!(
        field(invocation, "flags")[0]["value"].as_str().unwrap(),
        "<redacted>"
    );
}

/// Remove `format = ...` lines from the `[log]` table only.
fn strip_log_format(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_log = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            in_log = trimmed.starts_with("[log]");
        }
        if in_log && trimmed.starts_with("format") && trimmed.contains('=') {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ── Rebuilding a config from a record ───────────────────────────────────────

/// The config a record reaches: its sections as TOML with the effective
/// `[limits]` table added, for the round-trip test.
///
/// A redacted setting is skipped, because its value never entered the record
/// and comes from the secrets file. The derived socket path is skipped,
/// because it is a decided fact and no key. A route's `cache_explicit` is
/// derived, and its `cache` is a default unless the file set it, so both are
/// skipped where the parser recomputes them.
fn rebuild_config(
    sections: &[startup::Section],
    limits: &LimitsReport,
    config: &m6_core::config::RendererConfig,
) -> toml::Table {
    let mut sections = sections.to_vec();
    for section in &mut sections {
        if section.path == "server" {
            section.settings.remove("socket_path");
        }
    }
    for (index, route) in config.routes.iter().enumerate() {
        let path = format!("route[{index}]");
        if let Some(section) = sections.iter_mut().find(|s| s.path == path) {
            section.settings.remove("cache_explicit");
            if !route.cache_explicit {
                section.settings.remove("cache");
            }
        }
    }
    let mut table = sections_to_toml(&sections);
    let mut named = toml::Table::new();
    for (name, limit) in &limits.limits {
        let mut entry = toml::Table::new();
        entry.insert(
            "start".to_string(),
            toml::Value::Integer(limit.start as i64),
        );
        entry.insert("max".to_string(), toml::Value::Integer(limit.max as i64));
        named.insert((*name).to_string(), toml::Value::Table(entry));
    }
    table.insert("limits".to_string(), toml::Value::Table(named));
    table
}

/// Rebuild a TOML table from a record's sections.
///
/// A redacted setting is skipped: its value comes from the secrets file, which
/// the record never contained. Every other setting is written at its recorded
/// path with its recorded value, so loading the result produces the same
/// effective config, defaults and all.
fn sections_to_toml(sections: &[startup::Section]) -> toml::Table {
    let mut root = toml::Table::new();
    for section in sections {
        if section.path == "root" {
            for (key, setting) in &section.settings {
                if !setting.is_redacted() && !setting.value.is_null() {
                    root.insert(key.clone(), json_to_toml(&setting.value));
                }
            }
            continue;
        }
        for (key, setting) in &section.settings {
            if setting.is_redacted() || setting.value.is_null() {
                continue;
            }
            insert_path(&mut root, &section.path, key, json_to_toml(&setting.value));
        }
    }
    root
}

fn insert_path(root: &mut toml::Table, path: &str, key: &str, value: toml::Value) {
    insert_into(root, &split_path(path), key, value);
}

fn insert_into(table: &mut toml::Table, segments: &[Segment], key: &str, value: toml::Value) {
    let Some(first) = segments.first() else {
        table.insert(key.to_string(), value);
        return;
    };
    let Segment::Key(name) = first else {
        // A path never starts with an index: an array section is `name[0]`.
        return;
    };
    if let Some(Segment::Index(index)) = segments.get(1) {
        let array = table
            .entry(name.clone())
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        let array = array
            .as_array_mut()
            .expect("an array section's parent is an array");
        while array.len() <= *index {
            array.push(toml::Value::Table(toml::Table::new()));
        }
        let element = array[*index]
            .as_table_mut()
            .expect("an array section's element is a table");
        insert_into(element, &segments[2..], key, value);
    } else {
        let child = table
            .entry(name.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let child = child
            .as_table_mut()
            .expect("a table section's parent is a table");
        insert_into(child, &segments[1..], key, value);
    }
}

enum Segment {
    Key(String),
    Index(usize),
}

fn split_path(path: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '.' if !quoted => {
                push_segment(&mut out, &mut current);
            }
            '[' if !quoted => {
                push_segment(&mut out, &mut current);
                let mut digits = String::new();
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                    digits.push(c);
                }
                if let Ok(index) = digits.parse() {
                    out.push(Segment::Index(index));
                }
            }
            other => current.push(other),
        }
    }
    push_segment(&mut out, &mut current);
    out
}

fn push_segment(out: &mut Vec<Segment>, current: &mut String) {
    if current.is_empty() {
        return;
    }
    out.push(Segment::Key(std::mem::take(current)));
}

fn json_to_toml(value: &Value) -> toml::Value {
    match value {
        Value::Null => toml::Value::String(String::new()),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                toml::Value::Float(f)
            } else {
                toml::Value::String(n.to_string())
            }
        }
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(items) => toml::Value::Array(items.iter().map(json_to_toml).collect()),
        Value::Object(map) => toml::Value::Table(
            map.iter()
                .map(|(key, child)| (key.clone(), json_to_toml(child)))
                .collect(),
        ),
    }
}
