//! Minification for HTML, CSS, JSON and JavaScript.
//!
//! Applied before compression, and only for types the config enables. Each
//! function returns the original bytes unchanged if the input does not parse,
//! so a minifier that cannot understand a file degrades to passing it through
//! rather than corrupting it.
//!
//! JavaScript is minified by `oxc_minifier`, which parses a classic script,
//! compresses it and rewrites local bindings. Top-level bindings keep their
//! names and are never dropped, because another script on the page may use
//! them. A module script and any non-JavaScript `<script>` type are left as
//! written, and a parse failure serves the input bytes.

use oxc::allocator::Allocator;
use oxc::codegen::{Codegen, CodegenOptions};
use oxc::minifier::{
    CompressOptions, CompressOptionsUnused, MangleOptions, Minifier, MinifierOptions,
};
use oxc::parser::Parser;
use oxc::span::SourceType;

/// Minify HTML using the `minify-html` crate.
///
/// Inline `<script>` blocks are left as written. CSS and whitespace
/// minification apply. Use [`minify_html_with`] to minify the classic inline
/// scripts too.
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

/// Minify HTML, and the classic inline scripts in it when `inline_js` is set.
///
/// A `<script>` whose body is not classic JavaScript is left as written. That
/// covers `type="module"`, `application/ld+json` and every other JSON or
/// template type, and a tag with a `src` attribute.
pub fn minify_html_with(data: &[u8], inline_js: bool) -> Vec<u8> {
    let html = minify_html(data);
    if inline_js {
        minify_inline_scripts_vec(html)
    } else {
        html
    }
}

/// Minify one classic script. Returns the input unchanged on any failure, so a
/// script the minifier cannot understand is served as written.
pub fn minify_js(data: &[u8]) -> Vec<u8> {
    try_minify_js(data).unwrap_or_else(|| data.to_vec())
}

/// Minify one classic script, or return `None` when it does not parse.
///
/// `Some(bytes)` is the minified body, or the input when minifying it would
/// not make it smaller.
pub fn try_minify_js(data: &[u8]) -> Option<Vec<u8>> {
    let source = match std::str::from_utf8(data) {
        Ok(source) => source,
        Err(_) => {
            crate::log::warn!("minify_js: not UTF-8, serving the original bytes");
            return None;
        }
    };
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::script()).parse();
    if !parsed.diagnostics.is_empty() {
        let first = parsed
            .diagnostics
            .first()
            .map(std::string::ToString::to_string)
            .unwrap_or_default();
        crate::log::warn!(error = %first, "minify_js: parse failed, serving the original bytes");
        return None;
    }
    let mut program = parsed.program;
    // Top-level names are the page's globals. Another script may call them, and
    // an inline handler may name them, so they keep their names and no
    // unreferenced top-level declaration is dropped. Local names are mangled.
    let options = MinifierOptions {
        mangle: Some(MangleOptions {
            top_level: Some(false),
            ..MangleOptions::default()
        }),
        mangle_properties: None,
        compress: Some(CompressOptions {
            unused: CompressOptionsUnused::Keep,
            ..CompressOptions::default()
        }),
    };
    let minified = Minifier::new(options).minify(&allocator, &mut program);
    let code = Codegen::new()
        .with_options(CodegenOptions {
            minify: true,
            ..CodegenOptions::default()
        })
        .with_scoping(minified.scoping)
        .with_private_member_mappings(minified.class_private_mappings)
        .build(&program)
        .code;
    if code.is_empty() && !source.trim().is_empty() {
        crate::log::warn!("minify_js: produced an empty body, serving the original bytes");
        return None;
    }
    // A body that grew is not worth sending. This also leaves an already
    // minified script alone.
    if code.len() >= data.len() {
        return Some(data.to_vec());
    }
    Some(code.into_bytes())
}

/// Minify the classic inline `<script>` bodies in an HTML document.
///
/// The scanner walks tags, so a `<script>` in an attribute value is data. A
/// body is minified only when the tag has no `src` and its `type` is absent or
/// a JavaScript MIME type. Module scripts and non-JavaScript types keep their
/// bytes.
pub fn minify_inline_scripts(html: &[u8]) -> Vec<u8> {
    minify_inline_scripts_vec(html.to_vec())
}

/// The owned form, so a page with no classic script returns its bytes without a
/// second allocation. The page path is measured, and one allocation there is a
/// finding.
fn minify_inline_scripts_vec(html: Vec<u8>) -> Vec<u8> {
    let text = match std::str::from_utf8(&html) {
        Ok(text) => text,
        Err(_) => return html,
    };
    if !has_classic_inline_script(text) {
        return html;
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"<!--") {
            match find_ci(&bytes[i + 4..], b"-->") {
                Some(offset) => {
                    i += 4 + offset + 3;
                    continue;
                }
                None => break,
            }
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            match tag_end(bytes, i) {
                Some(end) => {
                    i = end;
                    continue;
                }
                None => break,
            }
        }
        if starts_with_ci(&bytes[i..], b"<script") && is_tag_boundary(bytes.get(i + 7).copied()) {
            let Some(open_end) = tag_end(bytes, i) else {
                break;
            };
            let Some(close) = find_ci(&bytes[open_end..], b"</script").map(|o| open_end + o) else {
                break;
            };
            let Some(close_end) = tag_end(bytes, close) else {
                break;
            };
            let tag = &text[i..open_end];
            let body = &text[open_end..close];
            let replacement = if script_is_classic(tag) && !body.trim().is_empty() {
                minify_inline_body(body)
            } else {
                body.as_bytes().to_vec()
            };
            out.push_str(&text[copied..open_end]);
            match std::str::from_utf8(&replacement) {
                Ok(body) => out.push_str(body),
                Err(_) => out.push_str(body),
            }
            copied = close;
            i = close_end;
            continue;
        }
        match tag_end(bytes, i) {
            Some(end) => i = end,
            None => break,
        }
    }
    out.push_str(&text[copied..]);
    out.into_bytes()
}

/// Whether the document has at least one `<script>` this pass would rewrite.
/// Scans the same tags as the rewrite and allocates nothing.
fn has_classic_inline_script(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"<!--") {
            match find_ci(&bytes[i + 4..], b"-->") {
                Some(offset) => {
                    i += 4 + offset + 3;
                    continue;
                }
                None => return false,
            }
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            match tag_end(bytes, i) {
                Some(end) => {
                    i = end;
                    continue;
                }
                None => return false,
            }
        }
        if starts_with_ci(&bytes[i..], b"<script") && is_tag_boundary(bytes.get(i + 7).copied()) {
            let Some(open_end) = tag_end(bytes, i) else {
                return false;
            };
            let Some(close) = find_ci(&bytes[open_end..], b"</script").map(|o| open_end + o) else {
                return false;
            };
            if script_is_classic(&text[i..open_end]) && !text[open_end..close].trim().is_empty() {
                return true;
            }
            let Some(close_end) = tag_end(bytes, close) else {
                return false;
            };
            i = close_end;
            continue;
        }
        match tag_end(bytes, i) {
            Some(end) => i = end,
            None => return false,
        }
    }
    false
}

fn minify_inline_body(body: &str) -> Vec<u8> {
    let minified = minify_js(body.as_bytes());
    if contains_ci(&minified, b"</script") {
        crate::log::warn!("minify_js: output held a script close tag, serving the original bytes");
        return body.as_bytes().to_vec();
    }
    minified
}

fn is_tag_boundary(next: Option<u8>) -> bool {
    match next {
        None => true,
        Some(byte) => matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/'),
    }
}

/// The index just past the `>` that closes the tag opened at `start`, honouring
/// quoted attribute values.
fn tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut quote = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
        } else if byte == b'"' || byte == b'\'' {
            quote = byte;
        } else if byte == b'>' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Whether the `<script ...>` tag opens a classic script that a minifier can
/// read. A `src` tag has no body. A module or a non-JavaScript type is left
/// alone.
fn script_is_classic(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    let mut i = 7;
    let mut kind: Option<String> = None;
    let mut language: Option<String> = None;
    let mut has_src = false;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b'>' {
            break;
        }
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let name = tag[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let value_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                value = tag[value_start..i].to_string();
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let value_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                    i += 1;
                }
                value = tag[value_start..i].to_string();
            }
        }
        let value = value.trim().to_ascii_lowercase();
        match name.as_str() {
            "src" => has_src = true,
            "type" => kind = Some(value),
            "language" => language = Some(value),
            _ => {}
        }
    }
    if has_src {
        return false;
    }
    if let Some(kind) = kind {
        if kind.is_empty() {
            return language.as_deref().is_none_or(is_javascript_language);
        }
        return is_javascript_type(&kind);
    }
    language.as_deref().is_none_or(is_javascript_language)
}

fn is_javascript_type(kind: &str) -> bool {
    let kind = kind.split(';').next().unwrap_or(kind).trim();
    matches!(
        kind,
        "text/javascript"
            | "application/javascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "application/ecmascript"
            | "text/jscript"
            | "text/livescript"
            | "text/x-javascript"
    ) || kind.starts_with("text/javascript1.")
}

fn is_javascript_language(language: &str) -> bool {
    language.contains("javascript") || language.contains("ecmascript")
}

fn starts_with_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    find_ci(haystack, needle).is_some()
}

fn find_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
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
