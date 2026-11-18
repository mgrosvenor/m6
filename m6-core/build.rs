//! Reads mimalloc's option number for `purge_delay` from the header bundled with
//! `libmimalloc-sys`.
//!
//! The Rust binding exports only some `mi_option_*` constants and leaves out
//! `purge_delay`, and the number moves between mimalloc versions. A literal in
//! the source would mean something else after an upgrade without a word, so the
//! number is counted from the header the linked library was built from: the
//! position of `mi_option_purge_delay` in `enum mi_option_e`.
//!
//! It also emits m6-core's build identity, the constants `m6_core::build_info`
//! reads. m6-core cannot name itself as a build dependency, so the helper comes
//! from m6-platform, the crate below it.

use std::{env, fs, path::PathBuf};

fn main() {
    emit_mimalloc_purge_delay();
    m6_platform::build_info::emit();
}

fn emit_mimalloc_purge_delay() {
    let include = env::var("DEP_MIMALLOC_INCLUDE_DIR")
        .expect("libmimalloc-sys reports where its mimalloc.h is");
    let header = PathBuf::from(include).join("mimalloc.h");
    println!("cargo:rerun-if-changed={}", header.display());
    let text = fs::read_to_string(&header).expect("read the bundled mimalloc.h");
    let start = text
        .find("typedef enum mi_option_e {")
        .expect("mimalloc.h defines enum mi_option_e");
    let body = &text[start..];
    let body = &body[..body.find("} mi_option_t;").expect("the enum ends")];
    let mut index = 0usize;
    let mut found = None;
    for line in body.lines().skip(1) {
        let line = line.split("//").next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name = line.split([',', '=', ' ']).next().unwrap_or("");
        if name == "mi_option_purge_delay" {
            found = Some(index);
            break;
        }
        index += 1;
    }
    let number = found.expect("mi_option_purge_delay is in enum mi_option_e");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("purge_delay.in");
    fs::write(out, number.to_string()).expect("write purge_delay.in");
}
