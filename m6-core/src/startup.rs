//! The start-up and shutdown recovery record.
//!
//! **The record is several `type=startup` lines, not one.** Each line is
//! self-contained, carries the same `run_id`, the node, the application name
//! and its full version, and a `part` that says what it holds. A reader filters
//! `type=startup` (or `journalctl M6_TYPE=startup`) and groups by `run_id`. The
//! last line is `part=complete` and carries `lines`, the number of startup
//! lines in the record including itself, so a reader that counts the lines it
//! received can tell a complete record from a truncated one.
//!
//! The parts, in the order they are written:
//!
//! 1. `identity`: the app, its full SemVer, its tag form, and one line per m6
//!    crate linked into it. The same data as `--version-full`.
//! 2. `invocation`: the command line, the flags that were declared, and every
//!    environment variable the application declares that changes its
//!    behaviour. An unset declared variable is listed with a null value, so a
//!    reader can tell "not set" from "never read".
//! 3. `config_file`: one line per config file as read, with its sha256. A path
//!    to a secret is a setting like any other and is logged. The contents of a
//!    secret are never logged.
//! 4. `config_section`: one line per config table, every key, every default,
//!    and the value's source (`file`, `default`, `env` or `flag`).
//! 5. `platform`: the decided host facts and the effective `[limits]`, each
//!    limit's start, max and source.
//! 6. `complete`: the count.
//!
//! A clean exit writes one `type=shutdown` line with the same `run_id`, the
//! uptime and the reason.
//!
//! # The node
//!
//! Every line carries `node`, the node the run happened on. It is fixed once,
//! when the record is built, and every line of the run writes that one value: a
//! host renamed while the run is in progress cannot make the run's lines
//! disagree. The shutdown line carries the same value.
//!
//! The name is the application's configured node name when it has one, which
//! [`Record::node_name`] states, and the host facts' `hostname` otherwise. Two
//! instances on one host that name themselves differently are two nodes, which
//! is the case a hostname alone merges. [`Node::source`] says `configured` or
//! `hostname`, so a reader can tell a claimed identity from an inferred one.
//!
//! A host that did not name itself either writes `node` as the empty string and
//! the reason in `node_unknown`. Both keys are present, because an empty value
//! cannot be mistaken for a real node and a missing key would look like a
//! record written before this field existed.
//!
//! # Redaction
//!
//! There are three mechanisms:
//!
//! - [`crate::config::Secret`] is a type. A config field declared with it
//!   serialises as `<redacted>` wherever the record would otherwise read it.
//! - A value whose dotted key was supplied by the **secrets file** is redacted,
//!   because the file's role is the type. The value is never copied into a
//!   [`Setting`] at all.
//! - A value with no type, which is a `site.toml` table the process does not
//!   parse or a key a service reads for itself, is redacted when its key names
//!   a credential. [`names_a_secret`] is the rule. Without it a `password` in a
//!   table no m6 struct owns was written in plain text, because nothing typed
//!   it.
//!
//! A path that names a secret file is a plain setting, so `tls_key =
//! "/etc/m6/tls.key"` is logged and the key's bytes never are.
//!
//! # The run id
//!
//! The run id is the process's start time in milliseconds since the Unix epoch,
//! a hyphen, and its pid: `1763452800123-4817`. It groups every line of one run.
//! It is set once, on the first call, and the shutdown line reads the same one.
//! A process start is used rather than a boot time because the boot time is a
//! platform fact W137 supplies and this module must work with a platform layer
//! that knows nothing, while a start time plus a pid is unique per run without
//! a host read: a pid cannot name two processes that started in the same
//! millisecond.
//!
//! # One call
//!
//! A service built on [`crate::site::Site`] gets the record at start and the
//! shutdown line at exit with no call of its own. An application with its own
//! config and loop calls [`Record::emit`] once after logging is up and
//! [`shutdown`] once on a clean exit. See `docs/m6-log.md` for the recovery
//! record and the line shapes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::build_info::BuildIdentity;
use crate::limits::{LimitSpec, Limits, KNOWN};
use crate::log::{Line, Sink, Type};

/// The text a secret's value is replaced with. Never a real value.
pub const REDACTED: &str = "<redacted>";

// ── The application's identity ──────────────────────────────────────────────

/// The identity of the binary that is running, registered by
/// `version_flags!`.
///
/// m6-core cannot know the application's own package name and version, and the
/// record needs it: the first identity line is the binary, then each m6 crate
/// it links. An application that registers none records m6-core's identity,
/// which is never wrong for a tool built only from core and is visibly not the
/// binary for a service.
static APP_IDENTITY: OnceLock<(BuildIdentity, Vec<BuildIdentity>)> = OnceLock::new();

/// Register the running binary's identity and the m6 crates it links.
///
/// Called by `version_flags!`, which expands the binary's own build
/// constants at the call site.
pub fn set_app_identity(own: BuildIdentity, linked: Vec<BuildIdentity>) {
    let _ = APP_IDENTITY.set((own, linked));
}

/// The registered identity, or m6-core's own when none was registered.
pub fn app_identity() -> (BuildIdentity, Vec<BuildIdentity>) {
    APP_IDENTITY
        .get()
        .cloned()
        .unwrap_or_else(|| (crate::build_info::identity(), crate::build_info::linked()))
}

// ── The run id ──────────────────────────────────────────────────────────────

/// One process run: the start time and the pid.
///
/// Displayed as `<milliseconds since the epoch>-<pid>`. See the module doc for
/// why this is the identifier rather than a boot time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId {
    start_ms: u64,
    pid: u32,
}

impl RunId {
    /// The run id for a process that starts now.
    pub fn now() -> RunId {
        RunId {
            start_ms: unix_millis(crate::platform::wall()),
            pid: std::process::id(),
        }
    }

    /// The run id from its two numbers, for a test or a reader.
    pub const fn new(start_ms: u64, pid: u32) -> RunId {
        RunId { start_ms, pid }
    }

    /// Milliseconds since the Unix epoch at which the run started.
    pub const fn start_ms(self) -> u64 {
        self.start_ms
    }

    /// The process id of the run.
    pub const fn pid(self) -> u32 {
        self.pid
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start_ms, self.pid)
    }
}

/// The process's one run id.
static RUN: OnceLock<RunId> = OnceLock::new();

/// What a shutdown line needs that the process may not have kept: the app, its
/// full version, and when the run started.
static IDENTITY: OnceLock<ShutdownIdentity> = OnceLock::new();

/// The record that a shutdown line repeats, set by the first record written.
struct ShutdownIdentity {
    run_id: RunId,
    app: String,
    version: String,
    node: Node,
}

/// The process's run id, set once at start.
///
/// The first call sets it. A second call returns the first, so every line of
/// one process groups under one run id even if two parts of the start ask.
pub fn run_id() -> RunId {
    *RUN.get_or_init(RunId::now)
}

fn unix_millis(now: std::time::SystemTime) -> u64 {
    now.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── A setting and its source ────────────────────────────────────────────────

/// Where an effective setting's value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The config file set it.
    File,
    /// The application filled in a default.
    Default,
    /// An environment variable set it.
    Env,
    /// A command-line flag set it.
    Flag,
}

impl Source {
    /// The word written to the line.
    pub fn text(self) -> &'static str {
        match self {
            Source::File => "file",
            Source::Default => "default",
            Source::Env => "env",
            Source::Flag => "flag",
        }
    }
}

impl Serialize for Source {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.text())
    }
}

/// One effective setting: its JSON value and where it came from.
///
/// A secret's `value` is [`REDACTED`]. The real value is never stored here.
#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    pub value: Value,
    pub source: Source,
}

impl Setting {
    /// A setting with a value and its source.
    pub fn new(value: impl Into<Value>, source: Source) -> Setting {
        Setting {
            value: value.into(),
            source,
        }
    }

    /// A setting whose value must not be written.
    pub fn secret(source: Source) -> Setting {
        Setting {
            value: Value::String(REDACTED.to_string()),
            source,
        }
    }

    /// Whether this setting's value is redacted.
    pub fn is_redacted(&self) -> bool {
        self.value == Value::String(REDACTED.to_string())
    }
}

/// One config table: its dotted path and every key in it.
///
/// A table from `[[route]]` is one section per element, with the path
/// `route[0]`, `route[1]`, and so on, so a large route table is many bounded
/// lines rather than one long one. An array of scalars is a [`Setting`] in its
/// parent section, because it is a value rather than a table.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub path: String,
    pub settings: BTreeMap<String, Setting>,
    /// The config file this table came from, when the record knows it. Two
    /// files can both have a `[server]`, and a reader must be able to tell
    /// which one a line describes.
    pub file: Option<String>,
}

impl Section {
    /// An empty section at `path`.
    pub fn new(path: impl Into<String>) -> Section {
        Section {
            path: path.into(),
            settings: BTreeMap::new(),
            file: None,
        }
    }

    /// Add one key.
    pub fn set(&mut self, key: impl Into<String>, setting: Setting) -> &mut Self {
        self.settings.insert(key.into(), setting);
        self
    }
}

// ── The config files and their source map ───────────────────────────────────

/// What a config file is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigRole {
    /// The process's own config.
    Config,
    /// The file a config names with `secrets_file`.
    Secrets,
    /// `site.toml`, shared by every service of one site.
    Site,
}

impl ConfigRole {
    /// The word written to the line.
    pub fn text(self) -> &'static str {
        match self {
            ConfigRole::Config => "config",
            ConfigRole::Secrets => "secrets",
            ConfigRole::Site => "site",
        }
    }
}

/// One config file as read: its path, what it is for, and its sha256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFile {
    pub path: PathBuf,
    pub role: ConfigRole,
    pub sha256: String,
}

/// Every config file read, and the dotted key each source set.
///
/// A loader adds the files it read, then the flags and the environment it
/// applied. The record asks [`ConfigTrace::source`] per dotted key, so a default
/// is the absence of a source rather than a list of defaults typed a second
/// time.
#[derive(Clone, Debug, Default)]
pub struct ConfigTrace {
    files: Vec<ConfigFile>,
    file_keys: BTreeSet<String>,
    secret_keys: BTreeSet<String>,
    flags: BTreeMap<String, String>,
    env: BTreeMap<String, String>,
}

impl ConfigTrace {
    /// An empty trace.
    pub fn new() -> ConfigTrace {
        ConfigTrace::default()
    }

    /// Add a config file that was read, with its text and parsed value.
    ///
    /// The text is hashed as read, so a reader can match the run to the exact
    /// bytes that were on disk. A file that does not parse still gets a line
    /// and a hash: a load that got this far only calls this with a parse that
    /// succeeded, and a caller that has only the bytes should use
    /// [`ConfigTrace::add_unparsed`].
    pub fn add_config_file(
        &mut self,
        path: &Path,
        role: ConfigRole,
        text: &str,
        value: &toml::Value,
    ) {
        self.files.push(ConfigFile {
            path: path.to_path_buf(),
            role,
            sha256: sha256(text),
        });
        let mut keys = BTreeSet::new();
        flatten_toml(value, "", &mut keys);
        if role == ConfigRole::Secrets {
            self.secret_keys.extend(keys);
        } else {
            self.file_keys.extend(keys);
        }
    }

    /// Add a file that was read but not parsed into a value.
    pub fn add_unparsed(&mut self, path: &Path, role: ConfigRole, text: &str) {
        self.files.push(ConfigFile {
            path: path.to_path_buf(),
            role,
            sha256: sha256(text),
        });
    }

    /// Record that a config file set `path`, for a key whose effective name
    /// differs from the file's own name (a table flattened into one field).
    pub fn mark_file(&mut self, path: &str) {
        self.file_keys.insert(path.to_string());
    }

    /// Record that a flag set `path`.
    pub fn set_flag(&mut self, path: &str, value: &str) {
        self.flags.insert(path.to_string(), value.to_string());
    }

    /// Record that an environment variable set `path`.
    pub fn set_env(&mut self, path: &str, value: &str) {
        self.env.insert(path.to_string(), value.to_string());
    }

    /// The files, in the order they were read.
    pub fn files(&self) -> &[ConfigFile] {
        &self.files
    }

    /// The flags that set a setting.
    pub fn flags(&self) -> &BTreeMap<String, String> {
        &self.flags
    }

    /// The environment variables that set a setting.
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Where `path` came from. A source set by a flag beats one set by the
    /// environment, which beats one set in a file. Anything else is a default.
    pub fn source(&self, path: &str) -> Source {
        let dotted = dotted_path(path);
        if self.flags.contains_key(path) || self.flags.contains_key(&dotted) {
            Source::Flag
        } else if self.env.contains_key(path) || self.env.contains_key(&dotted) {
            Source::Env
        } else if self.file_keys.contains(path)
            || self.file_keys.contains(&dotted)
            || self.secret_keys.contains(path)
            || self.secret_keys.contains(&dotted)
        {
            Source::File
        } else {
            Source::Default
        }
    }

    /// Whether `path`, or a table that contains it, came from the secrets file.
    ///
    /// An ancestor is checked because a whole table supplied by the secrets
    /// file is secret, not only its leaves.
    pub fn is_secret(&self, path: &str) -> bool {
        let dotted = dotted_path(path);
        for candidate in [path, dotted.as_str()] {
            if self.secret_keys.contains(candidate) {
                return true;
            }
            let mut end = candidate.len();
            while let Some(at) = candidate[..end].rfind('.') {
                if self.secret_keys.contains(&candidate[..at]) {
                    return true;
                }
                end = at;
            }
        }
        false
    }

    /// The dotted keys a file supplied.
    pub fn file_keys(&self) -> &BTreeSet<String> {
        &self.file_keys
    }

    /// The dotted keys the secrets file supplied. Never their values.
    pub fn secret_keys(&self) -> &BTreeSet<String> {
        &self.secret_keys
    }
}

/// The path with array brackets written as dots, which is how a TOML tree
/// flattens and how a setting path is written in a flag or environment
/// declaration.
fn dotted_path(path: &str) -> String {
    path.replace('[', ".").replace([']', '"'], "")
}

/// The sha256 of `text`, as lowercase hex.
pub fn sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Flatten a TOML value to the dotted path of every leaf.
///
/// An array of tables uses the element index: `route.0.path`. An array of
/// scalars is one leaf at the array's own path, because it is a value.
pub fn flatten_toml(value: &toml::Value, prefix: &str, out: &mut BTreeSet<String>) {
    match value {
        toml::Value::Table(table) => {
            if table.is_empty() && !prefix.is_empty() {
                out.insert(prefix.to_string());
            }
            for (key, child) in table {
                flatten_toml(child, &join_path(prefix, key), out);
            }
        }
        toml::Value::Array(items) => {
            if !items.is_empty() && items.iter().all(toml::Value::is_table) {
                for (index, child) in items.iter().enumerate() {
                    flatten_toml(child, &format!("{prefix}[{index}]"), out);
                }
            } else {
                out.insert(prefix.to_string());
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.insert(prefix.to_string());
            }
        }
    }
}

// ── The declared invocation ─────────────────────────────────────────────────

/// One command-line flag an application declares.
///
/// A declared flag that was not passed is listed as false in the record, so a
/// reader sees what the application reads rather than a guess from a name.
#[derive(Clone, Copy, Debug)]
pub struct FlagDecl {
    /// The flag as written, for example `--log-level`.
    pub name: &'static str,
    /// Whether it takes the next argument as its value.
    pub takes_value: bool,
    /// The dotted setting it sets, when it sets one.
    pub setting: Option<&'static str>,
    /// Whether the value is a secret. A secret flag's value is `<redacted>`
    /// on the invocation line, and its argv entry is redacted too, so the
    /// command line cannot leak a password.
    pub secret: bool,
}

/// One environment variable an application declares.
///
/// **Declaration, not discovery.** There is no scan for a name prefix: the
/// type that owns the behaviour names the variable, so a new variable is a
/// compile-time declaration beside the code that reads it, and a variable no
/// code reads cannot appear in the record.
#[derive(Clone, Copy, Debug)]
pub struct EnvDecl {
    /// The variable's name, exactly.
    pub name: &'static str,
    /// One short phrase saying what it changes.
    pub why: &'static str,
    /// The dotted setting it sets, when it sets one.
    pub setting: Option<&'static str>,
}

/// One declared environment variable as observed at start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EnvSetting {
    pub name: &'static str,
    pub why: &'static str,
    /// `None` when the variable is not set. Kept rather than omitted.
    pub value: Option<String>,
    /// The dotted config setting it replaced, when it sets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setting: Option<&'static str>,
}

/// One declared flag as observed at start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FlagSetting {
    pub name: &'static str,
    /// `None` when the flag was not passed.
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setting: Option<&'static str>,
}

/// The declared flags and environment variables, as observed on the command
/// line and in the environment.
#[derive(Clone, Debug, Default)]
pub struct Invocation {
    pub flags: Vec<FlagSetting>,
    pub env: Vec<EnvSetting>,
    /// The command line with every declared secret flag's value redacted.
    pub argv: Vec<String>,
}

impl Invocation {
    /// Read the declared flags from `argv` and the declared variables from the
    /// environment, and record every one that sets a setting in `trace`.
    pub fn read(
        argv: &[String],
        flags: &[FlagDecl],
        env: &[EnvDecl],
        trace: &mut ConfigTrace,
    ) -> Invocation {
        let mut read = Invocation {
            argv: argv.to_vec(),
            ..Invocation::default()
        };
        for decl in flags {
            let value = flag_value(argv, decl);
            if decl.secret {
                if decl.takes_value {
                    redact_flag(&mut read.argv, decl.name);
                }
            } else if let (Some(value), Some(setting)) = (value.as_deref(), decl.setting) {
                trace.set_flag(setting, value);
            }
            read.flags.push(FlagSetting {
                name: decl.name,
                value: if decl.secret {
                    value.as_ref().map(|_| REDACTED.to_string())
                } else {
                    value
                },
                setting: decl.setting,
            });
        }
        for decl in env {
            let value = std::env::var(decl.name).ok();
            if let (Some(value), Some(setting)) = (value.as_deref(), decl.setting) {
                trace.set_env(setting, value);
            }
            read.env.push(EnvSetting {
                name: decl.name,
                why: decl.why,
                value,
                setting: decl.setting,
            });
        }
        read
    }

    /// One declared flag's value.
    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|f| f.name == name)
            .and_then(|f| f.value.as_deref())
    }

    /// One declared variable's value.
    pub fn env(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|e| e.name == name)
            .and_then(|e| e.value.as_deref())
    }
}

fn flag_value(argv: &[String], decl: &FlagDecl) -> Option<String> {
    if decl.takes_value {
        if let Some(value) = argv.iter().find_map(|arg| inline_value(arg, decl.name)) {
            return Some(value.to_string());
        }
    }
    let at = argv.iter().position(|arg| arg == decl.name)?;
    if decl.takes_value {
        argv.get(at + 1).cloned()
    } else {
        Some("true".to_string())
    }
}

/// Redact every value `name` is given in `argv`, at the position the value
/// occupied, so a reader still sees that the flag was passed:
/// `--flag <redacted>` or `--flag=<redacted>`.
fn redact_flag(argv: &mut [String], name: &str) {
    for at in 0..argv.len() {
        if argv[at] == name && at + 1 < argv.len() {
            argv[at + 1] = REDACTED.to_string();
        } else if inline_value(&argv[at], name).is_some() {
            argv[at] = format!("{name}={REDACTED}");
        }
    }
}

/// The value of `arg` when it is `name=value`, the one-argument form.
fn inline_value<'a>(arg: &'a str, name: &str) -> Option<&'a str> {
    arg.strip_prefix(name)?.strip_prefix('=')
}

// ── The node ────────────────────────────────────────────────────────────────

/// Where a run's [`Node`] came from.
///
/// The distinction is the point of the field: a configured name is the identity
/// the deployment claimed, and a hostname is one this process read off the
/// machine. Two instances on one host give the same hostname and different
/// configured names, so a reader that sees `hostname` knows the key rests on the
/// machine rather than on a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeSource {
    /// The application's own configuration named the node.
    Configured,
    /// The application's configuration names no node, so the platform facts'
    /// `hostname` is the name.
    Hostname,
    /// No name at all: the host did not name itself either.
    None,
}

impl NodeSource {
    /// The word the record writes as `node_source`.
    pub fn text(self) -> &'static str {
        match self {
            NodeSource::Configured => "configured",
            NodeSource::Hostname => "hostname",
            NodeSource::None => "none",
        }
    }
}

/// The machine a run happened on: the `node` every line of the record states.
///
/// It is read once, when the [`HostReport`] is built, and every line of one run
/// writes the same value. The application's configured node name is used when
/// it has one ([`Record::node_name`]), and the host facts' `hostname` otherwise.
/// A host that did not name itself has an empty [`Node::name`] and a
/// [`Node::reason`]: an empty value cannot be mistaken for a real node, and the
/// key is still present so a reader can tell it from a record written before the
/// field existed. [`Node::source`] says which of the three a reader is holding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    name: String,
    reason: Option<String>,
    source: NodeSource,
}

impl Node {
    /// A node the application's configuration named.
    pub fn named(name: impl Into<String>) -> Node {
        Node {
            name: name.into(),
            reason: None,
            source: NodeSource::Configured,
        }
    }

    /// A node named from the platform facts' hostname.
    pub fn host(name: impl Into<String>) -> Node {
        Node {
            name: name.into(),
            reason: None,
            source: NodeSource::Hostname,
        }
    }

    /// A node the host did not name, and why.
    pub fn unknown(reason: impl Into<String>) -> Node {
        Node {
            name: String::new(),
            reason: Some(reason.into()),
            source: NodeSource::None,
        }
    }

    /// Read the node from the serialised host facts, whose `hostname` is
    /// `{"known": name}` or `{"unknown": reason}`.
    pub fn from_host_facts(facts: &Value) -> Node {
        if let Some(hostname) = facts.get("hostname") {
            if let Some(name) = hostname.get("known").and_then(Value::as_str) {
                return if name.is_empty() {
                    Node::unknown("the host reported an empty hostname")
                } else {
                    Node::host(name)
                };
            }
            if let Some(reason) = hostname.get("unknown").and_then(Value::as_str) {
                return Node::unknown(reason);
            }
            return Node::unknown("the hostname is tagged as neither known nor unknown");
        }
        if let Some(reason) = facts.get("unknown").and_then(Value::as_str) {
            return Node::unknown(reason);
        }
        Node::unknown("the host facts carry no hostname")
    }

    /// The node's name, empty when the host did not name itself.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Why the node is unnamed, or `None` when it is named.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// Where this name came from: the application's config or the hostname.
    pub fn source(&self) -> NodeSource {
        self.source
    }

    /// The word the record writes as `node_source`.
    pub fn source_text(&self) -> &'static str {
        self.source.text()
    }
}

// ── The decided host facts and limits ───────────────────────────────────────

/// The host facts the record reports.
///
/// The platform layer's `host_facts()` is the one reader, and this type is the
/// record's view of it. A fact that could not be read is `{"unknown":
/// "<reason>"}`, never an omitted key and never an error: an omitted fact looks
/// like a fact that is empty, and a reader cannot tell that from one the host
/// does not have.
#[derive(Clone, Debug, PartialEq)]
pub struct HostReport {
    pub facts: Value,
    /// The node these facts name, read once when the report was built.
    node: Node,
}

impl HostReport {
    /// A host report from a value the platform layer produced.
    pub fn new(facts: impl Into<Value>) -> HostReport {
        let facts = facts.into();
        HostReport {
            node: Node::from_host_facts(&facts),
            facts,
        }
    }

    /// A report from a host that knows nothing, with the reason.
    pub fn unknown(reason: &str) -> HostReport {
        HostReport::new(json!({"unknown": reason}))
    }

    /// The node these facts name. It is fixed when the report is built, so
    /// every line of a run states the same value.
    pub fn node(&self) -> &Node {
        &self.node
    }

    /// The report for this host, from the platform layer's one reader.
    ///
    /// `m6_core::platform::host_facts()` reads every fact once, at start, and
    /// never fails. Its serialised form is the tagged shape, so a fact it
    /// could not read arrives as `{"unknown": "<reason>"}` rather than as a
    /// missing key. The value is written under `host` and is never flattened:
    /// a reader must be able to tell "unknown, here is why" from "absent".
    pub fn decided() -> HostReport {
        HostReport::new(
            serde_json::to_value(crate::platform::host_facts())
                .unwrap_or_else(|_| json!({"unknown": "host facts did not serialise"})),
        )
    }

    /// Replace the node the facts would name with the application's own.
    ///
    /// The facts stay exactly as the platform read them, so the platform line
    /// still reports the machine's hostname. Only the identity every line
    /// states changes, and its [`NodeSource`] says the application chose it.
    fn with_node(mut self, node: Node) -> HostReport {
        self.node = node;
        self
    }
}

/// One `[limits]` entry: its start, its max, and where the pair came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LimitSetting {
    pub start: usize,
    pub max: usize,
    pub source: Source,
}

/// Every limit the application decided, keyed by its name.
#[derive(Clone, Debug, Default)]
pub struct LimitsReport {
    pub limits: BTreeMap<&'static str, LimitSetting>,
}

impl LimitsReport {
    /// Read every known limit from the effective config.
    pub fn read(limits: &Limits, trace: &ConfigTrace) -> LimitsReport {
        let mut report = LimitsReport::default();
        for spec in KNOWN {
            let limit = limits.get(spec);
            report.limits.insert(
                spec.name,
                LimitSetting {
                    start: limit.start(),
                    max: limit.max(),
                    source: limit_source(trace, spec),
                },
            );
        }
        report
    }
}

fn limit_source(trace: &ConfigTrace, spec: &LimitSpec) -> Source {
    let start = format!("limits.{}.start", spec.name);
    let max = format!("limits.{}.max", spec.name);
    match trace.source(&start) {
        Source::Default => trace.source(&max),
        set => set,
    }
}

// ── The record ──────────────────────────────────────────────────────────────

/// The start-up record for one run.
///
/// Build it at start, after logging is open, and call [`Record::emit`] once.
/// The record takes ownership of what it writes, so a config value that is not
/// passed to it cannot leak into the log.
#[derive(Clone, Debug)]
pub struct Record {
    run_id: RunId,
    app: String,
    own: BuildIdentity,
    linked: Vec<BuildIdentity>,
    argv: Vec<String>,
    invocation: Invocation,
    trace: ConfigTrace,
    sections: Vec<Section>,
    host: HostReport,
    limits: LimitsReport,
    decisions: Map<String, Value>,
}

impl Record {
    /// A record for `app`: the identity `version_flags!` registered, the host
    /// the platform layer decides, and the default limits. A service with a
    /// `[limits]` table replaces them with [`Record::limits`].
    pub fn new(app: impl Into<String>, argv: Vec<String>) -> Record {
        let (own, linked) = app_identity();
        Record {
            run_id: RunId::now(),
            app: app.into(),
            own,
            linked,
            invocation: Invocation::default(),
            trace: ConfigTrace::new(),
            sections: Vec::new(),
            host: HostReport::decided(),
            limits: LimitsReport::read(&Limits::default(), &ConfigTrace::new()),
            decisions: Map::new(),
            argv,
        }
    }

    /// The record's run id. [`Record::emit`] also makes it the process's run id,
    /// so the shutdown line matches.
    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// The application name written on every line.
    pub fn app(&self) -> &str {
        &self.app
    }

    /// The node every line of this record states, fixed when the record was
    /// built from the host facts, or when [`Record::node_name`] replaced it.
    pub fn node(&self) -> &Node {
        self.host.node()
    }

    /// State the application's configured node name on every line.
    ///
    /// An application whose config names its node passes that name, so two
    /// instances on one host stay two nodes. The host facts still report the
    /// machine's hostname, and the `node_source` key says the name was
    /// configured rather than read from the host. An empty name, or no call at
    /// all, leaves the hostname in place, marked `hostname`.
    pub fn node_name(mut self, name: impl Into<String>) -> Record {
        let name = name.into();
        if !name.is_empty() {
            self.host = self.host.with_node(Node::named(name));
        }
        self
    }

    /// Use an explicit run id, for a reader or a test.
    pub fn with_run_id(mut self, run_id: RunId) -> Record {
        self.run_id = run_id;
        self
    }

    /// The declared invocation, with the trace it updated.
    pub fn invocation(mut self, invocation: Invocation, trace: ConfigTrace) -> Record {
        self.argv = invocation.argv.clone();
        self.invocation = invocation;
        self.trace = trace;
        self
    }

    /// The config trace alone, for a caller that reads the invocation itself.
    pub fn trace(mut self, trace: ConfigTrace) -> Record {
        self.trace = trace;
        self
    }

    /// One config section.
    pub fn section(mut self, section: Section) -> Record {
        self.sections.push(section);
        self
    }

    /// Every config section, in order.
    pub fn sections(mut self, sections: Vec<Section>) -> Record {
        self.sections = sections;
        self
    }

    /// The decided host facts.
    pub fn host(mut self, host: HostReport) -> Record {
        self.host = host;
        self
    }

    /// The effective limits.
    pub fn limits(mut self, limits: LimitsReport) -> Record {
        self.limits = limits;
        self
    }

    /// A fact the start decided: the socket it bound, the address it listens
    /// on, whether memory is locked. Written on the platform line, so the
    /// reader of a completed record sees the decisions beside the host.
    pub fn decision(mut self, key: impl Into<String>, value: impl Into<Value>) -> Record {
        self.decisions.insert(key.into(), value.into());
        self
    }

    /// The sections, for a test that compares two records.
    pub fn recorded_sections(&self) -> &[Section] {
        &self.sections
    }

    /// How many startup lines this record writes, the completeness line
    /// included.
    pub fn line_count(&self) -> u64 {
        // identity, invocation, one per file, one per section, platform, complete.
        (4 + self.trace.files().len() + self.sections.len()) as u64
    }

    /// Write every line into `sink`, and return the run id they share.
    ///
    /// This is the writer a test calls with a file sink. A production process
    /// calls [`Record::emit`], which uses the operational sink.
    pub fn write(&self, sink: &mut Sink) -> RunId {
        let _ = RUN.set(self.run_id);
        let _ = IDENTITY.set(ShutdownIdentity {
            run_id: self.run_id,
            app: self.app.clone(),
            version: self.own.full(),
            node: self.host.node().clone(),
        });
        STARTED.get_or_init(crate::platform::now);

        let total = self.line_count();
        let mut seq = 0u64;
        let mut next = || {
            seq += 1;
            seq
        };

        self.base(sink, next(), "identity")
            .str("tag", &self.own.tag())
            .json("crates", &self.crate_lines())
            .str("message", "startup record: identity")
            .end();

        self.base(sink, next(), "invocation")
            .json("argv", &self.argv)
            .json("flags", &self.invocation.flags)
            .json("environment", &self.invocation.env)
            .str("message", "startup record: invocation")
            .end();

        for file in self.trace.files() {
            self.base(sink, next(), "config_file")
                .str("role", file.role.text())
                .str("path", &file.path.display().to_string())
                .str("sha256", &file.sha256)
                .str("message", "startup record: config file")
                .end();
        }

        for section in &self.sections {
            let settings: Map<String, Value> = section
                .settings
                .iter()
                .map(|(key, setting)| {
                    (
                        key.clone(),
                        json!({"value": setting.value, "source": setting.source.text()}),
                    )
                })
                .collect();
            let line = self
                .base(sink, next(), "config_section")
                .str("section", &section.path);
            let line = match &section.file {
                Some(file) => line.str("file", file),
                None => line,
            };
            line.json("settings", &settings)
                .str("message", "startup record: config section")
                .end();
        }

        let limits = json!(self.limits.limits);
        self.base(sink, next(), "platform")
            .json("host", &self.host.facts)
            .json("limits", &limits)
            .json("decided", &self.decisions)
            .str("message", "startup record: platform")
            .end();

        self.base(sink, next(), "complete")
            .u64("lines", total)
            .str("message", "startup record: complete")
            .end();

        self.run_id
    }

    /// Write the record into the process's operational log, if it is open.
    ///
    /// Returns the run id when the lines were written, and `None` when logging
    /// was never opened: a record is never a reason to crash a start.
    pub fn emit(&self) -> Option<RunId> {
        let mut written = false;
        crate::log::with_operational(|sink| {
            self.write(sink);
            written = true;
        });
        written.then_some(self.run_id)
    }

    fn base<'a>(&self, sink: &'a mut Sink, seq: u64, part: &str) -> Line<'a> {
        let node = self.host.node();
        Line::empty(sink, crate::log::Level::Info)
            .kind(Type::Startup)
            .str("run_id", &self.run_id.to_string())
            .u64("seq", seq)
            .str("part", part)
            .str("app", &self.app)
            .str("version", &self.own.full())
            .str("node", node.name())
            .str("node_source", node.source_text())
            .opt_str("node_unknown", node.reason())
    }

    fn crate_lines(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut lines = Vec::new();
        for id in std::iter::once(&self.own).chain(self.linked.iter()) {
            if seen.insert(id.name) {
                lines.push(id.line());
            }
        }
        lines
    }
}

/// The start of the process, for the shutdown line's uptime.
static STARTED: OnceLock<Instant> = OnceLock::new();

/// Write the one `type=shutdown` line for a clean exit.
///
/// Does nothing when no startup record was written: a `--dump-config` run or a
/// test that never started the log writes no shutdown line, because there is no
/// run id for a reader to group it under. The reason is a short phrase, for
/// example `signal` or `pass complete`.
pub fn shutdown(reason: &str) {
    let Some(identity) = IDENTITY.get() else {
        return;
    };
    let uptime_ms = STARTED
        .get()
        .map_or(0, |started| started.elapsed().as_millis() as u64);
    crate::log::with_operational(|sink| {
        write_shutdown_into(
            sink,
            identity.run_id,
            &identity.app,
            &identity.version,
            &identity.node,
            uptime_ms,
            reason,
        );
    });
}

/// Write one shutdown line into `sink`.
///
/// The process's own [`shutdown`] calls this into the operational sink with the
/// node the start-up record fixed at start, so the two agree. A test calls it
/// into a file sink, so the line's shape is proven on the bytes an operator
/// reads rather than on a private builder.
pub fn write_shutdown_into(
    sink: &mut Sink,
    run_id: RunId,
    app: &str,
    version: &str,
    node: &Node,
    uptime_ms: u64,
    reason: &str,
) {
    Line::empty(sink, crate::log::Level::Info)
        .kind(Type::Shutdown)
        .str("run_id", &run_id.to_string())
        .str("app", app)
        .str("version", version)
        .str("node", node.name())
        .str("node_source", node.source_text())
        .opt_str("node_unknown", node.reason())
        .u64("uptime_ms", uptime_ms)
        .str("reason", reason)
        .str("message", "shutdown")
        .end();
}

/// Record the log level the process runs at in the `log` section, sourced
/// from `trace`. The config file's `[log]` table is not the last word:
/// `site.toml` and `--log-level` come later in the order.
pub fn record_log_level(sections: &mut [Section], level: crate::log::Level, trace: &ConfigTrace) {
    if let Some(log_section) = sections.iter_mut().find(|s| s.path == "log") {
        log_section.set(
            "level",
            Setting::new(level.text().to_ascii_lowercase(), trace.source("log.level")),
        );
    }
}

// ── Turning an effective config into sections ───────────────────────────────

/// Split an effective config, as JSON, into one [`Section`] per table, with
/// every key and its source.
///
/// This is the generic half of the record: an application's config type
/// serialises its effective state (defaults included), and this turns it into
/// bounded lines. A table becomes its own section, an array of tables becomes
/// one section per element, and an array of scalars stays a value in its parent.
/// A scalar's source comes from `trace`, which records what each file, flag and
/// environment variable set. A value the secrets file supplied is replaced with
/// [`REDACTED`] before it can reach a line.
pub fn sections_from_value(prefix: &str, value: &Value, trace: &ConfigTrace) -> Vec<Section> {
    let mut out = Vec::new();
    match value {
        Value::Object(map) => decompose_object(prefix, map, trace, &mut out),
        _ => {
            let mut section = Section::new(if prefix.is_empty() { "root" } else { prefix });
            section.set("value", setting_for(prefix, value, trace));
            out.push(section);
        }
    }
    out
}

/// Split a TOML value into sections.
pub fn sections_from_toml(prefix: &str, value: &toml::Value, trace: &ConfigTrace) -> Vec<Section> {
    let as_json = crate::config::toml_to_json(value);
    sections_from_value(prefix, &as_json, trace)
}

fn decompose_object(
    prefix: &str,
    map: &Map<String, Value>,
    trace: &ConfigTrace,
    out: &mut Vec<Section>,
) {
    let name = if prefix.is_empty() { "root" } else { prefix };
    let mut scalars = BTreeMap::new();
    for (key, child) in map {
        let path = join_path(prefix, key);
        match child {
            Value::Object(inner) => decompose_object(&path, inner, trace, out),
            Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
                for (index, item) in items.iter().enumerate() {
                    if let Value::Object(inner) = item {
                        decompose_object(&format!("{path}[{index}]"), inner, trace, out);
                    }
                }
            }
            other => {
                scalars.insert(key.clone(), setting_for(&path, other, trace));
            }
        }
    }
    if !scalars.is_empty() || out.is_empty() {
        let mut section = Section::new(name);
        section.settings = scalars;
        out.push(section);
    }
}

fn setting_for(path: &str, value: &Value, trace: &ConfigTrace) -> Setting {
    if trace.is_secret(path) || names_a_secret(last_key(path)) {
        Setting::secret(trace.source(path))
    } else {
        Setting::new(value.clone(), trace.source(path))
    }
}

/// The last key of a dotted section path: `smtp.password` gives `password`
/// and `route[2]` gives `route`.
fn last_key(path: &str) -> &str {
    let key = path.rsplit('.').next().unwrap_or(path);
    key.split('[').next().unwrap_or(key)
}

/// The words a key holds when its value is a credential.
const SECRET_WORDS: [&str; 5] = ["password", "secret", "token", "credential", "private_key"];

/// The endings of a key whose value is where a secret is, not the secret.
const PATH_ENDINGS: [&str; 5] = ["_path", "_paths", "_file", "_files", "_dir"];

/// Whether a config key names a credential.
///
/// This is the rule for a value that no type describes: a table in `site.toml`
/// that the process does not parse, or a key a service reads for itself. Such a
/// value reaches the record as plain TOML, so [`crate::config::Secret`] cannot
/// mark it, and the key's name is the only thing left to judge it by. A key
/// holding `password`, `secret`, `token`, `credential` or `private_key` is
/// redacted. A key that names where a secret is kept, ending in `_path`,
/// `_file` or `_dir`, is a setting and is logged, as is any `public_key`.
///
/// The test `m6-core/tests/secret_fields.rs` holds every typed config field to
/// the same rule, so a typed field this rule would name is declared
/// [`crate::config::Secret`] and the two cannot disagree.
pub fn names_a_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    if key.contains("public_key") || PATH_ENDINGS.iter().any(|end| key.ends_with(end)) {
        return false;
    }
    SECRET_WORDS.iter().any(|word| key.contains(word)) || key.contains("privatekey")
}

// ── Paths ───────────────────────────────────────────────────────────────────

/// Join a dotted path and a key, quoting the key when TOML would need it.
///
/// `path` is empty at the root, so the result has no leading dot.
pub fn join_path(prefix: &str, key: &str) -> String {
    let key = quote_key(key);
    if prefix.is_empty() {
        key
    } else {
        format!("{prefix}.{key}")
    }
}

fn quote_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        key.to_string()
    } else {
        format!("\"{}\"", key.replace('\\', "\\\\").replace('"', "\\\""))
    }
}
