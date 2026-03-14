/// MIME type utilities for m6.

use std::path::Path;

/// Detect MIME type from the file extension of `path`.
/// Returns `"application/octet-stream"` for unknown extensions.
pub fn mime_from_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Return a reference to a static str without heap allocation.
    match ext.as_str() {
        // Text
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "json" => "application/json",
        "jsonld" => "application/ld+json",
        "md" => "text/markdown; charset=utf-8",

        // Images
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",

        // Fonts
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",

        // Audio / Video
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "ogg" => "audio/ogg",
        "ogv" => "video/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",

        // Archives / Binary
        "zip" => "application/zip",
        "gz" | "gzip" => "application/gzip",
        "br" => "application/x-brotli",
        "zst" => "application/zstd",
        "tar" => "application/x-tar",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",

        // Data / Feeds
        "atom" => "application/atom+xml",
        "rss" => "application/rss+xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",

        // Fallback
        _ => "application/octet-stream",
    }
}

/// Returns `true` if this MIME type should be compressed by default.
///
/// Compressible: text formats, JSON, XML, JavaScript, SVG, WASM, fonts.
pub fn should_compress_default(mime: &str) -> bool {
    // Strip any parameters (e.g. "; charset=utf-8").
    let base = mime.split(';').next().unwrap_or(mime).trim();

    matches!(
        base,
        "text/html"
            | "text/css"
            | "text/javascript"
            | "text/plain"
            | "text/csv"
            | "text/markdown"
            | "text/xml"
            | "application/json"
            | "application/ld+json"
            | "application/xml"
            | "application/javascript"
            | "application/atom+xml"
            | "application/rss+xml"
            | "application/yaml"
            | "application/toml"
            | "application/wasm"
            | "image/svg+xml"
            | "font/ttf"
            | "font/otf"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mime_from_path_html() {
        assert!(mime_from_path(Path::new("index.html")).starts_with("text/html"));
    }

    #[test]
    fn test_mime_from_path_unknown() {
        assert_eq!(mime_from_path(Path::new("file.xyz")), "application/octet-stream");
    }

    #[test]
    fn test_mime_from_path_no_extension() {
        assert_eq!(mime_from_path(Path::new("Makefile")), "application/octet-stream");
    }

    #[test]
    fn test_should_compress_text_html() {
        assert!(should_compress_default("text/html; charset=utf-8"));
        assert!(should_compress_default("application/json"));
        assert!(should_compress_default("image/svg+xml"));
    }

    #[test]
    fn test_should_not_compress_binary() {
        assert!(!should_compress_default("image/png"));
        assert!(!should_compress_default("image/jpeg"));
        assert!(!should_compress_default("application/octet-stream"));
        assert!(!should_compress_default("video/mp4"));
    }
}
