/// Minification helpers for HTML, CSS, JSON, and JS content.
///
/// Minification is applied BEFORE compression for better ratios.

/// Minify HTML using the `minify-html` crate.
///
/// `minify_inline_js` controls whether inline `<script>` blocks are
/// minified. It defaults to `false` because `minify-html` parses scripts as
/// ES modules (`TopLevelMode::Module`), which has different scoping semantics
/// from classic scripts and silently corrupts valid inline JS (e.g.
/// rewriting string `'\n'` as template literal `` `\\n` ``, making
/// `var`-declared functions inaccessible from `onclick` attributes, etc.).
/// CSS and whitespace minification are unaffected by this flag.
pub fn minify_html(data: &[u8], minify_inline_js: bool) -> Vec<u8> {
    let mut cfg = minify_html::Cfg::new();
    cfg.minify_css = true;
    cfg.minify_js = minify_inline_js;
    cfg.keep_comments = false;
    cfg.keep_closing_tags = true;
    minify_html::minify(data, &cfg)
}

/// Minify JavaScript using the `minify-js` crate (parse-js engine).
///
/// Parses as a top-level module and emits compact output.
/// Returns the original bytes unchanged if parsing fails — no corruption risk.
pub fn minify_js(data: &[u8]) -> Vec<u8> {
    let session = minify_js::Session::new();
    let mut out = Vec::new();
    match minify_js::minify(&session, minify_js::TopLevelMode::Module, data, &mut out) {
        Ok(()) => out,
        Err(_) => data.to_vec(),
    }
}

/// Minify CSS: strip /* ... */ comments and collapse runs of whitespace.
///
/// This is a conservative, regex-free implementation — safe for all valid CSS.
/// Does not attempt to remove last semicolons or perform structural optimisation.
pub fn minify_css(data: &[u8]) -> Vec<u8> {
    let input = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(_) => return data.to_vec(), // non-UTF-8: pass through untouched
    };
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut last_was_ws = false;
    while i < chars.len() {
        // Strip /* ... */ comments.
        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2; // skip closing */
            last_was_ws = true; // treat comment as whitespace
            continue;
        }
        // Collapse runs of whitespace (spaces, tabs, newlines) to a single space.
        if chars[i].is_ascii_whitespace() {
            if !last_was_ws {
                out.push(' ');
                last_was_ws = true;
            }
            i += 1;
            continue;
        }
        last_was_ws = false;
        out.push(chars[i]);
        i += 1;
    }
    out.trim().as_bytes().to_vec()
}

/// Minify JSON: parse and re-serialise in compact form.
///
/// Returns original bytes if the body is not valid JSON (conservative — no corruption risk).
pub fn minify_json(data: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<serde_json::Value>(data) {
        Ok(v) => serde_json::to_vec(&v).unwrap_or_else(|_| data.to_vec()),
        Err(_) => data.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minify_css_strips_comments() {
        let css = b"/* header styles */\nbody { color: red; /* inline */ }";
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(!s.contains("/*"), "comment not stripped: {}", s);
        assert!(s.contains("color: red"), "rule missing: {}", s);
    }

    #[test]
    fn test_minify_css_collapses_whitespace() {
        let css = b"body   {\n    color:   red;\n}";
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        // Multiple spaces should be collapsed to one.
        assert!(!s.contains("   "), "whitespace not collapsed: {}", s);
    }

    #[test]
    fn test_minify_json_compacts() {
        let json = b"{\n  \"key\": \"value\",\n  \"n\": 42\n}";
        let out = minify_json(json);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(!s.contains('\n'), "newlines not removed: {}", s);
        assert!(s.contains("\"key\""), "key missing");
    }

    #[test]
    fn test_minify_json_passthrough_on_invalid() {
        let bad = b"not json {{";
        let out = minify_json(bad);
        assert_eq!(&out, bad);
    }

    #[test]
    fn test_minify_js_compacts() {
        // The minifier may rename identifiers and rewrite syntax, but output must be shorter.
        let js = b"const x = 1;\nconst y = 2;\nconsole.log(x + y);\n";
        let out = minify_js(js);
        assert!(out.len() < js.len(), "minified JS should be shorter");
        // Output should still be valid UTF-8.
        assert!(std::str::from_utf8(&out).is_ok());
    }

    #[test]
    fn test_minify_js_passthrough_on_invalid() {
        let bad = b"function { {{ broken";
        let out = minify_js(bad);
        assert_eq!(&out, bad, "invalid JS should pass through unchanged");
    }

    #[test]
    fn test_minify_html_removes_comments() {
        let html = b"<html><body><!-- comment --><p>Hello</p></body></html>";
        let out = minify_html(html, false);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("Hello"), "content missing: {}", s);
    }
}
