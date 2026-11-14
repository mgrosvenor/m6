//! Is this path parameter safe to use?
//!
//! **The single implementation.** There were three and they disagreed on real
//! input: one rejected a leading slash and excluded `.`, one allowed `a..b`
//! inside a component, and one returned `Ok` with no character validation at
//! all for any parameter that happened to be named `relpath`.
//!
//! Path traversal safety is not something to hold three opinions about.
//! Allowed: alphanumeric, `-`, `_`, `.`, and `/` when the caller says the
//! value spans segments. Rejected: `..` anywhere as a substring, a leading or
//! trailing slash, and every other byte including space, control characters
//! and NUL.
//!
//! Whether slashes are allowed is decided by the **route**, not by the
//! parameter's name: a `{*name}` wildcard capture spans segments and an
//! ordinary `{name}` does not.
//!
//! The module also answers where a site-relative path points once every
//! symlink in it is followed, so the request file helpers and `m6-file` cannot
//! disagree about whether a path leaves the site.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum PathParamError {
    #[error("path traversal attempt")]
    Traversal,
    #[error("invalid characters in path parameter")]
    InvalidChars,
}

/// Validate a path parameter value.
///
/// **The single implementation.** There were three, and they disagreed on real
/// input. `m6-core`'s rejected a leading slash and excluded `.`;
/// `m6-file::route::is_safe_param` allowed `.` and permitted `a..b` inside a
/// component; `m6-render::request::validate_path_param` returned `Ok` with no
/// character validation at all for a parameter named `relpath`, accepting
/// spaces, control characters and NUL. Path traversal safety is not something
/// to hold three opinions about, so this is now the only one.
///
/// Allowed characters: alphanumeric, `-`, `_`, `.`.
/// If `allow_slash` is true, `/` is also permitted.
///
/// `.` is permitted because both real callers need it (`style.css`,
/// `hello-world.html`) and `..` is rejected independently, so allowing the
/// single dot costs nothing.
///
/// Rejects:
///   - `..` anywhere, as a substring rather than per component. Stricter than
///     the `m6-file` version it replaces, which allowed `a..b`.
///   - Leading or trailing `/` when `allow_slash` is set. An empty leading
///     component used to pass the character check vacuously.
///   - Every other byte, including space, control characters and NUL.
///
/// Returns the value unchanged if valid, `Err` if not.
pub fn validate_path_param(value: &str, allow_slash: bool) -> Result<&str, PathParamError> {
    if value.is_empty() {
        return Ok(value);
    }

    // Reject traversal sequences.
    if value.contains("..") {
        return Err(PathParamError::Traversal);
    }

    if allow_slash {
        // Reject leading or trailing slash.
        if value.starts_with('/') || value.ends_with('/') {
            return Err(PathParamError::InvalidChars);
        }
        // Allow alphanumeric, '-', '_', '.', '/'.
        for ch in value.chars() {
            if !ch.is_alphanumeric() && ch != '-' && ch != '_' && ch != '.' && ch != '/' {
                return Err(PathParamError::InvalidChars);
            }
        }
    } else {
        // Allow alphanumeric, '-', '_', '.'.
        for ch in value.chars() {
            if !ch.is_alphanumeric() && ch != '-' && ch != '_' && ch != '.' {
                return Err(PathParamError::InvalidChars);
            }
        }
    }

    Ok(value)
}

/// Why [`resolve_within`] refused a path.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolvePathError {
    #[error("path traversal attempt")]
    Traversal,
    #[error("path escapes the site directory")]
    Escape,
}

/// Resolve `rel` under `root` with every symlink followed, and refuse it
/// unless the result is inside `root`.
///
/// **The single implementation** for the site-relative file helpers and for
/// `m6-file`. A lexical check cannot see a symlink at all, and checking only
/// the last component misses a symlinked directory in the middle of the path.
/// Both defects came from checking the string the request supplied instead of
/// the path it resolves to.
///
/// `root` is canonicalised, so a site deployed through a symlinked directory
/// still resolves. A path that does not exist yet cannot be canonicalised
/// whole: its deepest existing ancestor is canonicalised, the components that
/// do not exist are appended, and the result is checked. An appended name
/// cannot be a symlink because its parent does not exist. A dangling symlink
/// counts as existing and is refused rather than treated as a new name, since
/// a write through it would create the target outside the root.
///
/// A symlink that stays inside `root` resolves to a path that still starts
/// with the canonical root and is accepted. That is how a site that links
/// `assets` to `static/assets` keeps working.
///
/// `..`, a leading `/` and a Windows prefix are refused before any filesystem
/// call.
pub fn resolve_within(root: &Path, rel: &Path) -> Result<PathBuf, ResolvePathError> {
    let root = root.canonicalize().map_err(|_| ResolvePathError::Escape)?;

    let mut joined = root.clone();
    for comp in rel.components() {
        match comp {
            Component::Normal(name) => joined.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ResolvePathError::Traversal);
            }
        }
    }

    // Walk down to the deepest component that exists. `symlink_metadata` does
    // not follow the final component, so a dangling symlink stops the walk and
    // is then refused by the canonicalise below.
    let mut probe = joined.as_path();
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    let existing = loop {
        if probe.symlink_metadata().is_ok() {
            break probe;
        }
        let Some(name) = probe.file_name() else {
            return Err(ResolvePathError::Escape);
        };
        missing.push(name);
        probe = probe.parent().ok_or(ResolvePathError::Escape)?;
    };

    let mut resolved = existing
        .canonicalize()
        .map_err(|_| ResolvePathError::Escape)?;
    for name in missing.iter().rev() {
        resolved.push(name);
    }

    if !resolved.starts_with(&root) {
        return Err(ResolvePathError::Escape);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The divergence table from CORE-DUPLICATION-AUDIT.md section 1, pinned.
    ///
    /// Three implementations disagreed on these exact inputs. Each row is a
    /// case where at least one of them said yes and another said no. This test
    /// is what makes the reconciliation a decision rather than an accident.
    #[test]
    fn the_three_implementations_are_reconciled() {
        // (value, allow_slash, expect_ok, which impl used to disagree)
        let cases: &[(&str, bool, bool, &str)] = &[
            ("hello-world", false, true, "all agreed"),
            ("style.css", false, true, "m6-core used to reject the dot"),
            ("a/b/c", true, true, "all agreed"),
            (
                "/leading",
                true,
                false,
                "m6-render and m6-file used to allow it",
            ),
            (
                "trailing/",
                true,
                false,
                "m6-render and m6-file used to allow it",
            ),
            (
                "a b",
                false,
                false,
                "m6-render relpath used to allow a space",
            ),
            (
                "a\u{0}b",
                false,
                false,
                "m6-render relpath used to allow NUL",
            ),
            (
                "a\nb",
                false,
                false,
                "m6-render relpath used to allow a newline",
            ),
            ("..", false, false, "all agreed"),
            ("../etc/passwd", true, false, "all agreed"),
            ("a..b", true, false, "m6-file allowed it inside a component"),
            ("a/../b", true, false, "all agreed"),
            (
                "%2e%2e",
                false,
                false,
                "not decoded here, and % is not allowed",
            ),
            ("a/b", false, false, "slash needs allow_slash"),
        ];
        for (value, allow_slash, expect_ok, why) in cases {
            let got = validate_path_param(value, *allow_slash).is_ok();
            assert_eq!(
                got, *expect_ok,
                "validate_path_param({value:?}, {allow_slash}) -> {got}, wanted {expect_ok} ({why})"
            );
        }
    }

    /// A parameter that cannot contain a slash must still be validated.
    ///
    /// This is the hole that existed in m6-render: a name-based special case
    /// returned Ok without looking at the characters, for a parameter its own
    /// router could never populate with a slash in the first place.
    #[test]
    fn a_slashless_param_is_still_character_checked() {
        for bad in ["a b", "a\u{7f}b", "a;b", "a|b", "a$b", "a\u{0}"] {
            assert!(
                validate_path_param(bad, false).is_err(),
                "{bad:?} must be rejected even without allow_slash"
            );
        }
    }

    #[test]
    fn test_validate_path_param_valid_simple() {
        assert_eq!(
            validate_path_param("hello-world_123", false).unwrap(),
            "hello-world_123"
        );
    }

    #[test]
    fn test_validate_path_param_rejects_traversal() {
        assert!(matches!(
            validate_path_param("../etc/passwd", false),
            Err(PathParamError::Traversal)
        ));
        assert!(matches!(
            validate_path_param("foo/../bar", true),
            Err(PathParamError::Traversal)
        ));
    }

    #[test]
    fn test_validate_path_param_rejects_leading_slash() {
        assert!(matches!(
            validate_path_param("/foo/bar", true),
            Err(PathParamError::InvalidChars)
        ));
    }

    #[test]
    fn test_validate_path_param_rejects_trailing_slash() {
        assert!(matches!(
            validate_path_param("foo/bar/", true),
            Err(PathParamError::InvalidChars)
        ));
    }

    #[test]
    fn test_validate_path_param_with_slash_allowed() {
        assert_eq!(
            validate_path_param("foo/bar/baz.txt", true).unwrap(),
            "foo/bar/baz.txt"
        );
    }

    #[test]
    fn test_validate_path_param_slash_disallowed() {
        assert!(matches!(
            validate_path_param("foo/bar", false),
            Err(PathParamError::InvalidChars)
        ));
    }
}
