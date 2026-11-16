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

/// A limit's name and its built-in defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitSpec {
    /// The key under `[limits]`.
    pub name: &'static str,
    /// The size made at start, in bytes.
    pub start: usize,
    /// The size growth stops at, in bytes.
    pub max: usize,
}

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const GIB: usize = 1024 * MIB;

/// The working area one rendered page is written into.
pub const RENDER_PAGE: LimitSpec = LimitSpec {
    name: "render_page",
    start: 512 * KIB,
    max: 4 * MIB,
};

/// Every limit m6 knows. A name outside this list under `[limits]` is refused.
pub const KNOWN: &[LimitSpec] = &[RENDER_PAGE];

/// One limit, with its values rounded and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limit {
    name: &'static str,
    start: usize,
    max: usize,
    file: PathBuf,
}

impl Limit {
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
            needed,
            name: self.name,
            max: self.max,
            file: self.file.clone(),
        }
    }
}

/// A write needed more than a limit's `max`.
#[derive(Debug)]
pub struct LimitExceeded {
    what: String,
    needed: usize,
    name: &'static str,
    max: usize,
    file: PathBuf,
}

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} needs at least {}, over [limits.{}] max = {} in {}; raise max to fix this",
            self.what,
            size_text(self.needed),
            self.name,
            size_text(self.max),
            self.file.display()
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
                    return Ok(round_up(default, page));
                };
                let (bytes, text) = parse_size(v).map_err(|e| {
                    anyhow::anyhow!("[limits.{}] {key} in {}: {e}", spec.name, file.display())
                })?;
                let used = round_up(bytes, page);
                if used != bytes {
                    warnings.push(format!(
                        "[limits.{}] {key} = {text} in {} is not a power-of-two multiple of the {} page; using {}. Set it to {} to silence this.",
                        spec.name,
                        file.display(),
                        size_text(page),
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
                    size_text(max),
                    size_text(start),
                    file.display()
                );
            }
            limits.push(Limit {
                name: spec.name,
                start,
                max,
                file: file.to_path_buf(),
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

/// The smallest `page * 2^k` that holds `bytes`.
fn round_up(bytes: usize, page: usize) -> usize {
    page * bytes.div_ceil(page).next_power_of_two()
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
        let mut size = self.size;
        while size < needed {
            size = size.saturating_mul(2);
        }
        let size = size.min(self.limit.max);
        self.bytes.reserve_exact(size - self.bytes.len());
        self.size = size;
        info!(
            limit = self.limit.name,
            bytes = size,
            "limit {} grew to {}",
            self.limit.name,
            size_text(size)
        );
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
}
