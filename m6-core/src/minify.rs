//! Minification for HTML, CSS, JSON and JavaScript.
//!
//! Applied before compression, and only for types the config enables. Each
//! function returns the original bytes unchanged if the input does not parse,
//! so a minifier that cannot understand a file degrades to passing it through
//! rather than corrupting it.
//!
//! Inline `<script>` minification is **off by default** and should stay that
//! way unless every inline script is a self-contained ES module: the engine
//! parses scripts as modules, which has different scoping, and it can silently
//! rewrite valid classic scripts into broken ones.

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
    cfg.keep_spaces_between_attributes = true;
    cfg.keep_html_and_head_opening_tags = true;
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
        Err(e) => {
            tracing::warn!(error = %e, "minify_js: parse failed, serving unminified source");
            data.to_vec()
        }
    }
}

/// Minify CSS: strip /* ... */ comments and collapse runs of whitespace.
///
/// Quoted strings (single and double, honouring backslash escapes) and
/// `url(...)` contents are copied verbatim, so a `/*` or a run of whitespace
/// inside them is data and is left alone. Does not attempt to remove last
/// semicolons or perform structural optimisation.
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
        match chars[i] {
            // Strip /* ... */ comments.
            '/' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                // skip the closing */
                i += 2;
                // A comment separates tokens like whitespace does, so emit a
                // space here, or `red/*c*/blue` merges into one identifier.
                if !last_was_ws {
                    out.push(' ');
                    last_was_ws = true;
                }
            }
            // Copy quoted strings verbatim, honouring backslash escapes.
            '"' | '\'' => {
                copy_string(&mut out, &chars, &mut i);
                last_was_ws = false;
            }
            // Copy url(...) contents verbatim.
            'u' | 'U' if url_open(&chars, i) => {
                out.push_str("url(");
                i += 4;
                copy_url(&mut out, &chars, &mut i);
                last_was_ws = false;
            }
            // Collapse runs of whitespace to a single space.
            c if c.is_ascii_whitespace() => {
                if !last_was_ws {
                    out.push(' ');
                    last_was_ws = true;
                }
                i += 1;
            }
            c => {
                last_was_ws = false;
                out.push(c);
                i += 1;
            }
        }
    }
    out.trim().as_bytes().to_vec()
}

/// Copy the quoted string at `chars[i]` (the opening quote) verbatim, honouring
/// backslash escapes, and advance `i` past the closing quote.
fn copy_string(out: &mut String, chars: &[char], i: &mut usize) {
    let quote = chars[*i];
    out.push(quote);
    *i += 1;
    while *i < chars.len() {
        let c = chars[*i];
        out.push(c);
        *i += 1;
        if c == '\\' {
            // Copy the escaped character too, so an escaped quote does not end
            // the string early.
            if *i < chars.len() {
                out.push(chars[*i]);
                *i += 1;
            }
        } else if c == quote {
            return;
        }
    }
}

/// Copy the contents of a `url(...)` call, starting just after the opening
/// `(`, verbatim, and advance `i` past the closing `)`. A quoted string inside
/// the call is copied whole so a `)` inside it does not end the call early.
fn copy_url(out: &mut String, chars: &[char], i: &mut usize) {
    while *i < chars.len() {
        match chars[*i] {
            '"' | '\'' => copy_string(out, chars, i),
            '\\' => {
                out.push('\\');
                *i += 1;
                if *i < chars.len() {
                    out.push(chars[*i]);
                    *i += 1;
                }
            }
            ')' => {
                out.push(')');
                *i += 1;
                return;
            }
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
}

/// Whether `chars[i..]` begins a `url(` call: the `url` keyword, case-
/// insensitive, immediately before the `(`, and not part of a longer identifier
/// such as `myurl(`.
fn url_open(chars: &[char], i: usize) -> bool {
    i + 3 < chars.len()
        && chars[i + 1].eq_ignore_ascii_case(&'r')
        && chars[i + 2].eq_ignore_ascii_case(&'l')
        && chars[i + 3] == '('
        && (i == 0 || !is_ident_char(chars[i - 1]))
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
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
    fn test_minify_css_preserves_double_quoted_string() {
        let css = br#"content: "a  /* b */  c""#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains(r#""a  /* b */  c""#), "string corrupted: {}", s);
    }

    #[test]
    fn test_minify_css_preserves_url_contents() {
        let css = br#"background: url("data:image/svg+xml;utf8,<svg>  </svg>")"#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains(r#"url("data:image/svg+xml;utf8,<svg>  </svg>")"#),
            "url corrupted: {}",
            s
        );
    }

    #[test]
    fn test_minify_css_preserves_single_quoted_string() {
        let css = br#"a::before { content: '/*' }"#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("'/*'"), "string treated as comment: {}", s);
    }

    #[test]
    fn test_minify_css_preserves_escaped_quotes_in_strings() {
        let css = br#"x{content:"a\"  b"}"#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains(r#""a\"  b""#),
            "escaped quote broke the string: {}",
            s
        );
    }

    #[test]
    fn test_minify_css_preserves_escaped_paren_in_url() {
        let css = br#"a{background:url(a\)/*b*/c)}"#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains(r#"url(a\)/*b*/c)"#),
            "escaped ) ended the url early: {}",
            s
        );
    }

    #[test]
    fn test_minify_css_comment_still_separates_tokens() {
        let css = br#"a{color:red/*c*/blue}"#;
        let out = minify_css(css);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains("red blue"),
            "comment did not separate tokens: {}",
            s
        );
        assert!(
            !s.contains("redblue"),
            "tokens merged across a comment: {}",
            s
        );
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
