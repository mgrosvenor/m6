//! No credential reaches the start-up record, from any file a service reads.
//!
//! Every field of a service's config, its secrets file and its `site.toml` is
//! filled with a distinct sentinel. A credential's sentinel starts `SECRET-`,
//! and every other one starts `plain-`. The record is built through
//! [`m6_core::site::record_sections`], the function a running `Site` service
//! calls, so m6-file, m6-html and m6-auth-server are all covered: they differ
//! only in the keys of their own config, and those keys are here.
//!
//! The test asserts both halves. No `SECRET-` sentinel appears in any line, and
//! every `plain-` sentinel does, so a record that wrote nothing could not pass.

use m6_core::log::Level;
use m6_core::site::{record_sections, RecordFiles};
use m6_core::startup::{ConfigRole, HostReport, LimitsReport, Record};

/// A service config with every renderer key and the own keys of m6-file,
/// m6-html and m6-auth-server, each value a sentinel.
const SERVICE: &str = r#"
secrets_file = "{SECRETS}"
global_params = ["plain-global-params.json"]
site_size_budget_mb = 4242
flash_secret = "SECRET-flash-secret"
api_token = "SECRET-service-api-token"
plain_service_key = "plain-service-key"

[log]
level = "debug"

[params_cache]
size = 4343

[server]
read_timeout_s = 44
socket_mode = "0640"

[minification]
inline_js = true
"text/plain" = true

[compression."text/plain"]
brotli = 5
gzip = 6

[[route]]
path = "/plain-route-path"
template = "plain-route-template.html"
params = ["plain-route-param.json"]
status = 203
cache = "no-store"
methods = ["GET", "POST"]
headers = [["X-Plain", "plain-route-header"]]
plain_route_setting = "plain-route-setting"
route_token = "SECRET-route-token"

[storage]
path = "/plain-storage/auth.db"

[tokens]
issuer = "plain-token-issuer"
audience = "plain-token-audience"
access_ttl = 4545
refresh_ttl = 4646

[keys]
public_key = "/plain-keys/public.pem"

[rate_limit]
max_attempts = 47
window_secs = 48

[smtp]
host = "plain-smtp-host"
port = 4949
from = "plain-smtp-from"
password = "SECRET-smtp-password-inline"
"#;

/// The secrets file. Every value in it is redacted by the file's role, so its
/// plain-looking values are credentials too.
const SECRETS: &str = r#"
relay_user = "SECRET-relay-user-from-secrets-file"

[mail]
username = "SECRET-mail-username-from-secrets-file"
password = "SECRET-mail-password-from-secrets-file"
"#;

/// `site.toml`, with the tables m6-http reads and the tables a site's own
/// application reads, which no m6 struct parses.
const SITE: &str = r#"
[site]
name = "plain-site-name"
domain = "plain-site.example.com"
describedby = "plain-site-describedby"

[server]
backend_timeout_secs = 51
allowed_methods = ["GET", "HEAD"]

[analytics]
enabled = true

[errors]
mode = "internal"
path = "plain-errors-path"

[[backend]]
name = "plain-backend-name"
sockets = "/plain-backend/*.sock"

[[route]]
path = "/plain-site-route"
backend = "plain-backend-name"
require = "role:plain-role"

[email]
host = "plain-email-host"
username = "plain-email-username"
password = "SECRET-email-password"
api_key_token = "SECRET-email-api-token"
client_secret = "SECRET-email-client-secret"

[oauth]
credentials = "SECRET-oauth-credentials"
private_key = "SECRET-oauth-private-key"
private_key_path = "/plain-oauth/private.pem"
"#;

fn sentinels(text: &str, prefix: &str) -> Vec<String> {
    text.split('"')
        .filter(|part| part.starts_with(prefix) || part.starts_with(&format!("/{prefix}")))
        .map(str::to_string)
        .collect()
}

#[test]
fn no_credential_sentinel_reaches_any_record_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let site_dir = dir.path();
    let secrets = site_dir.join("secrets.toml");
    std::fs::write(&secrets, SECRETS).expect("write the secrets file");
    let config_path = site_dir.join("m6-service.conf");
    let service = SERVICE.replace("{SECRETS}", &secrets.display().to_string());
    std::fs::write(&config_path, &service).expect("write the service config");
    let site_toml = site_dir.join("site.toml");
    std::fs::write(&site_toml, SITE).expect("write site.toml");

    let (config, mut trace) =
        m6_core::config::load_traced(&config_path, site_dir).expect("the service config loads");
    let site_value: toml::Value = toml::from_str(SITE).expect("site.toml parses");
    trace.add_config_file(&site_toml, ConfigRole::Site, SITE, &site_value);

    let sections = record_sections(
        &config,
        RecordFiles {
            site_dir,
            config_path: &config_path,
            site_toml: &site_toml,
            site_value: Some(&site_value),
        },
        &trace,
        Level::Info,
        &site_dir.join("m6-service.sock"),
    );
    let record = Record::new(
        "m6-service",
        m6_core::build_info::identity(),
        m6_core::build_info::linked(),
        vec!["m6-service".to_string()],
    )
    .trace(trace.clone())
    .sections(sections)
    .host(HostReport::unknown("a test host knows nothing"))
    .limits(LimitsReport::read(&config.limits, &trace));

    let path = dir.path().join("record.jsonl");
    let mut sink = m6_core::log::Sink::file(&path).expect("a file sink");
    record.write(&mut sink);
    sink.finish().expect("the sink flushes");
    let text = std::fs::read_to_string(&path).expect("the record is text");
    assert!(text.lines().count() > 10, "the record is written:\n{text}");

    let all = format!("{service}\n{SECRETS}\n{SITE}");
    let secret = sentinels(&all, "SECRET-");
    let plain = sentinels(&all, "plain-");
    assert!(
        secret.len() >= 12,
        "the fixture holds the credentials: {secret:?}"
    );
    assert!(
        plain.len() >= 20,
        "the fixture holds the settings: {plain:?}"
    );

    let leaked: Vec<&String> = secret
        .iter()
        .filter(|s| text.contains(s.as_str()))
        .collect();
    assert!(
        leaked.is_empty(),
        "credentials reached the start-up record: {leaked:?}\n{text}"
    );
    let missing: Vec<&String> = plain
        .iter()
        .filter(|s| !text.contains(s.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "settings are missing from the record, so the check above proves less than it says: {missing:?}\n{text}"
    );
}

/// Each credential is still named in the record, with `<redacted>` for its
/// value, so a reader sees that it was set.
#[test]
fn a_redacted_credential_keeps_its_key_and_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let site_toml = dir.path().join("site.toml");
    std::fs::write(&site_toml, SITE).expect("write site.toml");
    let site_value: toml::Value = toml::from_str(SITE).expect("site.toml parses");
    let mut trace = m6_core::startup::ConfigTrace::new();
    trace.add_config_file(&site_toml, ConfigRole::Site, SITE, &site_value);
    let sections = m6_core::startup::sections_from_toml("", &site_value, &trace);
    let email = sections
        .iter()
        .find(|s| s.path == "email")
        .expect("an email section");
    let password = &email.settings["password"];
    assert!(password.is_redacted(), "{password:?}");
    assert_eq!(password.source.text(), "file");
    assert_eq!(email.settings["username"].value, "plain-email-username");
    let oauth = sections
        .iter()
        .find(|s| s.path == "oauth")
        .expect("an oauth section");
    assert_eq!(
        oauth.settings["private_key_path"].value, "/plain-oauth/private.pem",
        "a path to a key is a setting and is logged"
    );
}
