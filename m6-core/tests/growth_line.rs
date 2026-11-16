//! The one growth log line, asserted on the bytes the logger wrote.
//!
//! The line I-217's addendum requires names the page and the old size as well
//! as the new one. A single test in its own binary, because it replaces
//! standard output for the process.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use m6_core::limits::{Bounded, Limits, RENDER_PAGE};

#[test]
fn the_growth_line_names_the_page_and_the_old_size() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("operational.jsonl");
    let file = File::options()
        .create(true)
        .append(true)
        .open(&path)
        .expect("the log file");

    let saved = m6_core::platform::fd::dup(&std::io::stdout()).expect("a copy of stdout");
    m6_core::platform::fd::dup2_stdout(&file).expect("stdout is the file");
    m6_core::log::init(m6_core::log::Level::Warn, None).expect("the log");

    let limits = Limits::from_toml(
        Some(
            &toml::from_str::<toml::Value>(
                "[limits.render_page]\nstart = \"512KiB\"\nmax = \"4MiB\"\n",
            )
            .expect("the limits table")
            .get("limits")
            .cloned()
            .expect("the table"),
        ),
        Path::new("site.toml"),
        4096,
    )
    .expect("the limits");
    let limit = limits.get(&RENDER_PAGE);
    assert_eq!((limit.start(), limit.max()), (512 * 1024, 4 * 1024 * 1024));

    let mut area = Bounded::<u8>::new(limit);
    // A page path, not a constant: the line has to name what grew.
    area.set_what("page /blog/x");
    area.write_all(&vec![b'x'; 600 * 1024])
        .expect("a 600 KiB page fits 4 MiB");
    assert_eq!(area.capacity(), 1024 * 1024, "one growth to 1 MiB");

    m6_core::log::stop().expect("the log stops");
    m6_core::platform::fd::dup2_stdout(&saved).expect("stdout is restored");

    let text = std::fs::read_to_string(&path).expect("the written line");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one line per growth: {text}");
    let line: serde_json::Value = serde_json::from_str(lines[0]).expect("one JSON line");
    assert_eq!(line["level"], "WARN");
    let fields = &line["fields"];
    assert_eq!(
        fields["message"],
        "limit render_page grew from 512 KiB to 1 MiB for page /blog/x (max 4 MiB, [limits.render_page] in site.toml)"
    );
    assert_eq!(fields["limit"], "render_page");
    assert_eq!(fields["from"], "512 KiB");
    assert_eq!(fields["to"], "1 MiB");
    assert_eq!(fields["what"], "page /blog/x");
    assert_eq!(fields["max"], "4 MiB");
    assert_eq!(fields["key"], "[limits.render_page]");
    assert_eq!(fields["file"], "site.toml");
    assert!(
        text.contains("grew from 512 KiB to 1 MiB for page /blog/x"),
        "the old and new sizes and the page are in the actual text: {text}"
    );
}
