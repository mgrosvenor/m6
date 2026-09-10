/// Path validation and resolution utilities.

use std::path::{Path, PathBuf};

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

/// Resolve `suffix` within `root`, checking that the result is inside `root`.
///
/// Returns `Some(path)` if the resolved path is within `root`, `None` on traversal.
///
/// If the path does not exist on disk the function still works by joining and
/// doing a lexical check (no `canonicalize` which requires existence).
pub fn safe_resolve(root: &Path, suffix: &Path) -> Option<PathBuf> {
    // Reject suffix that is absolute.
    if suffix.is_absolute() {
        return None;
    }

    // Build candidate path.
    let candidate = root.join(suffix);

    // Normalize by resolving . and .. components without requiring existence.
    let normalized = normalize_path(&candidate);
    let normalized_root = normalize_path(root);

    // Check that the normalized path starts with the normalized root.
    if normalized.starts_with(&normalized_root) {
        Some(normalized)
    } else {
        None
    }
}

/// Normalize a path by resolving `.` and `..` lexically (no filesystem access).
fn normalize_path(path: &Path) -> PathBuf {
    let mut components: Vec<std::path::Component> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Pop the last component only if it's a normal segment.
                match components.last() {
                    Some(std::path::Component::Normal(_)) => {
                        components.pop();
                    }
                    _ => {
                        components.push(component);
                    }
                }
            }
            other => components.push(other),
        }
    }
    components.iter().collect()
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
            ("/leading", true, false, "m6-render and m6-file used to allow it"),
            ("trailing/", true, false, "m6-render and m6-file used to allow it"),
            ("a b", false, false, "m6-render relpath used to allow a space"),
            ("a\u{0}b", false, false, "m6-render relpath used to allow NUL"),
            ("a\nb", false, false, "m6-render relpath used to allow a newline"),
            ("..", false, false, "all agreed"),
            ("../etc/passwd", true, false, "all agreed"),
            ("a..b", true, false, "m6-file allowed it inside a component"),
            ("a/../b", true, false, "all agreed"),
            ("%2e%2e", false, false, "not decoded here, and % is not allowed"),
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
        assert_eq!(validate_path_param("hello-world_123", false).unwrap(), "hello-world_123");
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

    #[test]
    fn test_safe_resolve_within_root() {
        let root = Path::new("/var/www");
        let result = safe_resolve(root, Path::new("pages/index.html"));
        assert_eq!(result, Some(PathBuf::from("/var/www/pages/index.html")));
    }

    #[test]
    fn test_safe_resolve_traversal_returns_none() {
        let root = Path::new("/var/www");
        assert!(safe_resolve(root, Path::new("../../etc/passwd")).is_none());
    }

    #[test]
    fn test_safe_resolve_absolute_suffix_returns_none() {
        let root = Path::new("/var/www");
        assert!(safe_resolve(root, Path::new("/etc/passwd")).is_none());
    }

    #[test]
    fn test_safe_resolve_dot_inside_stays_in_root() {
        let root = Path::new("/var/www");
        let result = safe_resolve(root, Path::new("pages/./index.html"));
        assert_eq!(result, Some(PathBuf::from("/var/www/pages/index.html")));
    }
}
