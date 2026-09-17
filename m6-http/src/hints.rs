//! Early-hints extraction: scan an HTML response body for cacheable assets
//! that the browser should preload.
//!
//! Called **only** on the cache-miss path, never on a cache hit.
//! Results are stored in `CachedResponse.hints` and reused on subsequent hits.

/// Return the `as=` attribute value for a URL based on its extension.
///
/// The query string is not part of the extension. Asset URLs are cache-busted
/// (`/assets/css/style.css?v=ae331def`), so testing the raw URL answers "no
/// recognised extension" for every asset on a site that does that, and a
/// `Link: rel=preload` without an `as=` is ignored by browsers.
fn preload_as(url: &str) -> &'static str {
    let url = match url.find('?') {
        Some(q) => &url[..q],
        None => url,
    };
    if url.ends_with(".css") {
        "style"
    } else if url.ends_with(".js") {
        "script"
    } else if url.ends_with(".woff2") || url.ends_with(".woff") {
        "font"
    } else if url.ends_with(".png")
        || url.ends_with(".jpg")
        || url.ends_with(".jpeg")
        || url.ends_with(".webp")
        || url.ends_with(".gif")
        || url.ends_with(".svg")
    {
        "image"
    } else {
        ""
    }
}

/// Scan `body` for asset `href` / `src` attributes and return absolute-path
/// URLs worth preloading.  Only URLs starting with `/` are returned (external
/// URLs are filtered out).  Only `.css`, `.js`, `.woff`, `.woff2`, `.png`,
/// `.jpg`, `.jpeg`, `.webp`, `.gif`, `.svg` extensions are hinted.
///
/// Returns an empty `Vec` if `content_type` is not `text/html`.
///
/// # The value may be unquoted, and usually is
///
/// HTML5 allows an attribute value with no quotes, ending at whitespace or `>`,
/// and m6's own minifier emits exactly that: a minified page carries
/// `href=/assets/css/style.css?v=ae331def`, not `href="..."`. A parser that
/// requires quotes therefore finds nothing on any page this server minifies,
/// which is every page on a deployment with minification on. Both forms are
/// read here.
///
/// # The URL is emitted as written
///
/// What goes in the hint is what the page asked for, query string and all,
/// because m6 keys its cache on the full path including the query: a hint
/// naming `/assets/css/style.css` when the page requests
/// `/assets/css/style.css?v=ae331def` preloads an entry no page will ask for,
/// and the visitor still pays for the miss. `preload_as` ignores the query when
/// it reads the extension.
pub fn extract_hints(body: &[u8], content_type: &str) -> Vec<String> {
    if !content_type.contains("text/html") {
        return vec![];
    }

    // Bytes throughout, not UTF-8 parsing: the patterns are ASCII and this runs
    // on the cache-miss path for every HTML response.
    let mut hints: Vec<String> = Vec::new();

    for name in [b"href=".as_ref(), b"src=".as_ref()] {
        let mut pos = 0usize;
        while pos < body.len() {
            let Some(rel) = find_bytes(&body[pos..], name) else {
                break;
            };
            let at = pos + rel;
            pos = at + name.len();

            // An attribute name is preceded by whitespace. Without this,
            // `data-href=` and `xlink:href=` are read as `href=` and their
            // values hinted.
            if at > 0 && !body[at - 1].is_ascii_whitespace() {
                continue;
            }

            let rest = &body[pos..];
            let url_bytes = match rest.first() {
                None => break,
                // Quoted: the value runs to the matching quote. An unterminated
                // quote is malformed markup, so give up on this attribute
                // rather than reading to the end of the document.
                Some(&q) if q == b'"' || q == b'\'' => {
                    let Some(end) = rest[1..].iter().position(|&b| b == q) else {
                        break;
                    };
                    pos += 1 + end + 1;
                    &rest[1..1 + end]
                }
                // Unquoted: HTML5 ends the value at whitespace or `>`.
                Some(_) => {
                    let end = rest
                        .iter()
                        .position(|&b| b.is_ascii_whitespace() || b == b'>')
                        .unwrap_or(rest.len());
                    pos += end;
                    &rest[..end]
                }
            };

            // Only absolute paths.
            if url_bytes.first() != Some(&b'/') {
                continue;
            }
            let url_str = match std::str::from_utf8(url_bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if preload_as(url_str).is_empty() {
                continue;
            }
            hints.push(url_str.to_string());
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
    if needle.is_empty() {
        return Some(0);
    }
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

    /// The cache keys on the full path including the query, so a hint has to
    /// name what the page will actually request. Stripping `?v=123` here
    /// preloads an entry nothing asks for and the visitor still pays the miss.
    #[test]
    fn a_hint_keeps_the_query_the_page_will_request() {
        let html = br#"<link href="/assets/style.css?v=123" rel="stylesheet">"#;
        let hints = extract_hints(html, "text/html");
        assert_eq!(hints, vec!["/assets/style.css?v=123"]);
    }

    /// ...and the header still has to say what kind of resource it is. A
    /// `rel=preload` with no `as=` is ignored by browsers, so reading the
    /// extension has to see past the query.
    #[test]
    fn a_cache_busted_url_still_gets_its_as_value() {
        assert_eq!(
            link_header("/assets/css/style.css?v=ae331def"),
            "</assets/css/style.css?v=ae331def>; rel=preload; as=style"
        );
    }

    /// HTML5 allows an unquoted attribute value, ending at whitespace or `>`.
    #[test]
    fn unquoted_attribute_values_are_read() {
        let html = br#"<link href=/assets/style.css?v=ae331def rel=stylesheet><script src=/assets/app.js></script><img src=/a.webp>"#;
        let hints = extract_hints(html, "text/html");
        assert_eq!(
            hints,
            vec!["/a.webp", "/assets/app.js", "/assets/style.css?v=ae331def"]
        );
    }

    /// `data-href` and `xlink:href` are not `href`. An attribute name is
    /// preceded by whitespace, which is what separates them.
    #[test]
    fn attributes_that_merely_end_in_href_are_not_read() {
        let html = br#"<div data-href=/not-a-hint.css></div><use xlink:href=/also-not.svg />"#;
        let hints = extract_hints(html, "text/html");
        assert!(
            hints.is_empty(),
            "matched something it should not: {hints:?}"
        );
    }

    /// **The fixture is what the server emits, not what the parser wants.**
    ///
    /// Every other test here writes its own HTML, and every one of them wrote
    /// it with quotes. They all passed while this function returned nothing at
    /// all for every page on a deployment with minification on, because m6's
    /// own minifier strips attribute quotes. So this one does not write the
    /// input: it runs the same `minify_html` production runs, and asserts on
    /// what comes out the other side.
    #[test]
    fn hints_survive_the_minifier_this_server_runs() {
        let source = br#"
            <html><head>
              <link href="/assets/css/style.css?v=ae331def" rel="stylesheet">
              <link rel="preload" href="/assets/fonts/montserrat.woff2" as="font" crossorigin>
            </head><body>
              <img src="/assets/icons/logo.svg?v=50f21795" width="80" height="80" alt="Logo">
              <script src="/assets/js/nav.js?v=9577f6ad" type="module"></script>
            </body></html>"#;

        let minified = m6_core::minify::minify_html(source, false);
        let rendered = String::from_utf8_lossy(&minified);
        assert!(
            !rendered.contains("href=\""),
            "this test is pointless if the minifier kept the quotes: {rendered}"
        );

        let hints = extract_hints(&minified, "text/html; charset=utf-8");
        assert_eq!(
            hints,
            vec![
                "/assets/css/style.css?v=ae331def",
                "/assets/fonts/montserrat.woff2",
                "/assets/icons/logo.svg?v=50f21795",
                "/assets/js/nav.js?v=9577f6ad",
            ],
            "minified output: {rendered}"
        );
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
