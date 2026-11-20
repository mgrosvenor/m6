//! Minification for HTML, CSS and JSON.
//!
//! Applied before compression, and only for types the config enables. Each
//! function returns the original bytes unchanged if the input does not parse,
//! so a minifier that cannot understand a file degrades to passing it through
//! rather than corrupting it.
//!
//! JavaScript is never minified, in a script file or inline in a page. The
//! only engine available re-emits a string holding an apostrophe as a template
//! literal and doubles its escapes, so `"didn't \u2014"` came out as
//! `` `didn't \\u2014` ``, and a newer release of it inserts a NUL byte after
//! a decoded escape. A script is served as written.

/// Minify HTML using the `minify-html` crate.
///
/// Inline `<script>` blocks are left as written, for the reason above. CSS and
/// whitespace minification apply.
pub fn minify_html(data: &[u8]) -> Vec<u8> {
    let mut cfg = minify_html::Cfg::new();
    cfg.minify_css = true;
    // Explicit, so a change of the crate's default cannot switch it on.
    cfg.minify_js = false;
    cfg.keep_comments = false;
    cfg.keep_closing_tags = true;
    cfg.keep_spaces_between_attributes = true;
    cfg.keep_html_and_head_opening_tags = true;
    minify_html::minify(data, &cfg)
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

    /// An inline script goes through `minify_html` as written. The engine
    /// that was switched on for it turned this string into a template literal
    /// with doubled escapes.
    #[test]
    fn test_minify_html_leaves_an_inline_script_as_written() {
        let html = b"<html><body><script>var m = \"That didn't verify \\u2014 tab\\there.\";</script></body></html>";
        let out = minify_html(html);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains("var m = \"That didn't verify \\u2014 tab\\there.\";"),
            "inline script changed: {s}"
        );
    }

    #[test]
    fn test_minify_html_removes_comments() {
        let html = b"<html><body><!-- comment --><p>Hello</p></body></html>";
        let out = minify_html(html);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("Hello"), "content missing: {}", s);
    }
}
