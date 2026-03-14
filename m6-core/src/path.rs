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
/// Allowed characters: alphanumeric, `-`, `_`.
/// If `allow_slash` is true, `.` and `/` are also permitted.
///
/// Rejects:
///   - `..` sequences
///   - Leading or trailing `/`
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
        // Allow alphanumeric, '-', '_' only.
        for ch in value.chars() {
            if !ch.is_alphanumeric() && ch != '-' && ch != '_' {
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
