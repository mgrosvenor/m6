//! Named limits: one rule for every bounded buffer and queue.
//!
//! A limit has a name, a `start` and a `max`. Both come from one config table,
//! with a built-in default per name:
//!
//! ```toml
//! [limits.render_page]
//! start = "512KiB"
//! max   = "4MiB"
//! ```
//!
//! The rule a limit carries:
//!
//! 1. The area is made at `start` when the service starts.
//! 2. It grows only when a write does not fit, by doubling, clamped at `max`.
//! 3. It keeps the grown size and never shrinks.
//! 4. Every growth logs one line naming the limit and the new size.
//! 5. A write that needs more than `max` is refused, with an error that names
//!    the limit, the size needed against the maximum, what was refused, and the
//!    config key and file to raise.
//!
//! Sizes are whole multiples of the OS page, `page * 2^k`, so doubling from a
//! start reaches `max` exactly. The page size comes from the platform layer.
//! A configured value of another size is rounded up with one warning, and a
//! value that cannot work refuses to start.
//!
//! This is a type, not an allocator: a [`BoundedBuf`] owns one `Vec<u8>`.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::bail;

use crate::log::info;

/// What a limit counts, which decides how its values are rounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Bytes in a large area: a whole power-of-two multiple of the OS page.
    PageBytes,
    /// Bytes in a small area: a power of two.
    Bytes,
    /// Entries in a queue or table, or items held. Used as written.
    Count,
}

/// A limit's name and its built-in defaults.
///
/// A limit that holds no memory of its own, such as a cap on a log field,
/// has a `start` equal to its `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitSpec {
    /// The key under `[limits]`.
    pub name: &'static str,
    /// The size made at start, in the limit's unit.
    pub start: usize,
    /// The size growth stops at, in the limit's unit.
    pub max: usize,
    pub unit: Unit,
}

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const GIB: usize = 1024 * MIB;

/// The working area one rendered page is written into.
pub const RENDER_PAGE: LimitSpec = LimitSpec {
    name: "render_page",
    start: 512 * KIB,
    max: 4 * MIB,
    unit: Unit::PageBytes,
};

const fn bytes(name: &'static str, start: usize, max: usize) -> LimitSpec {
    LimitSpec {
        name,
        start,
        max,
        unit: Unit::Bytes,
    }
}

const fn count(name: &'static str, start: usize, max: usize) -> LimitSpec {
    LimitSpec {
        name,
        start,
        max,
        unit: Unit::Count,
    }
}

/// A service's read buffer for one HTTP/1.1 request on its socket, and so the
/// largest request body it accepts. The head's own 32 KiB is a protocol limit
/// and comes on top.
pub const REQUEST_BODY: LimitSpec = bytes("request_body", 16 * KIB, 16 * MIB);
/// A service's write buffer for one response held in memory.
pub const RESPONSE_BUFFER: LimitSpec = bytes("response_buffer", 64 * KIB, 64 * MIB);
/// The buffer an upstream client reads one backend response into.
pub const UPSTREAM_READ: LimitSpec = bytes("upstream_read", 64 * KIB, 128 * MIB);
/// The buffer an upstream client builds one request in.
pub const UPSTREAM_REQUEST: LimitSpec = bytes("upstream_request", 16 * KIB, 32 * MIB);
/// The connection table of a service: slots made at start, and the most it
/// grows to. The open-file limit can lower the maximum.
pub const CONNECTIONS: LimitSpec = count("connections", 1024, 1_048_576);
/// The compressed form of one response.
pub const COMPRESS_OUTPUT: LimitSpec = bytes("compress_output", 64 * KIB, 128 * MIB);
/// The minified form of one response.
pub const MINIFY_OUTPUT: LimitSpec = bytes("minify_output", 64 * KIB, 128 * MIB);
/// The reply of a mail relay to one command.
pub const SMTP_REPLY: LimitSpec = bytes("smtp_reply", 512, 128 * KIB);
/// A page of an ndjson stream: `start` is the default page size and `max` the
/// largest page a client can ask for. A row larger than `max` is refused.
pub const NDJSON_PAGE: LimitSpec = bytes("ndjson_page", 4 * MIB, 64 * MIB);
/// The most a tail request reads from a file in one response. A longer tail
/// is returned in pages, each ending at the offset the next one starts from.
pub const TAIL_READ: LimitSpec = bytes("tail_read", 64 * KIB, 512 * KIB);
/// One operational log destination's buffer.
pub const LOG_BUFFER: LimitSpec = bytes("log_buffer", 16 * KIB, MIB);
/// The longest string field of one log line. A longer value is cut and ends in
/// `...`.
pub const LOG_FIELD: LimitSpec = bytes("log_field", 8 * KIB, 8 * KIB);
/// The edge's read buffer for one HTTP/1.1 connection, and so the largest
/// request it accepts.
pub const EDGE_REQUEST: LimitSpec = bytes("edge_request", 4 * KIB, 32 * MIB);
/// The edge's write buffer for one HTTP/1.1 connection, and so the largest
/// response it holds in memory.
pub const EDGE_RESPONSE: LimitSpec = bytes("edge_response", 512, 128 * MIB);
/// The edge's read buffer for one HTTP/2 connection.
pub const H2_RECV: LimitSpec = bytes("h2_recv", 16 * KIB, 4 * MIB);
/// One HTTP/2 header block, before it is decoded.
pub const H2_HEADER_BLOCK: LimitSpec = bytes("h2_header_block", 16 * KIB, 256 * KIB);
/// The body of one HTTP/2 request stream.
pub const H2_STREAM_BODY: LimitSpec = bytes("h2_stream_body", 4 * KIB, 32 * MIB);
/// Stale cache entries waiting to be fetched again.
pub const REFRESH_QUEUE: LimitSpec = count("refresh_queue", 16, 4096);
/// Client addresses the per-address request counter tracks.
pub const RATE_LIMIT_ADDRESSES: LimitSpec = count("rate_limit_addresses", 1024, 50_000);
/// The response cache's memory. A cache holds what fits and evicts the least
/// recently read entry for the next, so `start` equals `max`.
pub const CACHE_BYTES: LimitSpec = bytes("cache_bytes", 128 * MIB, 128 * MIB);
/// Polls the monitor holds in memory while its history file catches up.
pub const HISTORY_QUEUE: LimitSpec = count("history_queue", 8, 1024);

/// Every limit m6 knows. A name outside this list under `[limits]` is refused.
pub const KNOWN: &[LimitSpec] = &[
    RENDER_PAGE,
    REQUEST_BODY,
    RESPONSE_BUFFER,
    UPSTREAM_READ,
    UPSTREAM_REQUEST,
    CONNECTIONS,
    COMPRESS_OUTPUT,
    MINIFY_OUTPUT,
    SMTP_REPLY,
    NDJSON_PAGE,
    TAIL_READ,
    LOG_BUFFER,
    LOG_FIELD,
    EDGE_REQUEST,
    EDGE_RESPONSE,
    H2_RECV,
    H2_HEADER_BLOCK,
    H2_STREAM_BODY,
    REFRESH_QUEUE,
    RATE_LIMIT_ADDRESSES,
    CACHE_BYTES,
    HISTORY_QUEUE,
];

/// One limit, with its values rounded and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limit {
    name: &'static str,
    start: usize,
    max: usize,
    unit: Unit,
    file: std::sync::Arc<Path>,
}

impl Limit {
    /// A limit built in code, with no config file behind it: a test's, or a
    /// service that has none. The over-max error then names no file.
    pub fn custom(spec: &LimitSpec, start: usize, max: usize) -> Self {
        assert!(start <= max, "a limit's max is not below its start");
        Self {
            name: spec.name,
            start,
            max,
            unit: spec.unit,
            file: Path::new("").into(),
        }
    }

    /// The limit with its built-in defaults and no config file.
    pub fn builtin(spec: &LimitSpec) -> Self {
        Self::custom(spec, spec.start, spec.max)
    }

    /// The size an area of `current` bytes grows to so it holds `needed`:
    /// doubling, the last step clamped to `max`, and `None` when `needed` is
    /// past it. An area that already holds `needed` stays as it is.
    pub fn grown(&self, current: usize, needed: usize) -> Option<usize> {
        self.grown_to(current, needed, self.max)
    }

    /// As [`Limit::grown`] with the ceiling `ceiling` in place of `max`, for an
    /// area that holds a protocol's fixed head beside a body of up to `max`.
    pub fn grown_to(&self, current: usize, needed: usize, ceiling: usize) -> Option<usize> {
        if needed <= current {
            return Some(current);
        }
        if needed > ceiling {
            return None;
        }
        let mut size = current.max(self.start).max(1);
        while size < needed {
            size = size.saturating_mul(2);
        }
        Some(size.min(ceiling))
    }

    /// Log the one line a growth to `size` owes.
    pub fn log_growth(&self, size: usize) {
        info!(
            limit = self.name,
            "limit {} grew to {}",
            self.name,
            self.text(size)
        );
    }

    /// A size in this limit's unit, for a message.
    pub fn text(&self, n: usize) -> String {
        match self.unit {
            Unit::Count => n.to_string(),
            _ => size_text(n),
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn start(&self) -> usize {
        self.start
    }

    pub fn max(&self) -> usize {
        self.max
    }

    /// The refusal for `what` needing `needed` bytes, over this limit's `max`.
    pub fn exceeded(&self, needed: usize, what: &str) -> LimitExceeded {
        LimitExceeded {
            what: what.to_string(),
            needed: self.text(needed),
            name: self.name,
            max: self.text(self.max),
            file: self.file.clone(),
        }
    }
}

/// A write needed more than a limit's `max`.
#[derive(Debug)]
pub struct LimitExceeded {
    what: String,
    needed: String,
    name: &'static str,
    max: String,
    file: std::sync::Arc<Path>,
}

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let file = if self.file.as_os_str().is_empty() {
            "the config file".to_string()
        } else {
            self.file.display().to_string()
        };
        write!(
            f,
            "{} needs at least {}, over [limits.{}] max = {} in {file}; raise max to fix this",
            self.what, self.needed, self.name, self.max,
        )
    }
}

impl std::error::Error for LimitExceeded {}

/// The `[limits]` table of a config file, after rounding and checks.
#[derive(Debug, Clone)]
pub struct Limits {
    file: PathBuf,
    limits: Vec<Limit>,
    warnings: Vec<String>,
}

impl Default for Limits {
    /// Every limit at its built-in default, for a config with no `[limits]`.
    fn default() -> Self {
        Self::from_toml(None, Path::new(""), crate::platform::process::page_size())
            .expect("the built-in defaults are valid")
    }
}

impl Limits {
    /// Parse the `[limits]` table of `file`, for a platform whose page is `page`
    /// bytes.
    ///
    /// An absent table is every default. Refused: a `[limits]` that is not a
    /// table, a name not in [`KNOWN`], a key other than `start` and `max`, a
    /// value that is not a size, a zero, and a `max` below its `start`. A value
    /// that is not `page * 2^k` is rounded up and recorded in
    /// [`Limits::warnings`].
    pub fn from_toml(
        value: Option<&toml::Value>,
        file: &Path,
        page: usize,
    ) -> anyhow::Result<Self> {
        let table = match value {
            None => None,
            Some(v) => Some(
                v.as_table()
                    .ok_or_else(|| anyhow::anyhow!("[limits] must be a table"))?,
            ),
        };
        if let Some(table) = table {
            if let Some(name) = table.keys().find(|k| !KNOWN.iter().any(|s| s.name == **k)) {
                bail!(
                    "unknown limit [limits.{name}] in {}: known limits are {}",
                    file.display(),
                    KNOWN.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
                );
            }
        }
        let mut warnings = Vec::new();
        let mut limits = Vec::new();
        for spec in KNOWN {
            let set = table.and_then(|t| t.get(spec.name));
            let body = match set {
                None => None,
                Some(v) => Some(v.as_table().ok_or_else(|| {
                    anyhow::anyhow!(
                        "[limits.{}] must be a table in {}",
                        spec.name,
                        file.display()
                    )
                })?),
            };
            if let Some(key) = body.and_then(|b| b.keys().find(|k| *k != "start" && *k != "max")) {
                bail!(
                    "unknown key [limits.{}] {key} in {}: known keys are start, max",
                    spec.name,
                    file.display()
                );
            }
            let mut sized = |key: &str, default: usize| -> anyhow::Result<usize> {
                let Some(v) = body.and_then(|b| b.get(key)) else {
                    return Ok(round_unit(spec.unit, default, page));
                };
                let (n, text) = parse_value(spec.unit, v).map_err(|e| {
                    anyhow::anyhow!("[limits.{}] {key} in {}: {e}", spec.name, file.display())
                })?;
                let used = round_unit(spec.unit, n, page);
                if used != n {
                    let rule = match spec.unit {
                        Unit::PageBytes => {
                            format!("a power-of-two multiple of the {} page", size_text(page))
                        }
                        _ => "a power of two".to_string(),
                    };
                    warnings.push(format!(
                        "[limits.{}] {key} = {text} in {} is not {rule}; using {}. Set it to {} to silence this.",
                        spec.name,
                        file.display(),
                        size_text(used),
                        size_config(used),
                    ));
                }
                Ok(used)
            };
            let start = sized("start", spec.start)?;
            let max = sized("max", spec.max)?;
            if max < start {
                bail!(
                    "[limits.{}] max = {} is below start = {} in {}: raise max or lower start",
                    spec.name,
                    shown(spec.unit, max),
                    shown(spec.unit, start),
                    file.display()
                );
            }
            limits.push(Limit {
                name: spec.name,
                start,
                max,
                unit: spec.unit,
                file: file.into(),
            });
        }
        Ok(Self {
            file: file.to_path_buf(),
            limits,
            warnings,
        })
    }

    /// The limit for `spec`.
    pub fn get(&self, spec: &LimitSpec) -> &Limit {
        self.limits
            .iter()
            .find(|l| l.name == spec.name)
            .expect("every known limit is built")
    }

    /// The config file these limits came from.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// One line for each value rounded up. A service prints each to stderr at
    /// start, after its own name.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// The value a limit of `unit` takes for `n`: `page * 2^k` for a large area, a
/// power of two for a small one, and `n` for a count.
fn round_unit(unit: Unit, n: usize, page: usize) -> usize {
    match unit {
        Unit::PageBytes => page * n.div_ceil(page).next_power_of_two(),
        Unit::Bytes => n.next_power_of_two(),
        Unit::Count => n,
    }
}

fn shown(unit: Unit, n: usize) -> String {
    match unit {
        Unit::Count => n.to_string(),
        _ => size_text(n),
    }
}

/// One config value of a limit: a size for bytes, a whole number for a count.
/// Returns the number and the text as written.
fn parse_value(unit: Unit, v: &toml::Value) -> anyhow::Result<(usize, String)> {
    if unit != Unit::Count {
        return parse_size(v);
    }
    match v {
        toml::Value::Integer(n) => match usize::try_from(*n) {
            Ok(0) => bail!("0 must be above zero"),
            Ok(n) => Ok((n, n.to_string())),
            Err(_) => bail!("{n} is not a count"),
        },
        other => bail!("must be a whole number of entries, got {other}"),
    }
}

/// A size as the config states it: a whole number of bytes, or a number with
/// `B`, `KiB`, `MiB` or `GiB`. Returns the bytes and the text as written.
fn parse_size(v: &toml::Value) -> anyhow::Result<(usize, String)> {
    let (bytes, text) = match v {
        toml::Value::Integer(n) => (u64::try_from(*n).ok(), n.to_string()),
        toml::Value::String(s) => {
            let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
            let (digits, unit) = s.split_at(digits_end);
            let factor = match unit {
                "" | "B" => Some(1),
                "KiB" => Some(KIB as u64),
                "MiB" => Some(MIB as u64),
                "GiB" => Some(GIB as u64),
                _ => bail!("{s:?} is not a size: write a number of bytes, or a number then B, KiB, MiB or GiB"),
            };
            let n: Option<u64> = digits.parse().ok();
            (n.zip(factor).and_then(|(n, f)| n.checked_mul(f)), s.clone())
        }
        other => bail!("must be a size such as \"512KiB\", got {other}"),
    };
    match bytes.and_then(|b| usize::try_from(b).ok()) {
        Some(0) => bail!("{text} must be above zero"),
        Some(b) => Ok((b, text)),
        None => bail!("{text} is not a size this machine can hold"),
    }
}

/// A size for a message: the largest whole unit, `512 KiB`.
fn size_text(bytes: usize) -> String {
    let (n, unit) = split_unit(bytes);
    format!("{n} {unit}")
}

/// A size for a config line: `512KiB`.
fn size_config(bytes: usize) -> String {
    let (n, unit) = split_unit(bytes);
    format!("{n}{unit}")
}

fn split_unit(bytes: usize) -> (usize, &'static str) {
    if bytes != 0 && bytes.is_multiple_of(GIB) {
        (bytes / GIB, "GiB")
    } else if bytes != 0 && bytes.is_multiple_of(MIB) {
        (bytes / MIB, "MiB")
    } else if bytes != 0 && bytes.is_multiple_of(KIB) {
        (bytes / KIB, "KiB")
    } else {
        (bytes, "B")
    }
}

/// An owned byte area that follows a [`Limit`].
///
/// Made at the limit's `start`. A write that does not fit grows it by
/// doubling, clamped at `max`, and keeps the new size. A write that needs more
/// than `max` fails with an `io::Error` and leaves the size needed in
/// [`BoundedBuf::overflow`], so the caller can build the refusal with
/// [`Limit::exceeded`] and the name of what it was writing.
#[derive(Debug)]
pub struct BoundedBuf {
    limit: Limit,
    bytes: Vec<u8>,
    /// The size the area has grown to. The `Vec` holds at least this much.
    size: usize,
    overflow: Option<usize>,
}

impl BoundedBuf {
    /// Allocate the area at the limit's `start`. Start only.
    pub fn new(limit: &Limit) -> Self {
        Self {
            limit: limit.clone(),
            bytes: Vec::with_capacity(limit.start),
            size: limit.start,
            overflow: None,
        }
    }

    pub fn limit(&self) -> &Limit {
        &self.limit
    }

    /// The bytes written since the last [`BoundedBuf::clear`].
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The size the area has grown to, in bytes.
    pub fn capacity(&self) -> usize {
        self.size
    }

    /// Empty the area for the next use. The capacity stays.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.overflow = None;
    }

    /// The size the last refused write needed, if one was refused since
    /// [`BoundedBuf::clear`].
    pub fn overflow(&self) -> Option<usize> {
        self.overflow
    }

    /// Make room for `needed` bytes in all, or record the refusal.
    fn reserve_total(&mut self, needed: usize) -> bool {
        if needed <= self.size {
            return true;
        }
        if needed > self.limit.max {
            self.overflow = Some(needed);
            return false;
        }
        let Some(size) = self.limit.grown(self.size, needed) else {
            self.overflow = Some(needed);
            return false;
        };
        self.bytes.reserve_exact(size - self.bytes.len());
        self.size = size;
        self.limit.log_growth(size);
        true
    }
}

impl io::Write for BoundedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let needed = self.bytes.len().saturating_add(buf.len());
        if !self.reserve_total(needed) {
            return Err(io::Error::other(format!(
                "limit {} is full at {}",
                self.limit.name,
                size_text(self.limit.max)
            )));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A first-in first-out queue that follows a [`Limit`] counted in entries.
///
/// Made at the limit's `start`. It grows by doubling when full, clamped at
/// `max`, keeps its size, and a [`BoundedQueue::push_back`] past `max` is
/// refused with the error and the item handed back, never dropped silently.
#[derive(Debug)]
pub struct BoundedQueue<T> {
    limit: Limit,
    items: std::collections::VecDeque<T>,
    /// The entries the queue has grown to hold.
    size: usize,
}

impl<T> BoundedQueue<T> {
    /// Allocate room for the limit's `start` entries. Start only.
    pub fn new(limit: &Limit) -> Self {
        Self {
            limit: limit.clone(),
            items: std::collections::VecDeque::with_capacity(limit.start()),
            size: limit.start(),
        }
    }

    pub fn limit(&self) -> &Limit {
        &self.limit
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The entries the queue has grown to hold.
    pub fn capacity(&self) -> usize {
        self.size
    }

    pub fn front(&self) -> Option<&T> {
        self.items.front()
    }

    pub fn pop_front(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn contains(&self, item: &T) -> bool
    where
        T: PartialEq,
    {
        self.items.contains(item)
    }

    /// Add `item` at the back. Past the maximum the queue is unchanged and the
    /// item is returned beside the error, naming `what` was refused.
    pub fn push_back(&mut self, item: T, what: &str) -> Result<(), (T, LimitExceeded)> {
        let needed = self.items.len() + 1;
        let Some(size) = self.limit.grown(self.size, needed) else {
            return Err((item, self.limit.exceeded(needed, what)));
        };
        if size > self.size {
            self.items.reserve_exact(size - self.items.len());
            self.size = size;
            self.limit.log_growth(size);
        }
        self.items.push_back(item);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const FILE: &str = "/etc/m6/site.toml";

    fn parse(body: &str, page: usize) -> anyhow::Result<Limits> {
        let v: toml::Value = toml::from_str(body).unwrap();
        Limits::from_toml(v.get("limits"), Path::new(FILE), page)
    }

    fn render_page(l: &Limits) -> Limit {
        l.get(&RENDER_PAGE).clone()
    }

    #[test]
    fn no_table_is_the_defaults_and_silent() {
        for page in [4096, 16384] {
            let l = parse("", page).unwrap();
            assert_eq!(render_page(&l).start(), 512 * KIB);
            assert_eq!(render_page(&l).max(), 4 * MIB);
            assert!(l.warnings().is_empty());
        }
    }

    #[test]
    fn set_values_are_read_in_every_unit() {
        let l = parse(
            "[limits.render_page]\nstart = \"1MiB\"\nmax = 8388608\n",
            4096,
        )
        .unwrap();
        assert_eq!(render_page(&l).start(), MIB);
        assert_eq!(render_page(&l).max(), 8 * MIB);
        assert!(l.warnings().is_empty());
        let l = parse("[limits.render_page]\nmax = \"1GiB\"\n", 4096).unwrap();
        assert_eq!(render_page(&l).max(), GIB);
    }

    /// 500 KiB is no power-of-two multiple of a 16 KiB page: it is rounded up
    /// to 512 KiB, and the one warning names the key, the file, the value set
    /// and the value used.
    #[test]
    fn a_value_off_the_page_rule_is_rounded_up_and_warned() {
        let l = parse("[limits.render_page]\nstart = \"500KiB\"\n", 16384).unwrap();
        assert_eq!(render_page(&l).start(), 512 * KIB);
        assert_eq!(
            l.warnings(),
            [format!(
                "[limits.render_page] start = 500KiB in {FILE} is not a power-of-two multiple of the 16 KiB page; using 512 KiB. Set it to 512KiB to silence this."
            )]
        );
    }

    /// 48 KiB is a multiple of the page but not a power-of-two multiple.
    #[test]
    fn a_multiple_that_is_not_a_power_of_two_is_rounded() {
        let l = parse("[limits.render_page]\nstart = \"48KiB\"\n", 16384).unwrap();
        assert_eq!(render_page(&l).start(), 64 * KIB);
        assert_eq!(l.warnings().len(), 1);
    }

    #[test]
    fn an_aligned_value_is_silent() {
        let l = parse(
            "[limits.render_page]\nstart = \"64KiB\"\nmax = \"2MiB\"\n",
            16384,
        )
        .unwrap();
        assert!(l.warnings().is_empty());
    }

    #[test]
    fn values_that_cannot_work_refuse_to_start() {
        let cases = [
            (
                "[limits.render_page]\nstart = \"8MiB\"\n",
                "[limits.render_page] max = 4 MiB is below start = 8 MiB",
            ),
            (
                "[limits.render_page]\nmax = \"256KiB\"\n",
                "[limits.render_page] max = 256 KiB is below start = 512 KiB",
            ),
            ("[limits.render_page]\nstart = 0\n", "0 must be above zero"),
            (
                "[limits.render_page]\nstart = \"12 parsecs\"\n",
                "is not a size",
            ),
            (
                "[limits.render_page]\nstart = true\n",
                "must be a size such as",
            ),
            (
                "[limits.render_page]\nbig = 1\n",
                "unknown key [limits.render_page] big",
            ),
            (
                "[limits.render_pages]\nmax = 1\n",
                "unknown limit [limits.render_pages]",
            ),
            ("limits = 3\n", "[limits] must be a table"),
        ];
        for (body, want) in cases {
            let err = parse(body, 4096).unwrap_err().to_string();
            assert!(err.contains(want), "{body:?} gave {err}");
            if !want.starts_with('[') || want.contains("below") {
                assert!(
                    err.contains(FILE) || err.contains("must be a table"),
                    "{err}"
                );
            }
        }
    }

    fn buf(start: usize, max: usize) -> BoundedBuf {
        let body = format!("[limits.render_page]\nstart = {start}\nmax = {max}\n");
        BoundedBuf::new(&render_page(&parse(&body, 4096).unwrap()))
    }

    #[test]
    fn growth_doubles_clamps_at_max_and_never_shrinks() {
        let mut b = buf(4096, 4 * 4096);
        assert_eq!(b.capacity(), 4096);
        b.write_all(&[1; 4096]).unwrap();
        assert_eq!(b.capacity(), 4096, "a page that fits does not grow");
        b.write_all(&[1; 1]).unwrap();
        assert_eq!(b.capacity(), 2 * 4096, "one byte past start doubles");
        b.write_all(&vec![1; 3 * 4096 - 1]).unwrap();
        assert_eq!(b.capacity(), 4 * 4096, "the last step is clamped to max");
        b.clear();
        assert_eq!(b.capacity(), 4 * 4096, "clear keeps the size");
        assert!(b.as_bytes().is_empty());
    }

    #[test]
    fn one_big_write_doubles_until_it_fits() {
        let mut b = buf(4096, 64 * 4096);
        b.write_all(&vec![0; 5 * 4096]).unwrap();
        assert_eq!(b.capacity(), 8 * 4096);
    }

    /// Past max the write is refused, the area keeps its size and its bytes,
    /// and the error names the limit, the size needed, what was refused and
    /// the key and file to raise.
    #[test]
    fn past_max_the_error_names_the_key_and_the_file() {
        let mut b = buf(4096, 2 * 4096);
        b.write_all(&vec![0; 2 * 4096]).unwrap();
        assert!(b.write_all(&[0; 10]).is_err());
        assert_eq!(b.overflow(), Some(2 * 4096 + 10));
        assert_eq!(b.capacity(), 2 * 4096);
        let msg = b
            .limit()
            .exceeded(b.overflow().unwrap(), "page /blog/x")
            .to_string();
        assert_eq!(
            msg,
            format!(
                "page /blog/x needs at least 8202 B, over [limits.render_page] max = 8 KiB in {FILE}; raise max to fix this"
            )
        );
        b.clear();
        assert_eq!(b.overflow(), None);
    }

    #[test]
    fn every_known_limit_has_valid_defaults() {
        for page in [4096, 16384, 65536] {
            let l = parse("", page).unwrap();
            assert!(l.warnings().is_empty(), "{:?}", l.warnings());
            for spec in KNOWN {
                let limit = l.get(spec);
                assert!(limit.start() <= limit.max(), "{}", spec.name);
                if spec.unit != Unit::Count {
                    assert!(limit.start().is_power_of_two(), "{}", spec.name);
                    assert!(limit.max().is_power_of_two(), "{}", spec.name);
                }
            }
        }
    }

    #[test]
    fn a_count_is_used_as_written_and_a_zero_is_refused() {
        let l = parse("[limits.refresh_queue]\nstart = 10\nmax = 300\n", 4096).unwrap();
        let q = l.get(&REFRESH_QUEUE);
        assert_eq!((q.start(), q.max()), (10, 300));
        assert!(l.warnings().is_empty());
        let err = parse("[limits.refresh_queue]\nmax = 0\n", 4096)
            .unwrap_err()
            .to_string();
        assert!(err.contains("0 must be above zero"), "{err}");
        let err = parse("[limits.refresh_queue]\nmax = \"4KiB\"\n", 4096)
            .unwrap_err()
            .to_string();
        assert!(err.contains("whole number of entries"), "{err}");
    }

    /// A small area is rounded to a power of two, not to a page.
    #[test]
    fn a_small_area_rounds_to_a_power_of_two() {
        let l = parse("[limits.smtp_reply]\nstart = 600\n", 16384).unwrap();
        assert_eq!(l.get(&SMTP_REPLY).start(), 1024);
        assert!(l.warnings()[0].contains("is not a power of two; using 1 KiB"));
    }

    #[test]
    fn grown_doubles_clamps_and_refuses_past_max() {
        let limit = Limit::custom(&REQUEST_BODY, 1024, 8192);
        assert_eq!(limit.grown(1024, 1000), Some(1024));
        assert_eq!(limit.grown(1024, 1025), Some(2048));
        assert_eq!(limit.grown(2048, 5000), Some(8192));
        assert_eq!(limit.grown(8192, 8193), None);
        assert_eq!(limit.grown_to(8192, 8193, 8192 + 100), Some(8292));
        let msg = limit.exceeded(8193, "request").to_string();
        assert_eq!(
            msg,
            "request needs at least 8193 B, over [limits.request_body] max = 8 KiB in the config file; raise max to fix this"
        );
    }

    #[test]
    fn a_queue_grows_keeps_its_size_and_refuses_past_max() {
        let limit = Limit::custom(&REFRESH_QUEUE, 2, 4);
        let mut q = BoundedQueue::new(&limit);
        assert_eq!(q.capacity(), 2);
        q.push_back(1, "an item").unwrap();
        q.push_back(2, "an item").unwrap();
        assert_eq!(q.capacity(), 2, "within the start nothing grows");
        q.push_back(3, "an item").unwrap();
        assert_eq!(q.capacity(), 4, "one past the start doubles");
        q.push_back(4, "an item").unwrap();
        let (item, err) = q.push_back(5, "a background refresh").unwrap_err();
        assert_eq!(item, 5, "the refused item comes back");
        assert_eq!(
            err.to_string(),
            "a background refresh needs at least 5, over [limits.refresh_queue] max = 4 in the config file; raise max to fix this"
        );
        assert_eq!(q.len(), 4);
        q.clear();
        assert_eq!(q.capacity(), 4, "the queue never shrinks");
    }
}
