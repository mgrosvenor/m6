//! Every config field that holds a credential is declared `Secret<T>`.
//!
//! The start-up record writes every config field it is given. A field typed
//! [`m6_core::Secret`] serialises as `<redacted>`, so the record cannot print
//! it, and a field typed `String` is printed. This test reads every config
//! struct in the workspace from its source and fails on a field whose name
//! [`m6_core::startup::names_a_secret`] calls a credential and whose type is
//! not `Secret`. The rule for an untyped value and the rule for a typed field
//! are one function, so the two cannot drift.
//!
//! A config struct is one whose name ends in `Config`, `Toml` or `Section`, or
//! any struct in a file named `config.rs`. Three kinds of field are exempt,
//! each by its type: a path (`PathBuf` or `Path`), which says where a secret is
//! and is logged on purpose, a nested config table such as
//! `tokens: Option<TokensConfig>`, and a field already typed `Secret`.

use std::path::{Path, PathBuf};

use m6_core::startup::names_a_secret;
use syn::visit::Visit;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("m6-core sits in the workspace")
        .to_path_buf()
}

/// Every `.rs` file under each workspace member's `src`.
fn sources() -> Vec<PathBuf> {
    let root = workspace_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("the root manifest");
    let manifest: toml::Value = toml::from_str(&manifest).expect("the root manifest parses");
    let members = manifest["workspace"]["members"]
        .as_array()
        .expect("the workspace lists its members");
    let mut out = Vec::new();
    for member in members {
        let src = root
            .join(member.as_str().expect("a member is a path"))
            .join("src");
        collect(&src, &mut out);
    }
    assert!(out.len() > 50, "the walk found only {} files", out.len());
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// What a field's type says about it.
#[derive(Debug, PartialEq)]
enum Kind {
    Secret,
    Path,
    Table,
    Plain,
}

fn is_config_name(name: &str) -> bool {
    name.ends_with("Config") || name.ends_with("Toml") || name.ends_with("Section")
}

/// Classify a type by every path segment it names, so `Option<Secret<String>>`
/// and `Vec<PathBuf>` are read through their wrappers.
fn kind(ty: &syn::Type) -> Kind {
    #[derive(Default)]
    struct Names(Vec<String>);
    impl<'ast> Visit<'ast> for Names {
        fn visit_path_segment(&mut self, segment: &'ast syn::PathSegment) {
            self.0.push(segment.ident.to_string());
            syn::visit::visit_path_segment(self, segment);
        }
    }
    let mut names = Names::default();
    names.visit_type(ty);
    if names.0.iter().any(|n| n == "Secret") {
        Kind::Secret
    } else if names.0.iter().any(|n| n == "PathBuf" || n == "Path") {
        Kind::Path
    } else if names.0.iter().any(|n| is_config_name(n)) {
        Kind::Table
    } else {
        Kind::Plain
    }
}

/// The key a field is read from: its `serde(rename)` when it has one, and its
/// name otherwise. Both are checked, because either can be the one a reader
/// sees.
fn renamed(field: &syn::Field) -> Option<String> {
    let mut out = None;
    for attr in &field.attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let value: syn::LitStr = meta.value()?.parse()?;
                out = Some(value.value());
            } else if meta.input.peek(syn::Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            }
            Ok(())
        });
    }
    out
}

/// One field the rule reads, and what the walk found.
#[derive(Debug)]
struct Found {
    file: PathBuf,
    owner: String,
    field: String,
    kind: Kind,
}

/// Every credential-named field of every config struct in `text`.
fn credential_fields(file: &Path, text: &str) -> (usize, Vec<Found>) {
    let parsed = syn::parse_file(text).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    let in_config_file = file.file_name().is_some_and(|n| n == "config.rs");
    #[derive(Default)]
    struct Structs<'a>(Vec<&'a syn::ItemStruct>);
    impl<'ast> Visit<'ast> for Structs<'ast> {
        fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
            self.0.push(item);
        }
    }
    let mut structs = Structs::default();
    structs.visit_file(&parsed);
    let mut configs = 0;
    let mut out = Vec::new();
    for item in structs.0 {
        let owner = item.ident.to_string();
        if !(in_config_file || is_config_name(&owner)) {
            continue;
        }
        configs += 1;
        for field in &item.fields {
            let Some(ident) = &field.ident else { continue };
            let name = ident.to_string();
            let named =
                names_a_secret(&name) || renamed(field).is_some_and(|key| names_a_secret(&key));
            if named {
                out.push(Found {
                    file: file.to_path_buf(),
                    owner: owner.clone(),
                    field: name,
                    kind: kind(&field.ty),
                });
            }
        }
    }
    (configs, out)
}

#[test]
fn every_credential_field_in_a_config_struct_is_a_secret() {
    let root = workspace_root();
    let mut configs = 0;
    let mut found = Vec::new();
    for file in sources() {
        let text = std::fs::read_to_string(&file).expect("a source file is readable");
        let (n, fields) = credential_fields(&file, &text);
        configs += n;
        found.extend(fields);
    }
    // A walk that finds no config struct proves nothing. The workspace has
    // more than thirty.
    assert!(configs > 30, "the walk found only {configs} config structs");

    let plain: Vec<String> = found
        .iter()
        .filter(|f| f.kind == Kind::Plain)
        .map(|f| {
            format!(
                "{}: {}.{} names a credential and is not Secret<T>",
                f.file.strip_prefix(&root).unwrap_or(&f.file).display(),
                f.owner,
                f.field
            )
        })
        .collect();
    assert!(
        plain.is_empty(),
        "declare each of these m6_core::Secret<T>, or a PathBuf when it holds a path:\n{}",
        plain.join("\n")
    );

    // The typed secret the workspace still has is found, so the walk reaches
    // the crates and reads through `Option`.
    for (owner, field) in [("RelayConfig", "credentials")] {
        assert!(
            found
                .iter()
                .any(|f| f.owner == owner && f.field == field && f.kind == Kind::Secret),
            "{owner}.{field} was not found as a Secret: {found:?}"
        );
    }
}

/// The rule fails a plain credential field, and passes each exempt kind.
#[test]
fn the_walk_fails_a_plain_credential_and_passes_the_exempt_kinds() {
    let text = r#"
        struct MailConfig {
            password: String,
            api_token: Option<String>,
            #[serde(rename = "secret")]
            shared: String,
            relay_password: Option<m6_core::Secret<String>>,
            private_key: Option<PathBuf>,
            token_file: String,
            public_key: String,
            tokens: Option<TokensConfig>,
            host: String,
        }
        struct NotConfigured {
            password: String,
        }
    "#;
    let (configs, found) = credential_fields(Path::new("example.rs"), text);
    assert_eq!(configs, 1, "only the config struct is read");
    let kinds: Vec<(&str, &Kind)> = found.iter().map(|f| (f.field.as_str(), &f.kind)).collect();
    assert_eq!(
        kinds,
        [
            ("password", &Kind::Plain),
            ("api_token", &Kind::Plain),
            ("shared", &Kind::Plain),
            ("relay_password", &Kind::Secret),
            ("private_key", &Kind::Path),
            ("tokens", &Kind::Table),
        ],
        "token_file, public_key and host are not credential names"
    );
}

/// The name rule the walk and the record share.
#[test]
fn the_name_rule_names_credentials_and_not_their_paths() {
    for key in [
        "password",
        "smtp_password",
        "SMTP-Password",
        "flash_secret",
        "api_token",
        "credentials",
        "private_key",
        "privateKey",
    ] {
        assert!(names_a_secret(key), "{key} is a credential");
    }
    for key in [
        "secrets_file",
        "token_file",
        "private_key_path",
        "password_file",
        "public_key",
        "host",
        "username",
        "tls_key",
    ] {
        assert!(!names_a_secret(key), "{key} is not a credential");
    }
}
