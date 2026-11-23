//! Minification for HTML, CSS, JSON and JavaScript.
//!
//! Applied before compression, and only for types the config enables. Each
//! function returns the original bytes unchanged if the input does not parse,
//! so a minifier that cannot understand a file degrades to passing it through
//! rather than corrupting it.
//!
//! JavaScript is minified by `oxc_minifier`, which parses either a classic
//! script (`.js`, and an inline `<script>` with no type) or a module (`.mjs`
//! and `type="module"`), compresses it and rewrites local bindings. Top-level
//! bindings keep their names and are never dropped, because another script on
//! the page may use them. A non-JavaScript `<script>` type is left as written.
//!
//! # The parse budget
//!
//! The parser runs on the single event loop, so an ordinary request must not be
//! able to spend an unbounded amount of it. Before a body is parsed,
//! [`try_minify_js_with`] checks two limits from config: the body's size
//! ([`crate::limits::MINIFY_PARSE`]) and its bracket nesting
//! ([`crate::limits::MINIFY_NESTING`]). A body over either is served as written,
//! with one log line, never truncated. A recursive-descent parser's stack use
//! follows nesting, and this workspace aborts on a stack overflow, so the depth
//! check is what keeps a file on disk from taking the process down on every
//! restart.
//!
//! A parse is used only when `diagnostics` is empty AND the parser did not
//! report a fatal error. `fatal_error` is the parser's own "I stopped early"
//! state, and a program from it is empty or partial; checking the diagnostics
//! alone would rely on the two fields never disagreeing.
//!
//! # Logging one fallback
//!
//! [`warn_unminified`] writes at most one line per distinct subject (a file
//! version, or a template's inline body) and carries the running total, so a
//! client looping on one URL cannot flood the journal. The file path itself
//! calls this only on the parse, from a cache keyed by path, size and mtime.
//! [`warn_unminified_render`] serves a rendered body, which has no version, so
//! it also caps new subjects in time. A handler that varies a failing body on
//! every request writes one line a second rather than one a request.

use oxc::allocator::Allocator;
use oxc::codegen::{Codegen, CodegenOptions};
use oxc::minifier::{
    CompressOptions, CompressOptionsUnused, MangleOptions, Minifier, MinifierOptions,
};
use oxc::parser::Parser;
use oxc::span::SourceType;
use std::sync::atomic::{AtomicU64, Ordering};

/// Whether JavaScript source is parsed as a classic script or as a module.
///
/// The distinction is not cosmetic: `import` and `export` are syntax errors in
/// a classic script, so a module parsed as one fails on every request. An
/// `.mjs` file and a `type="module"` inline script are modules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsKind {
    Script,
    Module,
}

impl JsKind {
    /// The kind a path names. `.mjs` is a module; every other extension is a
    /// classic script.
    pub fn for_path(path: &std::path::Path) -> Self {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("mjs") => JsKind::Module,
            _ => JsKind::Script,
        }
    }

    fn source_type(self) -> SourceType {
        match self {
            JsKind::Script => SourceType::script(),
            JsKind::Module => SourceType::mjs(),
        }
    }
}

/// The limits one parse may spend, from `[limits.minify_parse]` and
/// `[limits.minify_nesting]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseBudget {
    /// The largest body, in bytes, that will be parsed.
    pub bytes: usize,
    /// The deepest bracket nesting, in levels, that will be parsed.
    pub nesting: usize,
}

impl ParseBudget {
    /// The built-in defaults, for a caller with no config (tests, and the
    /// exported helpers that predate the config budget).
    pub fn builtin() -> Self {
        Self {
            bytes: crate::limits::MINIFY_PARSE.max,
            nesting: crate::limits::MINIFY_NESTING.max,
        }
    }

    /// The budget a service configured with `limits` allows.
    pub fn from_limits(limits: &crate::limits::Limits) -> Self {
        Self {
            bytes: limits.get(&crate::limits::MINIFY_PARSE).max(),
            nesting: limits.get(&crate::limits::MINIFY_NESTING).max(),
        }
    }
}

impl Default for ParseBudget {
    fn default() -> Self {
        Self::builtin()
    }
}

/// Why a JavaScript body was served as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JsFallback {
    /// The body is not UTF-8.
    NotUtf8,
    /// The body is larger than the parse budget.
    OverBudget { bytes: usize, budget: usize },
    /// The body nests brackets deeper than the parse budget.
    TooDeep { depth: usize, budget: usize },
    /// The parser reported a syntax error; the first one is carried.
    Parse(String),
    /// The parser stopped early (a fatal error); its program is not complete.
    Aborted,
    /// Minifying produced an empty body where the input was not empty.
    Empty,
}

impl std::fmt::Display for JsFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsFallback::NotUtf8 => f.write_str("the body is not UTF-8"),
            JsFallback::OverBudget { bytes, budget } => f.write_str(&format!(
                "the body is {bytes} bytes, over the minify_parse budget of {budget}"
            )),
            JsFallback::TooDeep { depth, budget } => f.write_str(&format!(
                "the body nests {depth} brackets, over the minify_nesting budget of {budget}"
            )),
            JsFallback::Parse(first) => write!(f, "the parser reported {first}"),
            JsFallback::Aborted => f.write_str("the parser reported a fatal error"),
            JsFallback::Empty => f.write_str("minifying produced an empty body"),
        }
    }
}

/// Minify HTML using the `minify-html` crate.
///
/// Inline `<script>` blocks are left as written. CSS and whitespace
/// minification apply. Use [`minify_html_with`] to minify the JavaScript
/// inline scripts too.
pub fn minify_html(data: &[u8]) -> Vec<u8> {
    let mut cfg = minify_html::Cfg::new();
    cfg.minify_css = true;
    // Explicit, so a change of the crate's default cannot switch it on.
    cfg.minify_js = false;
    cfg.keep_comments = false;
    cfg.keep_closing_tags = true;
    cfg.keep_spaces_between_attributes = true;
    cfg.keep_html_and_head_opening_tags = true;
    minify_html::minify(data, &cfg)
}

/// Minify HTML, and the JavaScript inline scripts in it when `inline_js` is
/// set.
///
/// A `<script>` whose body is not JavaScript is left as written. That covers
/// `application/ld+json` and every other JSON or template type, and a tag with
/// a `src` attribute. A module script is parsed as a module.
pub fn minify_html_with(data: &[u8], inline_js: bool) -> Vec<u8> {
    minify_html_with_budget(data, inline_js, ParseBudget::builtin())
}

/// [`minify_html_with`] within a service's parse budget.
pub fn minify_html_with_budget(data: &[u8], inline_js: bool, budget: ParseBudget) -> Vec<u8> {
    let html = minify_html(data);
    if inline_js {
        minify_inline_scripts_vec(html, budget)
    } else {
        html
    }
}

/// Every JavaScript body served as written since start.
static UNMINIFIED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Every body this thread has handed to the parser. One service loop is
    /// one thread, so this is the service's own count, and a test on its own
    /// thread is not disturbed by another test's parses.
    static PARSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Bodies handed to the parser on this thread since it started.
pub fn parse_count() -> u64 {
    PARSES.with(std::cell::Cell::get)
}

/// Bodies served as written since start, whether or not their line was
/// rate-limited by [`warn_unminified`] or [`warn_unminified_render`].
pub fn unminified_total() -> u64 {
    UNMINIFIED.load(Ordering::Relaxed)
}

thread_local! {
    /// The subjects whose fallback line has already been written on this
    /// thread. One service loop is one thread, so this is the service's own
    /// memory. Bounded, because a site can serve many files.
    static LOGGED_UNMINIFIED: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

thread_local! {
    /// Fallback lines this thread has written, on either path. A test has no
    /// log sink, so this is how it counts what reached the log.
    static LINES_WRITTEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

    /// When this thread last wrote a render-path line.
    static LAST_RENDER_LINE: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
}

/// The shortest gap between two render-path fallback lines.
///
/// A rendered body has no file version, and a handler that returns a different
/// body on every request would make every body a new subject. The render path
/// therefore caps new subjects in time as well: a loop on a failing handler
/// writes at most one line a second, and the running total still counts every
/// request that was served as written.
pub const UNMINIFIED_RENDER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Fallback lines written on this thread since it started, on either path.
pub fn unminified_lines() -> u64 {
    LINES_WRITTEN.with(std::cell::Cell::get)
}

/// Write the one line a body served as written gets, and count it.
///
/// `subject` names the body's version: for a file, its path, size and mtime;
/// for an inline script, a hash of the template's body. A subject already
/// logged on this thread is counted but not written again, so a loop on one
/// URL writes one line, and a changed file (a new subject) writes another.
pub fn warn_unminified(subject: &str, fallback: &JsFallback) {
    let total = UNMINIFIED.fetch_add(1, Ordering::Relaxed) + 1;
    if subject_is_new(subject) {
        write_line(subject, fallback, total);
    }
}

/// Write the one line a rendered body served as written gets, and count it.
///
/// A rendered body has no version, so a handler that varies the body on every
/// request cannot be made a finite set of subjects. This path caps new
/// subjects in time ([`UNMINIFIED_RENDER_INTERVAL`]) and still writes once per
/// subject, so a body that does not parse cannot write a line per request.
pub fn warn_unminified_render(subject: &str, fallback: &JsFallback) {
    let total = UNMINIFIED.fetch_add(1, Ordering::Relaxed) + 1;
    if render_line_due(subject, crate::platform::now()) {
        write_line(subject, fallback, total);
    }
}

fn write_line(subject: &str, fallback: &JsFallback, total: u64) {
    LINES_WRITTEN.with(|lines| lines.set(lines.get() + 1));
    crate::log::warn!(
        subject,
        reason = %fallback,
        served_unminified = total,
        "minify_js: {fallback}; serving the original bytes"
    );
}

/// Whether the render path may write a line for `subject` at `now`.
///
/// The interval is checked before the subject, so a subject held back by the
/// interval is not recorded and can log when the interval turns. A subject
/// already logged stays silent whatever the interval.
fn render_line_due(subject: &str, now: std::time::Instant) -> bool {
    let open = LAST_RENDER_LINE.with(|last| {
        !last
            .get()
            .is_some_and(|last| now.duration_since(last) < UNMINIFIED_RENDER_INTERVAL)
    });
    if !open || !subject_is_new(subject) {
        return false;
    }
    LAST_RENDER_LINE.with(|last| last.set(Some(now)));
    true
}

/// Whether `subject` has not been logged on this thread yet, recording it if
/// so. Bounded, so a site that serves many distinct files cannot grow it
/// without limit.
fn subject_is_new(subject: &str) -> bool {
    LOGGED_UNMINIFIED.with(|logged| {
        let mut logged = logged.borrow_mut();
        if logged.iter().any(|seen| seen == subject) {
            return false;
        }
        if logged.len() >= 64 {
            logged.remove(0);
        }
        logged.push(subject.to_string());
        true
    })
}

/// Minify one classic script. Returns the input unchanged on any failure, so a
/// script the minifier cannot understand is served as written.
pub fn minify_js(data: &[u8]) -> Vec<u8> {
    minify_js_with(JsKind::Script, data, ParseBudget::builtin())
}

/// Minify one script of `kind` within `budget`, or return the input unchanged.
pub fn minify_js_with(kind: JsKind, data: &[u8], budget: ParseBudget) -> Vec<u8> {
    try_minify_js_with(kind, data, budget).unwrap_or_else(|_| data.to_vec())
}

/// Minify one classic script, or return `None` when it does not parse.
///
/// `Some(bytes)` is the minified body, or the input when minifying it would
/// not make it smaller.
pub fn try_minify_js(data: &[u8]) -> Option<Vec<u8>> {
    try_minify_js_with(JsKind::Script, data, ParseBudget::builtin()).ok()
}

/// Minify one script of `kind`, or say why it is served as written.
///
/// The body's size and bracket nesting are checked before the parser runs, so
/// a body the loop cannot afford is never handed to the recursive descent. A
/// fatal error or any diagnostic is a fallback, and the program a fatal error
/// leaves behind is never minified.
pub fn try_minify_js_with(
    kind: JsKind,
    data: &[u8],
    budget: ParseBudget,
) -> Result<Vec<u8>, JsFallback> {
    if data.len() > budget.bytes {
        return Err(JsFallback::OverBudget {
            bytes: data.len(),
            budget: budget.bytes,
        });
    }
    let Ok(source) = std::str::from_utf8(data) else {
        return Err(JsFallback::NotUtf8);
    };
    let depth = max_nesting(source);
    if depth > budget.nesting {
        return Err(JsFallback::TooDeep {
            depth,
            budget: budget.nesting,
        });
    }
    PARSES.with(|parses| parses.set(parses.get() + 1));
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, kind.source_type()).parse();
    parse_verdict(&parsed.diagnostics, parsed.fatal_error)?;
    let mut program = parsed.program;
    // Top-level names are the page's globals. Another script may call them, and
    // an inline handler may name them, so they keep their names and no
    // unreferenced top-level declaration is dropped. Local names are mangled.
    let options = MinifierOptions {
        mangle: Some(MangleOptions {
            top_level: Some(false),
            ..MangleOptions::default()
        }),
        mangle_properties: None,
        compress: Some(CompressOptions {
            unused: CompressOptionsUnused::Keep,
            ..CompressOptions::default()
        }),
    };
    let minified = Minifier::new(options).minify(&allocator, &mut program);
    let code = Codegen::new()
        .with_options(CodegenOptions {
            minify: true,
            ..CodegenOptions::default()
        })
        .with_scoping(minified.scoping)
        .with_private_member_mappings(minified.class_private_mappings)
        .build(&program)
        .code;
    if code.is_empty() && !source.trim().is_empty() {
        return Err(JsFallback::Empty);
    }
    // A body that grew is not worth sending. This also leaves an already
    // minified script alone.
    if code.len() >= data.len() {
        return Ok(data.to_vec());
    }
    Ok(code.into_bytes())
}

/// Whether a finished parse may be minified, which is the rule the production
/// path applies to every parse.
///
/// `fatal_error` is checked first. The parser's contract says a fatal error
/// leaves the program empty and a diagnostic, but the truncated-script guard
/// must not depend on that agreement: a fatal error is a fallback even with no
/// diagnostic. Any diagnostic is a fallback too, because a program from a
/// recoverable error is not the program that was written.
fn parse_verdict(
    diagnostics: &oxc::diagnostics::Diagnostics,
    fatal_error: bool,
) -> Result<(), JsFallback> {
    if fatal_error {
        return Err(JsFallback::Aborted);
    }
    match diagnostics.first() {
        Some(first) => Err(JsFallback::Parse(first.to_string())),
        None => Ok(()),
    }
}

/// The deepest bracket nesting `source` reaches, ignoring string literals and
/// comments.
///
/// This is a guard, not a parser: it walks bytes once, tracks the four bracket
/// kinds, and never allocates more than one frame per level. A template
/// literal's `${...}` expression is code and is scanned as code. A regular
/// expression is skipped when the preceding token can end before one, and
/// treated as code otherwise, which can only over-count a body and make the
/// minifier decline it. Nothing here can make a body look shallower than it is.
fn max_nesting(source: &str) -> usize {
    let bytes = source.as_bytes();
    // The closer each open frame is waiting for. `T` marks a template
    // expression, so the `}` that ends `${` can return to the template text.
    let mut stack: Vec<u8> = Vec::new();
    let mut depth = 0usize;
    let mut max = 0usize;
    let mut prev = Prev::Start;
    let mut in_template = false;
    let mut i = 0usize;
    while i < bytes.len() {
        if in_template {
            match bytes[i] {
                b'\\' => i += 2,
                b'`' => {
                    in_template = false;
                    prev = Prev::End;
                    i += 1;
                }
                b'$' if bytes.get(i + 1) == Some(&b'{') => {
                    stack.push(b'T');
                    stack.push(b'}');
                    depth += 1;
                    max = max.max(depth);
                    prev = Prev::Op;
                    in_template = false;
                    i += 2;
                }
                _ => i += 1,
            }
            continue;
        }
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'\'' | b'"' => {
                i = skip_string(bytes, i, bytes[i]);
                prev = Prev::End;
            }
            b'`' => {
                in_template = true;
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'/' if prev.allows_regex() => {
                i = skip_regex(bytes, i);
                prev = Prev::End;
            }
            b'/' => {
                prev = Prev::Op;
                i += 1;
            }
            b'(' => {
                stack.push(b')');
                depth += 1;
                prev = Prev::Op;
                i += 1;
            }
            b'[' => {
                stack.push(b']');
                depth += 1;
                prev = Prev::Op;
                i += 1;
            }
            b'{' => {
                stack.push(b'}');
                depth += 1;
                prev = Prev::Op;
                i += 1;
            }
            b')' | b']' | b'}' => {
                if stack.last() == Some(&bytes[i]) {
                    stack.pop();
                    depth = depth.saturating_sub(1);
                    if bytes[i] == b'}' && stack.last() == Some(&b'T') {
                        stack.pop();
                        in_template = true;
                    }
                }
                prev = Prev::End;
                i += 1;
            }
            b if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' => {
                prev = Prev::Word;
                i += 1;
            }
            b'.' => {
                // After a dot comes a name, never a regular expression.
                prev = Prev::End;
                i += 1;
            }
            _ => {
                prev = Prev::Op;
                i += 1;
            }
        }
        max = max.max(depth);
    }
    max
}

/// What the previous significant token was, for the one regular-expression
/// decision a byte scan can make safely.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prev {
    Start,
    Word,
    End,
    Op,
}

impl Prev {
    /// A `/` here can begin a regular expression. A `/` after a token that can
    /// end an expression is division.
    fn allows_regex(self) -> bool {
        matches!(self, Prev::Start | Prev::Op)
    }
}

/// The index just past the string opening at `start` with `quote`.
fn skip_string(bytes: &[u8], mut i: usize, quote: u8) -> usize {
    i += 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// The index just past the regular expression opening at `i`.
///
/// An unterminated literal ends the scan at the end of the body: the parser
/// will report it, and everything it would have contained is skipped here.
fn skip_regex(bytes: &[u8], mut i: usize) -> usize {
    i += 1;
    let mut in_class = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'[' => {
                in_class = true;
                i += 1;
            }
            b']' => {
                in_class = false;
                i += 1;
            }
            b'/' if !in_class => return i + 1,
            b'\n' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Minify the JavaScript inline `<script>` bodies in an HTML document.
///
/// The scanner walks tags, so a `<script>` in an attribute value is data. A
/// body is minified only when the tag has no `src` and its `type` is a
/// JavaScript type: an absent type or `text/javascript` is a classic script,
/// and `type="module"` is parsed as a module. Any other type keeps its bytes.
pub fn minify_inline_scripts(html: &[u8]) -> Vec<u8> {
    minify_inline_scripts_budget(html, ParseBudget::builtin())
}

/// [`minify_inline_scripts`] within a service's parse budget.
pub fn minify_inline_scripts_budget(html: &[u8], budget: ParseBudget) -> Vec<u8> {
    minify_inline_scripts_vec(html.to_vec(), budget)
}

/// The owned form, so a page with no JavaScript script returns its bytes
/// without a second allocation. The page path is measured, and one allocation
/// there is a finding.
fn minify_inline_scripts_vec(html: Vec<u8>, budget: ParseBudget) -> Vec<u8> {
    let text = match std::str::from_utf8(&html) {
        Ok(text) => text,
        Err(_) => return html,
    };
    if !has_inline_script(text) {
        return html;
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"<!--") {
            match find_ci(&bytes[i + 4..], b"-->") {
                Some(offset) => {
                    i += 4 + offset + 3;
                    continue;
                }
                None => break,
            }
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            match tag_end(bytes, i) {
                Some(end) => {
                    i = end;
                    continue;
                }
                None => break,
            }
        }
        if starts_with_ci(&bytes[i..], b"<script") && is_tag_boundary(bytes.get(i + 7).copied()) {
            let Some(open_end) = tag_end(bytes, i) else {
                break;
            };
            let Some(close) = find_ci(&bytes[open_end..], b"</script").map(|o| open_end + o) else {
                break;
            };
            let Some(close_end) = tag_end(bytes, close) else {
                break;
            };
            let tag = &text[i..open_end];
            let body = &text[open_end..close];
            let replacement = match script_kind(tag) {
                Some(kind) if !body.trim().is_empty() => minify_inline_body(body, kind, budget),
                _ => body.as_bytes().to_vec(),
            };
            out.push_str(&text[copied..open_end]);
            match std::str::from_utf8(&replacement) {
                Ok(body) => out.push_str(body),
                Err(_) => out.push_str(body),
            }
            copied = close;
            i = close_end;
            continue;
        }
        match tag_end(bytes, i) {
            Some(end) => i = end,
            None => break,
        }
    }
    out.push_str(&text[copied..]);
    out.into_bytes()
}

/// Whether the document has at least one `<script>` this pass would rewrite.
/// Scans the same tags as the rewrite and allocates nothing.
fn has_inline_script(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"<!--") {
            match find_ci(&bytes[i + 4..], b"-->") {
                Some(offset) => {
                    i += 4 + offset + 3;
                    continue;
                }
                None => return false,
            }
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            match tag_end(bytes, i) {
                Some(end) => {
                    i = end;
                    continue;
                }
                None => return false,
            }
        }
        if starts_with_ci(&bytes[i..], b"<script") && is_tag_boundary(bytes.get(i + 7).copied()) {
            let Some(open_end) = tag_end(bytes, i) else {
                return false;
            };
            let Some(close) = find_ci(&bytes[open_end..], b"</script").map(|o| open_end + o) else {
                return false;
            };
            if script_kind(&text[i..open_end]).is_some() && !text[open_end..close].trim().is_empty()
            {
                return true;
            }
            let Some(close_end) = tag_end(bytes, close) else {
                return false;
            };
            i = close_end;
            continue;
        }
        match tag_end(bytes, i) {
            Some(end) => i = end,
            None => return false,
        }
    }
    false
}

fn minify_inline_body(body: &str, kind: JsKind, budget: ParseBudget) -> Vec<u8> {
    let minified = match try_minify_js_with(kind, body.as_bytes(), budget) {
        Ok(minified) => minified,
        Err(fallback) => {
            warn_unminified(&inline_subject(body), &fallback);
            return body.as_bytes().to_vec();
        }
    };
    if contains_ci(&minified, b"</script") {
        let fallback = JsFallback::Parse("an output close tag".to_string());
        warn_unminified(&inline_subject(body), &fallback);
        return body.as_bytes().to_vec();
    }
    minified
}

/// A stable name for one body, for the fallback's one-line-per-body rule. A
/// changed body logs again; an unchanged one does not.
pub fn body_subject(prefix: &str, body: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    format!("{prefix}:{}:{:x}", body.len(), hasher.finish())
}

/// A stable name for one template's inline body, for the fallback's
/// one-line-per-body rule. The body is what the template holds, so a changed
/// template logs again and an unchanged one does not.
fn inline_subject(body: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    format!("inline:{}:{:x}", body.len(), hasher.finish())
}

fn is_tag_boundary(next: Option<u8>) -> bool {
    match next {
        None => true,
        Some(byte) => matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/'),
    }
}

/// The index just past the `>` that closes the tag opened at `start`, honouring
/// quoted attribute values.
fn tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut quote = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
        } else if byte == b'"' || byte == b'\'' {
            quote = byte;
        } else if byte == b'>' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// What a `<script ...>` tag opens, when the minifier can read its body.
///
/// A `src` tag has no body. A non-JavaScript type is left alone. `type="module"`
/// is a module and everything else JavaScript is a classic script.
fn script_kind(tag: &str) -> Option<JsKind> {
    let bytes = tag.as_bytes();
    let mut i = 7;
    let mut kind: Option<String> = None;
    let mut language: Option<String> = None;
    let mut has_src = false;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b'>' {
            break;
        }
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let name = tag[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let value_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                value = tag[value_start..i].to_string();
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let value_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                    i += 1;
                }
                value = tag[value_start..i].to_string();
            }
        }
        let value = value.trim().to_ascii_lowercase();
        match name.as_str() {
            "src" => has_src = true,
            "type" => kind = Some(value),
            "language" => language = Some(value),
            _ => {}
        }
    }
    if has_src {
        return None;
    }
    if let Some(kind) = kind {
        if kind.is_empty() {
            return language
                .as_deref()
                .is_none_or(is_javascript_language)
                .then_some(JsKind::Script);
        }
        if kind == "module" {
            return Some(JsKind::Module);
        }
        return is_javascript_type(&kind).then_some(JsKind::Script);
    }
    language
        .as_deref()
        .is_none_or(is_javascript_language)
        .then_some(JsKind::Script)
}

fn is_javascript_type(kind: &str) -> bool {
    let kind = kind.split(';').next().unwrap_or(kind).trim();
    matches!(
        kind,
        "text/javascript"
            | "application/javascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "application/ecmascript"
            | "text/jscript"
            | "text/livescript"
            | "text/x-javascript"
    ) || kind.starts_with("text/javascript1.")
}

fn is_javascript_language(language: &str) -> bool {
    language.contains("javascript") || language.contains("ecmascript")
}

fn starts_with_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    find_ci(haystack, needle).is_some()
}

fn find_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

/// Minify CSS: strip /* ... */ comments and collapse runs of whitespace.
///
/// Quoted strings (single and double, honouring backslash escapes) and
/// `url(...)` contents are copied verbatim, so a `/*` or a run of whitespace
/// inside them is data and is left alone. Does not attempt to remove last
/// semicolons or perform structural optimisation.
pub fn minify_css(data: &[u8]) -> Vec<u8> {
    let input = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(_) => return data.to_vec(), // non-UTF-8: pass through untouched
    };
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut last_was_ws = false;
    while i < chars.len() {
        match chars[i] {
            // Strip /* ... */ comments.
            '/' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                // skip the closing */
                i += 2;
                // A comment separates tokens like whitespace does, so emit a
                // space here, or `red/*c*/blue` merges into one identifier.
                if !last_was_ws {
                    out.push(' ');
                    last_was_ws = true;
                }
            }
            // Copy quoted strings verbatim, honouring backslash escapes.
            '"' | '\'' => {
                copy_string(&mut out, &chars, &mut i);
                last_was_ws = false;
            }
            // Copy url(...) contents verbatim.
            'u' | 'U' if url_open(&chars, i) => {
                out.push_str("url(");
                i += 4;
                copy_url(&mut out, &chars, &mut i);
                last_was_ws = false;
            }
            // Collapse runs of whitespace to a single space.
            c if c.is_ascii_whitespace() => {
                if !last_was_ws {
                    out.push(' ');
                    last_was_ws = true;
                }
                i += 1;
            }
            c => {
                last_was_ws = false;
                out.push(c);
                i += 1;
            }
        }
    }
    out.trim().as_bytes().to_vec()
}

/// Copy the quoted string at `chars[i]` (the opening quote) verbatim, honouring
/// backslash escapes, and advance `i` past the closing quote.
fn copy_string(out: &mut String, chars: &[char], i: &mut usize) {
    let quote = chars[*i];
    out.push(quote);
    *i += 1;
    while *i < chars.len() {
        let c = chars[*i];
        out.push(c);
        *i += 1;
        if c == '\\' {
            // Copy the escaped character too, so an escaped quote does not end
            // the string early.
            if *i < chars.len() {
                out.push(chars[*i]);
                *i += 1;
            }
        } else if c == quote {
            return;
        }
    }
}

/// Copy the contents of a `url(...)` call, starting just after the opening
/// `(`, verbatim, and advance `i` past the closing `)`. A quoted string inside
/// the call is copied whole so a `)` inside it does not end the call early.
fn copy_url(out: &mut String, chars: &[char], i: &mut usize) {
    while *i < chars.len() {
        match chars[*i] {
            '"' | '\'' => copy_string(out, chars, i),
            '\\' => {
                out.push('\\');
                *i += 1;
                if *i < chars.len() {
                    out.push(chars[*i]);
                    *i += 1;
                }
            }
            ')' => {
                out.push(')');
                *i += 1;
                return;
            }
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
}

/// Whether `chars[i..]` begins a `url(` call: the `url` keyword, case-
/// insensitive, immediately before the `(`, and not part of a longer identifier
/// such as `myurl(`.
fn url_open(chars: &[char], i: usize) -> bool {
    i + 3 < chars.len()
        && chars[i + 1].eq_ignore_ascii_case(&'r')
        && chars[i + 2].eq_ignore_ascii_case(&'l')
        && chars[i + 3] == '('
        && (i == 0 || !is_ident_char(chars[i - 1]))
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

/// Minify JSON: parse and re-serialise in compact form.
///
/// Returns original bytes if the body is not valid JSON (conservative — no corruption risk).
pub fn minify_json(data: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<serde_json::Value>(data) {
        Ok(v) => serde_json::to_vec(&v).unwrap_or_else(|_| data.to_vec()),
        Err(_) => data.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minify_css_strips_comments() {
        let css = b"/* header styles */\nbody { color: red; /* inline */ }";
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(!s.contains("/*"), "comment not stripped: {}", s);
        assert!(s.contains("color: red"), "rule missing: {}", s);
    }

    #[test]
    fn test_minify_css_collapses_whitespace() {
        let css = b"body   {\n    color:   red;\n}";
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        // Multiple spaces should be collapsed to one.
        assert!(!s.contains("   "), "whitespace not collapsed: {}", s);
    }

    #[test]
    fn test_minify_css_preserves_double_quoted_string() {
        let css = br#"content: "a  /* b */  c""#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains(r#""a  /* b */  c""#), "string corrupted: {}", s);
    }

    #[test]
    fn test_minify_css_preserves_url_contents() {
        let css = br#"background: url("data:image/svg+xml;utf8,<svg>  </svg>")"#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains(r#"url("data:image/svg+xml;utf8,<svg>  </svg>")"#),
            "url corrupted: {}",
            s
        );
    }

    #[test]
    fn test_minify_css_preserves_single_quoted_string() {
        let css = br#"a::before { content: '/*' }"#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("'/*'"), "string treated as comment: {}", s);
    }

    #[test]
    fn test_minify_css_preserves_escaped_quotes_in_strings() {
        let css = br#"x{content:"a\"  b"}"#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains(r#""a\"  b""#),
            "escaped quote broke the string: {}",
            s
        );
    }

    #[test]
    fn test_minify_css_preserves_escaped_paren_in_url() {
        let css = br#"a{background:url(a\)/*b*/c)}"#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains(r#"url(a\)/*b*/c)"#),
            "escaped ) ended the url early: {}",
            s
        );
    }

    #[test]
    fn test_minify_css_comment_still_separates_tokens() {
        let css = br#"a{color:red/*c*/blue}"#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains("red blue"),
            "comment did not separate tokens: {}",
            s
        );
        assert!(
            !s.contains("redblue"),
            "tokens merged across a comment: {}",
            s
        );
    }

    #[test]
    fn test_minify_json_compacts() {
        let json = b"{\n  \"key\": \"value\",\n  \"n\": 42\n}";
        let out = minify_json(json);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(!s.contains('\n'), "newlines not removed: {}", s);
        assert!(s.contains("\"key\""), "key missing");
    }

    #[test]
    fn test_minify_json_passthrough_on_invalid() {
        let bad = b"not json {{";
        let out = minify_json(bad);
        assert_eq!(&out, bad);
    }

    /// An inline script goes through `minify_html` as written. The engine
    /// that was switched on for it turned this string into a template literal
    /// with doubled escapes.
    #[test]
    fn test_minify_html_leaves_an_inline_script_as_written() {
        let html = b"<html><body><script>var m = \"That didn't verify \\u2014 tab\\there.\";</script></body></html>";
        let out = minify_html(html);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains("var m = \"That didn't verify \\u2014 tab\\there.\";"),
            "inline script changed: {s}"
        );
    }

    #[test]
    fn test_minify_html_removes_comments() {
        let html = b"<html><body><!-- comment --><p>Hello</p></body></html>";
        let out = minify_html(html);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("Hello"), "content missing: {}", s);
    }
    /// A module must be parsed as a module. The wrong outcome: `import` is a
    /// syntax error in classic-script mode, so the file falls back on every
    /// request and the journal fills.
    #[test]
    fn a_module_is_parsed_as_a_module() {
        let module = b"import x from \"y\"; export default x;";
        assert!(
            try_minify_js_with(JsKind::Module, module, ParseBudget::builtin()).is_ok(),
            "a module parsed as a module must minify"
        );
        assert!(
            try_minify_js_with(JsKind::Script, module, ParseBudget::builtin()).is_err(),
            "the same bytes as a classic script are a syntax error"
        );
    }

    /// A body over the byte budget is served as written, before the parser
    /// runs. The budget is the config's, not a number invented here.
    #[test]
    fn a_body_over_the_parse_budget_is_not_parsed() {
        let body = b"var x = 1;";
        let budget = ParseBudget {
            bytes: body.len() - 1,
            nesting: 1024,
        };
        assert_eq!(
            try_minify_js_with(JsKind::Script, body, budget),
            Err(JsFallback::OverBudget {
                bytes: body.len(),
                budget: body.len() - 1,
            })
        );
        assert_eq!(minify_js_with(JsKind::Script, body, budget), body);
    }

    /// A body nested past the budget is refused before the recursive descent.
    /// The wrong outcome is a stack overflow, which aborts the process and
    /// happens again on every restart for the same file.
    #[test]
    fn a_body_nested_past_the_budget_is_not_parsed() {
        let body = format!("var x = {}1{};", "(".repeat(2_000), ")".repeat(2_000));
        let budget = ParseBudget {
            bytes: body.len(),
            nesting: 64,
        };
        match try_minify_js_with(JsKind::Script, body.as_bytes(), budget) {
            Err(JsFallback::TooDeep { depth, budget: 64 }) => {
                assert!(depth >= 2_000, "the depth was measured: {depth}");
            }
            other => panic!("expected TooDeep, got {other:?}"),
        }
        assert_eq!(
            minify_js_with(JsKind::Script, body.as_bytes(), budget),
            body.as_bytes(),
            "a body over the nesting budget is served as written"
        );
    }

    /// Strings, comments, template text and regular expressions do not count as
    /// code nesting, so ordinary JavaScript is not refused.
    #[test]
    fn nesting_ignores_strings_comments_templates_and_regexes() {
        let body = br#"
            var s = "((((((((((((((((((((";
            // ((((((((((((((((((((
            /* (((((((((((((((((((( */
            var t = `]]]]]]]]]]]]]]] ${ (1 + (2 * (3))) } ]]]]];
            var r = /[((((((((((((((([/;
            f(g(h(1)));
        "#;
        assert!(
            max_nesting(std::str::from_utf8(body).unwrap()) < 8,
            "the scan counted brackets inside a literal: {}",
            max_nesting(std::str::from_utf8(body).unwrap())
        );
    }

    /// A parse that stopped part-way is a fallback, and the original bytes are
    /// served. The wrong outcome: the partial program is minified, so a
    /// truncated script is sent.
    #[test]
    fn a_partial_parse_serves_the_original_bytes() {
        let body = b"function f() { var x = 1; if (x) {";
        assert!(
            try_minify_js_with(JsKind::Script, body, ParseBudget::builtin()).is_err(),
            "an unterminated body must not reach the minifier"
        );
        assert_eq!(
            minify_js(body),
            body,
            "a body that fails part-way is served byte for byte"
        );
    }

    /// The completion rule that closes the truncated-script hole, against the
    /// production verdict rather than a copy of it: a fatal error is a fallback
    /// even if the diagnostics list is empty.
    #[test]
    fn a_fatal_error_is_a_fallback_even_with_no_diagnostics() {
        let empty = oxc::diagnostics::Diagnostics::new();
        assert_eq!(
            parse_verdict(&empty, true),
            Err(JsFallback::Aborted),
            "fatal with no diagnostics"
        );
        assert_eq!(
            parse_verdict(&empty, false),
            Ok(()),
            "a clean parse is minified"
        );
    }

    /// One line per subject: a loop on the same file version writes once, and a
    /// changed version (a new subject) writes again.
    #[test]
    fn one_line_is_written_for_each_subject() {
        let subject = "minify-test-one-line:/tmp/module.mjs:100:7";
        assert!(subject_is_new(subject), "the first fallback logs");
        assert!(
            !subject_is_new(subject),
            "the same version does not log again"
        );
        assert!(
            subject_is_new("minify-test-one-line:/tmp/module.mjs:101:7"),
            "a changed version logs again"
        );
    }
}
