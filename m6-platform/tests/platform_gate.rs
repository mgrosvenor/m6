//! The gate that keeps the operating system inside the platform layer.
//!
//! Every `.rs` file and every `Cargo.toml` in the workspace is read, tests and
//! benches included. Outside `m6-platform/src/`, no file may name a platform in a `cfg`, name one of
//! the four OS crates, read the clock directly in shipping code, or read a
//! service manager's variable. The two OS files must
//! define the same items, so a run on one OS sees an arm the other lacks.

use std::path::{Path, PathBuf};

/// The workspace root: the parent of m6-platform.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("m6-platform sits in the workspace")
        .to_path_buf()
}

/// Inside the layer: the one place the OS is named. This file is excluded as
/// well, because it spells the words it refuses.
fn in_layer(rel: &Path) -> bool {
    rel.starts_with("m6-platform/src") || rel == Path::new("m6-platform/tests/platform_gate.rs")
}

/// Every file under the root with the given name or extension, skipping the
/// build output and git, as paths relative to the root.
fn files(root: &Path, want: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    fn walk(dir: &Path, want: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if name != "target" && name != ".git" {
                    walk(&path, want, out);
                }
            } else if want(&path) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, want, &mut out);
    out.sort();
    out.into_iter()
        .map(|p| p.strip_prefix(root).expect("under the root").to_path_buf())
        .collect()
}

fn rust_files(root: &Path) -> Vec<PathBuf> {
    files(root, &|p| p.extension().is_some_and(|e| e == "rs"))
}

/// Lines of `rel` that `offends`, as "path:line: text".
fn offending_lines(root: &Path, rel: &Path, offends: &dyn Fn(&str) -> bool) -> Vec<String> {
    let text = std::fs::read_to_string(root.join(rel)).expect("read the file");
    text.lines()
        .enumerate()
        .filter(|(_, line)| offends(line))
        .map(|(i, line)| format!("{}:{}: {}", rel.display(), i + 1, line.trim()))
        .collect()
}

fn assert_none(rule: &str, found: Vec<String>) {
    assert!(
        found.is_empty(),
        "{rule}: {} offending line(s), each belongs in m6-platform/src/:\n{}",
        found.len(),
        found.join("\n")
    );
}

/// G1. No `cfg` predicate names a platform outside the layer. Break: one
/// `#[cfg(target_os = "linux")]` added to a test file outside the layer.
#[test]
fn no_platform_conditional_outside_the_layer() {
    const WORDS: [&str; 8] = [
        "target_os",
        "target_family",
        "target_vendor",
        "target_arch",
        "target_pointer_width",
        "target_endian",
        "cfg(unix",
        "cfg(windows",
    ];
    let root = root();
    let mut found = Vec::new();
    for rel in rust_files(&root) {
        if in_layer(&rel) {
            continue;
        }
        found.extend(offending_lines(&root, &rel, &|line| {
            let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            WORDS.iter().any(|w| compact.contains(w))
        }));
    }
    assert_none("G1 (platform conditionals)", found);
}

/// G2. No OS crate is named outside the layer, in code or in a manifest.
/// Break: one `use libc::` in a test, or `nix` added to a service's
/// `[dependencies]`.
#[test]
fn no_platform_crate_outside_the_layer() {
    const CRATES: [&str; 4] = ["mio", "nix", "rustix", "libc"];
    let root = root();
    let mut found = Vec::new();
    for rel in rust_files(&root) {
        if in_layer(&rel) {
            continue;
        }
        found.extend(offending_lines(&root, &rel, &|line| {
            CRATES.iter().any(|c| names_crate(line, c))
        }));
    }
    for rel in files(&root, &|p| p.file_name().is_some_and(|n| n == "Cargo.toml")) {
        if rel == Path::new("m6-platform/Cargo.toml") {
            continue;
        }
        found.extend(offending_lines(&root, &rel, &|line| {
            let key = line.split('=').next().unwrap_or("").trim();
            CRATES.contains(&key)
        }));
    }
    assert_none("G2 (OS crates)", found);
}

/// G3. No direct clock read outside the layer in shipping code: a loop reads
/// the clock once per wake through `platform::now`, and the wall clock through
/// `platform::wall`. A test or a bench may time itself. Every path form counts,
/// not the literal `Instant::now()`: a qualified or imported `Instant::now`, a
/// clock type renamed by a `use ... as`, `SystemTime::now` passed as
/// a function value, and a path broken across lines before `::now`. Break: one
/// `std::time::Instant::now` in a service's `src/`.
#[test]
fn no_direct_clock_read_outside_the_layer() {
    let root = root();
    let mut found = Vec::new();
    for rel in rust_files(&root) {
        let under = |dir: &str| rel.components().any(|c| c.as_os_str() == dir);
        if in_layer(&rel) || under("tests") || under("benches") || under("examples") {
            continue;
        }
        found.extend(clock_reads(&root, &rel));
    }
    assert_none("G3 (direct clock reads)", found);
}

/// Lines of `rel` that read a clock directly, as "path:line: text".
fn clock_reads(root: &Path, rel: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(root.join(rel)).expect("read the file");
    let lines: Vec<&str> = text.lines().collect();
    clock_path_lines(&text)
        .into_iter()
        .map(|line| {
            let source = lines.get(line - 1).copied().unwrap_or("");
            format!("{}:{}: {}", rel.display(), line, source.trim())
        })
        .collect()
}

/// The 1-based lines in `text` that read a clock directly. A direct read is a
/// `::`-joined path ending in `now` whose segment before `now` is a type named
/// `Instant` or `SystemTime`, so every qualification matches:
///
/// - `Instant::now()`, `std::time::Instant::now()`, `time::Instant::now()`
/// - `SystemTime::now` as a function value, with no parentheses
/// - `use std::time::Instant as Clock;` then `Clock::now()`
/// - a path broken before `::now`, which rustfmt does when the line is long
///
/// A `now` method on any other type does not match, which keeps a domain type
/// out of the rule. A line that is wholly a `//` comment is not code, so a doc
/// comment that quotes the path does not match either. A `use` of the clock
/// function itself (`use std::time::Instant::now`) is named on its `use` line,
/// where the bypass starts.
fn clock_path_lines(text: &str) -> Vec<usize> {
    let clock_types = clock_type_names(text);
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut line = 1usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if !ident_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < bytes.len() && ident_byte(bytes[i]) {
            i += 1;
        }
        let method = &text[start..i];
        let clock_method = method == "now" || method == "now_utc" || method == "now_local";
        if !clock_method || is_comment_line(text, start) {
            continue;
        }
        if segment_before(text, start).is_some_and(|ty| clock_types.contains(&ty)) {
            found.push(line);
        }
    }
    found
}

/// The names that mean a clock type in `text`: `Instant`, `SystemTime`, every
/// name a `use ... as <name>` binds to one of them, and every name a
/// `type <name> = ... Instant` or `= ... SystemTime` declares. A type alias is
/// one line of legal code, so a matcher that only reads `use` misses it.
fn clock_type_names(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut names = vec![
        "Instant",
        "SystemTime",
        "Utc",
        "Local",
        "OffsetDateTime",
        "FileTime",
    ];
    names.extend(type_alias_names(text, &mut Vec::new()));
    names.sort_unstable();
    names.dedup();
    let mut i = 0usize;
    while i < bytes.len() {
        if !ident_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < bytes.len() && ident_byte(bytes[i]) {
            i += 1;
        }
        let word = &text[start..i];
        if (word != "Instant"
            && word != "SystemTime"
            && word != "Utc"
            && word != "Local"
            && word != "OffsetDateTime"
            && word != "FileTime")
            || is_comment_line(text, start)
        {
            continue;
        }
        let mut j = skip_space(bytes, i);
        let as_start = j;
        while j < bytes.len() && ident_byte(bytes[j]) {
            j += 1;
        }
        if &text[as_start..j] != "as" {
            continue;
        }
        j = skip_space(bytes, j);
        let alias_start = j;
        while j < bytes.len() && ident_byte(bytes[j]) {
            j += 1;
        }
        let alias = &text[alias_start..j];
        if !alias.is_empty() && alias != "_" && !names.contains(&alias) {
            names.push(alias);
        }
    }
    names
}

/// The names a `type <name> = ...` declaration binds when the right side
/// names `Instant` or `SystemTime`, so `type Clock = std::time::Instant;`
/// makes `Clock::now()` a path G3 sees rather than a way around it.
fn type_alias_names<'a>(text: &'a str, seen: &mut Vec<&'a str>) -> Vec<&'a str> {
    let bytes = text.as_bytes();
    let mut names = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if !ident_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < bytes.len() && ident_byte(bytes[i]) {
            i += 1;
        }
        if &text[start..i] != "type" || is_comment_line(text, start) {
            continue;
        }
        let mut j = skip_space(bytes, i);
        let name_start = j;
        while j < bytes.len() && ident_byte(bytes[j]) {
            j += 1;
        }
        let name = &text[name_start..j];
        if name.is_empty() {
            continue;
        }
        let rest_start = j;
        let rest_end = text[j..].find([';', '\n']).map_or(text.len(), |k| j + k);
        let rest = &text[rest_start..rest_end];
        // The right side must *end* in a clock type, so `type CachedErrorPage =
        // (std::time::Instant, ...)` does not make `CachedErrorPage::now()` a
        // clock read: naming a type that holds an Instant is not naming the
        // clock.
        let rhs = rest.split('=').nth(1).unwrap_or("");
        let last = rhs
            .trim()
            .trim_end_matches('>')
            .rsplit("::")
            .next()
            .unwrap_or("")
            .trim();
        let clock_types = [
            "Instant",
            "SystemTime",
            "Utc",
            "Local",
            "OffsetDateTime",
            "FileTime",
        ];
        if clock_types.contains(&last) && !seen.contains(&name) && !names.contains(&name) {
            seen.push(name);
            names.push(name);
        }
    }
    names
}

/// The identifier immediately before the `::` that precedes `at`, if `at` is
/// a path segment. Whitespace, including a newline, is skipped, so
/// `Instant\n    ::now` names `Instant` too.
fn segment_before(text: &str, at: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut j = at;
    while j > 0 && bytes[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    if j < 2 || bytes[j - 1] != b':' || bytes[j - 2] != b':' {
        return None;
    }
    j = skip_space_back(bytes, j - 2);
    let end = j;
    while j > 0 && ident_byte(bytes[j - 1]) {
        j -= 1;
    }
    if j < end {
        Some(&text[j..end])
    } else {
        None
    }
}

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn skip_space(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn skip_space_back(bytes: &[u8], mut i: usize) -> usize {
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i
}

/// Whether the line holding byte `at` is wholly a `//` comment.
fn is_comment_line(text: &str, at: usize) -> bool {
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    text[start..at].trim_start().starts_with("//")
}

/// The forms G3 must catch, and the lookalikes it must not. This file is
/// excluded from G3, so the paths can be spelled here.
#[test]
fn the_clock_rule_catches_every_path_form_and_no_other() {
    let caught = [
        ("let t = std::time::Instant::now();", 1),
        ("let t = std::time::SystemTime::now();", 1),
        ("let t = time::Instant::now();", 1),
        ("let t = Instant::now();", 1),
        ("let t = SystemTime::now;", 1),
        ("let t = Instant::now\n    ();", 1),
        ("let t = std::time::Instant\n    ::now();", 2),
        ("let t = std::time::Instant::\n    now();", 2),
        ("use std::time::Instant as Clock;\nlet t = Clock::now();", 2),
        ("use std::time::SystemTime as Wall;\nlet t = Wall::now;", 2),
        (
            "use std::time::{\n    Instant as Clock,\n};\nlet t = Clock::now();",
            4,
        ),
        ("use std::time::Instant::now;\nlet t = now();", 1),
        ("type Clock = std::time::Instant;\nlet t = Clock::now();", 2),
        ("type Wall = SystemTime;\nlet t = Wall::now();", 2),
        ("type Counter = u64;\nlet t = Counter::now();", 0),
        ("let t = m6_core::platform::now();", 0),
        ("let n = m6_core::testkit::alloc_count::now();", 0),
        ("let t = chrono::Utc::now();", 1),
        ("let t = chrono::Local::now();", 1),
        ("let t = clock.now();", 0),
        ("let t = now();", 0),
        (
            "fn now() -> std::time::Instant { std::time::Instant::UNIX_EPOCH }",
            0,
        ),
        ("// Instant::now() belongs in m6-platform/src", 0),
        ("let t = MyDomainClock::now();", 0),
    ];
    for (text, line) in caught {
        let expected: Vec<usize> = if line == 0 { Vec::new() } else { vec![line] };
        assert_eq!(
            clock_path_lines(text),
            expected,
            "line numbers for {text:?}"
        );
    }
}

/// G5. No read of a systemd-only variable anywhere outside the layer: m6
/// never talks to a service manager and never relies on one to create
/// anything, so the same binary runs under launchd. Break: one
/// `std::env::var("NOTIFY_SOCKET")` in a service.
#[test]
fn no_service_manager_variable_outside_the_layer() {
    const VARIABLES: [&str; 8] = [
        "CREDENTIALS_DIRECTORY",
        "STATE_DIRECTORY",
        "RUNTIME_DIRECTORY",
        "NOTIFY_SOCKET",
        "LISTEN_FDS",
        "WATCHDOG_USEC",
        "INVOCATION_ID",
        "JOURNAL_STREAM",
    ];
    let root = root();
    let mut found = Vec::new();
    for rel in rust_files(&root) {
        if in_layer(&rel) {
            continue;
        }
        found.extend(offending_lines(&root, &rel, &|line| {
            VARIABLES.iter().any(|v| line.contains(v))
        }));
    }
    assert_none("G5 (service manager variables)", found);
}

/// Whether `line` names the crate `c` as a path (`c::`), a `use` or an
/// `extern crate`. A path check at a word boundary, so `std::os::unix::` does
/// not read as `nix::`.
fn names_crate(line: &str, c: &str) -> bool {
    let path = format!("{c}::");
    let as_path = line.match_indices(&path).any(|(i, _)| {
        !line[..i]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_' || ch == ':')
    });
    let t = line.trim_start();
    as_path
        || t.starts_with(&format!("use {c};"))
        || t.starts_with(&format!("use {c} "))
        || t.starts_with(&format!("extern crate {c}"))
}

/// G4. The two arms define the same items, so a macOS-only run sees a
/// missing Linux arm and the reverse. Break: a `pub(super) fn` added to one
/// arm only.
#[test]
fn the_two_arms_define_the_same_items() {
    fn items(root: &Path, rel: &str) -> Vec<String> {
        let text = std::fs::read_to_string(root.join(rel)).expect("read the arm");
        let mut out: Vec<String> = text
            .lines()
            .filter_map(|l| {
                let l = l.trim_start().strip_prefix("pub(super) ")?;
                let rest = l
                    .strip_prefix("fn ")
                    .or_else(|| l.strip_prefix("struct "))
                    .or_else(|| l.strip_prefix("const "))?;
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                Some(name)
            })
            .collect();
        out.sort();
        out
    }
    let root = root();
    let linux = items(&root, "m6-platform/src/linux.rs");
    let macos = items(&root, "m6-platform/src/macos.rs");
    assert!(!linux.is_empty() && !macos.is_empty());
    assert_eq!(
        linux, macos,
        "the Linux and macOS arms must define the same items"
    );
}
