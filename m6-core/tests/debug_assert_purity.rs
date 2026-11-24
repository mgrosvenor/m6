//! No `debug_assert!` argument may change state.
//!
//! A release build compiles a `debug_assert!`'s arguments out. A call that
//! writes, pushes or sends inside one therefore runs in a test build and
//! vanishes in the binary that ships. HTTP/2's frame writer did exactly that:
//! every frame was written by a `put` call inside a debug assertion, so the
//! release build of m6-http sent no HTTP/2 frame at all while every test in
//! the dev profile passed.
//!
//! This test scans every Rust source in the workspace, takes the whole
//! argument list of each `debug_assert!`, `debug_assert_eq!` and
//! `debug_assert_ne!` (across lines, by balanced parentheses) and fails when
//! it calls a method from [`MUTATORS`]. The release-profile test run in CI is
//! the second guard, for whatever this list misses.

use std::path::{Path, PathBuf};

/// Method names that change their receiver or the world.
const MUTATORS: &[&str] = &[
    "put",
    "push",
    "push_back",
    "push_front",
    "extend",
    "extend_from_slice",
    "insert",
    "remove",
    "write",
    "write_all",
    "send",
    "pop",
    "pop_front",
    "pop_back",
    "drain",
    "clear",
    "append",
    "truncate",
    "take",
    "set",
];

/// The argument text of every debug assertion in `src`, with its line.
fn debug_assert_args(src: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut rest = src;
    let mut offset = 0;
    while let Some(at) = rest.find("debug_assert") {
        let start = offset + at;
        let tail = &src[start..];
        let Some(open) = tail.find('(') else { break };
        let name = &tail[..open];
        let step = start + "debug_assert".len();
        if matches!(
            name,
            "debug_assert!" | "debug_assert_eq!" | "debug_assert_ne!"
        ) {
            let mut depth = 0usize;
            let mut end = None;
            for (i, ch) in tail[open..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(open + i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(end) = end {
                let line = src[..start].matches('\n').count() + 1;
                found.push((line, tail[open + 1..end].to_string()));
            }
        }
        offset = step;
        rest = &src[step..];
    }
    found
}

/// The first mutating method call in `args`, if any.
fn mutating_call(args: &str) -> Option<&'static str> {
    MUTATORS.iter().copied().find(|m| {
        let needle = format!(".{m}(");
        args.contains(&needle) || args.contains(&format!(".{m}::<"))
    })
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == "target" || name.to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_debug_assert_argument_mutates() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("m6-core sits in the workspace");
    let mut files = Vec::new();
    rust_files(root, &mut files);
    assert!(
        files.len() > 50,
        "the scan found only {} sources under {}",
        files.len(),
        root.display()
    );
    let mut bad = Vec::new();
    for file in &files {
        let src = std::fs::read_to_string(file).expect("read a source");
        for (line, args) in debug_assert_args(&src) {
            if let Some(call) = mutating_call(&args) {
                bad.push(format!("{}:{line}: .{call}(..)", file.display()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a debug assertion's arguments vanish in a release build, so these calls never run there:\n{}",
        bad.join("\n")
    );
}

/// The scanner itself: the defect it exists for is caught, across lines, and
/// a pure comparison is not.
#[test]
fn the_scanner_catches_a_write_inside_a_debug_assert() {
    let bad = concat!(
        "fn f() {\n    debug_",
        "assert!(\n        buf.put(&head),\n        \"fits\"\n    );\n}\n"
    );
    let found = debug_assert_args(bad);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, 2);
    assert_eq!(mutating_call(&found[0].1), Some("put"));

    let eq = concat!("debug_", "assert_eq!(q.pop_front(), Some(1));");
    assert_eq!(
        mutating_call(&debug_assert_args(eq)[0].1),
        Some("pop_front")
    );

    let pure = concat!("debug_", "assert!(len < MAX, \"overflow (by one)\");");
    assert_eq!(mutating_call(&debug_assert_args(pure)[0].1), None);
}
