//! The design rule: m6-http decodes no token and links no JWT library.
//!
//! The edge asks `POST /check` for every credential. A local verifier in this
//! crate would be a second answer to the same question, and it would drift
//! from the authority's. This gate reads the manifest and the crate's source,
//! so a JWT library creeping back in fails a test rather than a review.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("m6-http sits in the workspace")
        .to_path_buf()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() != "target" && entry.file_name() != ".git" {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `m6-http` names no JWT library, in its manifest or in any source file.
#[test]
fn m6_http_does_not_link_jsonwebtoken() {
    let root = root();
    let manifest = std::fs::read_to_string(root.join("m6-http/Cargo.toml")).expect("manifest");
    assert!(
        !manifest.contains("jsonwebtoken"),
        "m6-http/Cargo.toml names jsonwebtoken; the edge asks /check and decodes no token"
    );

    let mut files = Vec::new();
    for sub in ["m6-http/src", "m6-http/tests", "m6-http/benches"] {
        let dir = root.join(sub);
        if dir.is_dir() {
            rust_files(&dir, &mut files);
        }
    }
    for path in files {
        if path.ends_with("m6-http/tests/dependency_gate.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read the source");
        assert!(
            !text.contains("jsonwebtoken"),
            "{} names jsonwebtoken; the edge asks /check and decodes no token",
            path.display()
        );
    }
}

/// A JWT decode lives in the authority, not in a service that asks it.
#[test]
fn a_jwt_decode_lives_in_the_authority() {
    let root = root();
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let allowed = |rel: &Path| {
        rel.starts_with("m6-auth")
            || rel == Path::new("m6-core/src/auth.rs")
            || rel.starts_with("m6-auth-server")
    };
    for path in files {
        let rel = path.strip_prefix(&root).expect("under the root");
        if allowed(rel) || rel == Path::new("m6-http/tests/dependency_gate.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read the source");
        assert!(
            !text.contains("jsonwebtoken::decode"),
            "{} decodes a JWT outside m6-auth, m6-auth-server and m6-core::auth",
            rel.display()
        );
    }
}
