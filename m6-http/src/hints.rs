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
            // The page's own instruction wins over ours.
            //
            // `loading=lazy` says "do not fetch this until it is needed". A hint
            // naming the same URL says "fetch it now", arrives first, and wins.
            // Lazy loading on such a page is then inert: measured on one live
            // site, 17 of 17 images marked lazy were preloaded anyway.
            //
            // A server overriding an explicit, standardised author instruction
            // is wrong whether or not it is faster, so this is a correctness
            // rule rather than a tuning one.
            if tag_defers_loading(body, at) {
                continue;
            }
            hints.push(url_str.to_string());
        }
    }

    // Ordered by what the resource IS, then by URL.
    //
    // A plain lexicographic sort ordered by accident of directory name: on one
    // live site `/assets/company-logos/` sorted ahead of `/assets/css/`, so the
    // render-blocking stylesheet was announced 18th behind 17 logos. Ranking on
    // `preload_as` needs nothing about a site's layout, because `as=` is the
    // standard's own statement of a resource's role.
    //
    // Browsers assign preload priority from `as=` rather than from header order,
    // so this is unlikely to be worth much on its own. It is here because the
    // output should be defensible to a reader, and because ordering by accident
    // is not a decision anyone made.
    hints.sort_by(|a, b| preload_rank(a).cmp(&preload_rank(b)).then_with(|| a.cmp(b)));
    // Still correct after the change: equal strings share a rank, so duplicates
    // remain adjacent.
    hints.dedup();
    hints
}

/// Rank a hint by the role `preload_as` gives it: render-blocking first, then
/// parser-blocking, then what the browser can defer.
fn preload_rank(url: &str) -> u8 {
    match preload_as(url) {
        "style" => 0,
        "font" => 1,
        "script" => 2,
        "image" => 3,
        _ => 4,
    }
}

/// Does the tag containing the attribute at `attr_pos` ask the browser to defer
/// the fetch?
///
/// True for `loading=lazy` and for `fetchpriority=low`, quoted or not, since
/// both are the author saying this resource is not wanted yet. Walks back to the
/// opening `<` and forward to the closing `>` so only the one tag is examined; a
/// bare `loading=lazy` elsewhere in the document cannot suppress an unrelated
/// URL.
fn tag_defers_loading(body: &[u8], attr_pos: usize) -> bool {
    // Bound the walk. A tag longer than this is malformed, and scanning to the
    // start of the document for every attribute on a large page is not free:
    // this runs on the cache-miss path for every HTML response.
    const MAX_TAG: usize = 4096;

    let start = body[..attr_pos]
        .iter()
        .rposition(|&b| b == b'<')
        .filter(|s| attr_pos - s <= MAX_TAG);
    let Some(start) = start else { return false };
    let end = body[attr_pos..]
        .iter()
        .position(|&b| b == b'>')
        .map(|e| attr_pos + e)
        .filter(|e| e - start <= MAX_TAG)
        .unwrap_or(body.len().min(start + MAX_TAG));

    let tag = &body[start..end];
    attr_has_value(tag, b"loading=", b"lazy") || attr_has_value(tag, b"fetchpriority=", b"low")
}

/// Is `name` present in `tag` with the value `want`, quoted or unquoted?
fn attr_has_value(tag: &[u8], name: &[u8], want: &[u8]) -> bool {
    let mut pos = 0usize;
    while let Some(rel) = find_bytes(&tag[pos..], name) {
        let at = pos + rel;
        pos = at + name.len();
        // An attribute name is preceded by whitespace, the same rule the URL
        // scan above uses: without it `data-loading=` reads as `loading=`.
        if at > 0 && !tag[at - 1].is_ascii_whitespace() {
            continue;
        }
        let rest = &tag[pos..];
        let value = match rest.first() {
            None => return false,
            Some(&q) if q == b'"' || q == b'\'' => match rest[1..].iter().position(|&b| b == q) {
                Some(e) => &rest[1..1 + e],
                None => return false,
            },
            Some(_) => {
                let e = rest
                    .iter()
                    .position(|&b| b.is_ascii_whitespace() || b == b'>')
                    .unwrap_or(rest.len());
                &rest[..e]
            }
        };
        if value.eq_ignore_ascii_case(want) {
            return true;
        }
    }
    false
}

/// Split a hint URL into the path and query a request is built from.
///
/// A hint is a URL as the page wrote it, so it carries its cache-busting query:
/// `/assets/css/style.css?v=ae331def`. Anything turning one into a request must
/// pass those as two parts, because the cache key builders strip the path at
/// `?` and read the query only from their own argument. Hand them the whole
/// string as a path and `/a.css?v=1` keys identically to `/a.css`, while the
/// request still fetches the versioned resource: the versioned response is then
/// stored under the unversioned key.
pub fn split_url(url: &str) -> (&str, Option<&str>) {
    match url.split_once('?') {
        Some((path, query)) if !query.is_empty() => (path, Some(query)),
        Some((path, _)) => (path, None),
        None => (url, None),
    }
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

    /// A hint is a URL, and anything building a request from one needs its two
    /// parts separately. See `split_url` for what passing the whole string as a
    /// path costs.
    #[test]
    fn a_hint_splits_into_the_path_and_query_a_request_needs() {
        assert_eq!(
            split_url("/assets/css/style.css?v=ae331def"),
            ("/assets/css/style.css", Some("v=ae331def"))
        );
        assert_eq!(
            split_url("/assets/fonts/montserrat.woff2"),
            ("/assets/fonts/montserrat.woff2", None)
        );
        // A trailing `?` with nothing after it is not a query.
        assert_eq!(split_url("/a.css?"), ("/a.css", None));
        // Only the first `?` separates; the rest belongs to the query.
        assert_eq!(split_url("/a.css?a=1?b=2"), ("/a.css", Some("a=1?b=2")));
    }

    /// HTML5 allows an unquoted attribute value, ending at whitespace or `>`.
    #[test]
    fn unquoted_attribute_values_are_read() {
        let html = br#"<link href=/assets/style.css?v=ae331def rel=stylesheet><script src=/assets/app.js></script><img src=/a.webp>"#;
        let hints = extract_hints(html, "text/html");
        // style, script, image. Alphabetical would have put the image first.
        assert_eq!(
            hints,
            vec!["/assets/style.css?v=ae331def", "/assets/app.js", "/a.webp"]
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
            // style, font, script, image: the ranked order, not alphabetical.
            // The image sorted third here before the ranking went in.
            vec![
                "/assets/css/style.css?v=ae331def",
                "/assets/fonts/montserrat.woff2",
                "/assets/js/nav.js?v=9577f6ad",
                "/assets/icons/logo.svg?v=50f21795",
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

    // ── The page's own deferral instructions win ─────────────────────────────

    #[test]
    fn lazy_images_are_not_hinted() {
        // Unquoted, which is what m6's own minifier emits.
        let html = br#"<img src=/a/logo.jpg loading=lazy width=10 height=10>"#;
        assert!(extract_hints(html, "text/html").is_empty());
    }

    #[test]
    fn lazy_images_are_not_hinted_when_quoted() {
        let html = br#"<img src="/a/logo.jpg" loading="lazy">"#;
        assert!(extract_hints(html, "text/html").is_empty());
    }

    #[test]
    fn lazy_before_src_is_still_seen() {
        // Attribute order is the author's choice; the whole tag is examined.
        let html = br#"<img loading=lazy src=/a/logo.jpg>"#;
        assert!(extract_hints(html, "text/html").is_empty());
    }

    #[test]
    fn fetchpriority_low_is_not_hinted() {
        let html = br#"<img src=/a/logo.jpg fetchpriority=low>"#;
        assert!(extract_hints(html, "text/html").is_empty());
    }

    #[test]
    fn eager_images_are_still_hinted() {
        let html = br#"<img src=/a/hero.jpg loading=eager>"#;
        assert_eq!(extract_hints(html, "text/html"), vec!["/a/hero.jpg"]);
    }

    #[test]
    fn lazy_on_one_tag_does_not_suppress_another() {
        // The regression this guards: a document-wide search for `loading=lazy`
        // would drop every hint on any page that lazy-loads anything.
        let html = br#"<img src=/a/logo.jpg loading=lazy><img src=/a/hero.jpg>"#;
        assert_eq!(extract_hints(html, "text/html"), vec!["/a/hero.jpg"]);
    }

    #[test]
    fn data_loading_is_not_read_as_loading() {
        let html = br#"<img src=/a/hero.jpg data-loading=lazy>"#;
        assert_eq!(extract_hints(html, "text/html"), vec!["/a/hero.jpg"]);
    }

    // ── Order is by role, not by directory name ─────────────────────────────

    #[test]
    fn style_and_font_precede_script_and_image() {
        // Paths chosen so a plain lexicographic sort gives the WRONG answer:
        // `/a-img/` < `/b-css/` < `/c-font/` < `/d-js/`.
        let html = br#"<img src=/a-img/logo.jpg>
                       <link href=/b-css/style.css rel=stylesheet>
                       <link href=/c-font/text.woff2>
                       <script src=/d-js/app.js></script>"#;
        assert_eq!(
            extract_hints(html, "text/html"),
            vec![
                "/b-css/style.css",
                "/c-font/text.woff2",
                "/d-js/app.js",
                "/a-img/logo.jpg",
            ]
        );
    }

    #[test]
    fn duplicates_are_still_removed_after_the_ranked_sort() {
        // dedup() only removes ADJACENT equals, so this guards the sort key:
        // equal strings must still land together.
        let html = br#"<img src=/a/x.jpg><link href=/b/s.css rel=stylesheet><img src=/a/x.jpg>"#;
        assert_eq!(
            extract_hints(html, "text/html"),
            vec!["/b/s.css", "/a/x.jpg"]
        );
    }
}
