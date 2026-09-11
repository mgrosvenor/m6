/// Utility functions exposed via the prelude.

/// Slugify a string: "Hello World" → "hello-world"
pub fn slugify(s: &str) -> String {
    slug::slugify(s)
}

/// Current date in ISO 8601 format: "2024-01-15"
pub fn today_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Current datetime in ISO 8601 format: "2024-01-15T10:30:00Z"
pub fn now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("Rust is great!"), "rust-is-great");
    }

    #[test]
    fn test_dates_not_empty() {
        assert!(!today_iso8601().is_empty());
        assert!(!now_iso8601().is_empty());
    }
}
