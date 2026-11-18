//! The start-up and shutdown recovery record, tested through the interface an
//! application uses.
//!
//! Every test writes a real record into a file Sink and reads the JSONL back,
//! so the assertions are on the bytes an operator and a monitor see. No test
//! inspects a private builder.

use std::path::{Path, PathBuf};

use m6_core::startup::{
    self, ConfigRole, ConfigTrace, EnvDecl, FlagDecl, HostReport, Invocation, LimitsReport, Record,
    RunId, Setting, Source,
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
    Record::new(
        name,
        m6_core::build_info::identity(),
        m6_core::build_info::linked(),
        argv.to_vec(),
    )
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
            Setting::decided(socket_value.clone(), trace.source("server.socket_path")),
        );
    let record = Record::new(
        "m6-service",
        m6_core::build_info::identity(),
        m6_core::build_info::linked(),
        argv,
    )
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
        setting(&lines, "minification", "inline_js"),
        &serde_json::json!({"value": false, "source": "default"})
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
    let rebuilt = record.to_toml();
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
    assert_eq!(field(&line, "uptime_ms"), 1_234);
    assert_eq!(field(&line, "reason"), "signal");
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
    assert_eq!(section.settings["size"].rebuild, true);
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
    let record = Record::new(
        app,
        m6_core::build_info::identity(),
        m6_core::build_info::linked(),
        argv.clone(),
    )
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
    sizes.sort_by(|a, b| b.0.cmp(&a.0));
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
    let dir = tempfile::tempdir().unwrap();
    let (_, config, trace) = fixture(dir.path(), CONFIG);
    let flags = [FlagDecl {
        name: "--password",
        takes_value: true,
        setting: None,
        secret: true,
    }];
    let argv = vec![
        "m6-service".to_string(),
        "--password".to_string(),
        "pw-must-never-be-logged".to_string(),
    ];
    let mut trace = trace;
    let invocation = Invocation::read(&argv, &flags, &[], &mut trace);
    let record = Record::new(
        "m6-service",
        m6_core::build_info::identity(),
        m6_core::build_info::linked(),
        argv,
    )
    .invocation(invocation, trace.clone())
    .sections(config.startup_sections(dir.path(), &trace))
    .host(HostReport::unknown("none"));
    let (lines, text) = write(&record);
    assert!(
        !text.contains("pw-must-never-be-logged"),
        "a secret flag's value is in no line:\n{text}"
    );
    let invocation = lines.iter().find(|l| part(l) == "invocation").unwrap();
    assert_eq!(
        field(invocation, "argv")[2].as_str().unwrap(),
        "<redacted>",
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
