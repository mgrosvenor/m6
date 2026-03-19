/// Early-hints extraction: scan an HTML response body for cacheable assets
/// that the browser should preload.
///
/// Called **only** on the cache-miss path, never on a cache hit.
/// Results are stored in `CachedResponse.hints` and reused on subsequent hits.

/// Return the `as=` attribute value for a URL based on its extension.
fn preload_as(url: &str) -> &'static str {
    if url.ends_with(".css")                          { "style"  }
    else if url.ends_with(".js")                      { "script" }
    else if url.ends_with(".woff2") || url.ends_with(".woff") { "font"   }
    else if url.ends_with(".png") || url.ends_with(".jpg")
         || url.ends_with(".jpeg") || url.ends_with(".webp")
         || url.ends_with(".gif")  || url.ends_with(".svg")  { "image"  }
    else                                              { ""       }
}

/// Scan `body` for asset `href` / `src` attributes and return absolute-path
/// URLs worth preloading.  Only URLs starting with `/` are returned (external
/// URLs are filtered out).  Only `.css`, `.js`, `.woff`, `.woff2`, `.png`,
/// `.jpg`, `.jpeg`, `.webp`, `.gif`, `.svg` extensions are hinted.
///
/// Returns an empty `Vec` if `content_type` is not `text/html`.
pub fn extract_hints(body: &[u8], content_type: &str) -> Vec<String> {
    if !content_type.contains("text/html") {
        return vec![];
    }

    // Avoid UTF-8 parsing for speed: work entirely on bytes. Only need ASCII
    // patterns (`href="`, `src="`, `href='`, `src='`).
    let mut hints: Vec<String> = Vec::new();

    for pattern in &[b"href=\"".as_ref(), b"src=\"".as_ref(),
                     b"href='".as_ref(),  b"src='".as_ref()] {
        let close = if pattern.ends_with(b"\"") { b'"' } else { b'\'' };
        let mut pos = 0usize;
        while pos < body.len() {
            // Find next occurrence of pattern.
            let Some(rel) = find_bytes(&body[pos..], pattern) else { break };
            let value_start = pos + rel + pattern.len();
            let rest = &body[value_start..];
            // Find closing quote.
            let Some(end) = rest.iter().position(|&b| b == close) else { break };
            let url_bytes = &rest[..end];
            pos = value_start + end + 1;

            // Only absolute paths.
            if url_bytes.first() != Some(&b'/') {
                continue;
            }
            // Must have a recognised extension worth hinting.
            let url_str = match std::str::from_utf8(url_bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Strip query string before checking extension so that
            // URLs like `/style.css?v=1` are recognised correctly.
            let clean = match url_str.find('?') {
                Some(q) => &url_str[..q],
                None    => url_str,
            };
            if preload_as(clean).is_empty() {
                continue;
            }
            hints.push(clean.to_string());
        }
    }

    hints.sort();
    hints.dedup();
    hints
}

/// Build the `Link:` header value for one hint URL.
/// e.g. `</assets/style.css>; rel=preload; as=style`
pub fn link_header(url: &str) -> String {
    let as_val = preload_as(url);
    if as_val.is_empty() {
        format!("<{url}>; rel=preload")
    } else {
        // Fonts also need crossorigin for CORS pre-flight.
        if as_val == "font" {
            format!("<{url}>; rel=preload; as={as_val}; crossorigin")
        } else {
            format!("<{url}>; rel=preload; as={as_val}")
        }
    }
}

// ── Byte-level substring search ───────────────────────────────────────────────

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() { return Some(0); }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_css_and_js() {
        let html = br#"<link href="/assets/style.css" rel="stylesheet">
<script src="/assets/app.js"></script>"#;
        let hints = extract_hints(html, "text/html; charset=utf-8");
        assert!(hints.contains(&"/assets/style.css".to_string()));
        assert!(hints.contains(&"/assets/app.js".to_string()));
    }

    #[test]
    fn test_no_hints_for_non_html() {
        let body = b"body { color: red; }";
        let hints = extract_hints(body, "text/css");
        assert!(hints.is_empty());
    }

    #[test]
    fn test_external_urls_excluded() {
        let html = br#"<link href="https://cdn.example.com/style.css">"#;
        let hints = extract_hints(html, "text/html");
        assert!(hints.is_empty());
    }

    #[test]
    fn test_query_string_stripped() {
        let html = br#"<link href="/assets/style.css?v=123" rel="stylesheet">"#;
        let hints = extract_hints(html, "text/html");
        assert_eq!(hints, vec!["/assets/style.css"]);
    }

    #[test]
    fn test_dedup() {
        let html = br#"<link href="/a.css"><link href="/a.css">"#;
        let hints = extract_hints(html, "text/html");
        assert_eq!(hints.len(), 1);
    }

    #[test]
    fn test_link_header_font_has_crossorigin() {
        let h = link_header("/fonts/inter.woff2");
        assert!(h.contains("crossorigin"));
        assert!(h.contains("as=font"));
    }
}
