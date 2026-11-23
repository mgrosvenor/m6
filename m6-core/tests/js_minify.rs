//! JavaScript minification: the string that started it, a corpus of awkward
//! literals, behaviour equivalence in a real engine, a third-party-sized
//! script, the fallback when a script does not parse, and the inline-script
//! scanner.
//!
//! Behaviour is the test. A minified script may spell a string differently,
//! but it must produce the same value.

use boa_engine::{Context, Source};
use std::path::{Path, PathBuf};

const D3: &str = include_str!("fixtures/d3.v7.9.0.js");

/// Run a classic script in Boa and return `globalThis.__result`.
fn run(script: &str) -> String {
    let mut context = Context::default();
    context
        .eval(Source::from_bytes(script))
        .unwrap_or_else(|error| panic!("the script must run: {error}\n{script}"));
    let value = context
        .eval(Source::from_bytes("globalThis.__result"))
        .expect("the script sets globalThis.__result");
    value
        .to_string(&mut context)
        .expect("__result is a string")
        .to_std_string_escaped()
}

/// Run `body` on a thread with an explicit stack, and carry a panic back to
/// the test harness so a failure is still a failure.
///
/// Boa's parser is recursive descent: it walks the whole expression precedence
/// chain once per bracket level, and its frames are large in a debug build.
/// This fixture's deepest expression nests 13 bracket levels by the nesting
/// guard's own scan. Under the address sanitiser those levels cost the parser
/// more than the 2 MiB a test thread is born with, so the parser overflows and
/// the test binary dies with a stack-overflow report that names Boa's frames.
///
/// Boa is a dev-dependency: it never ships. The production minifier (oxc) has
/// already parsed the same bytes earlier in this test, and it is oxc the
/// request path runs. So the stack this needs is the harness's to supply, and
/// the test does not touch `[limits.minify_nesting]` for it.
#[allow(
    clippy::disallowed_methods,
    reason = "test code: one thread for the whole test, to give Boa's parser a stack; the event-loop rule is about the shipping loop"
)]
fn on_a_boa_stack(body: impl FnOnce() + Send + 'static) {
    // Eight times the 4 MiB measured to pass on aarch64 debug + ASan (2 MiB,
    // the test thread's own stack, overflows). A thread stack is reserved
    // rather than committed, so the size costs address space and nothing else.
    const STACK: usize = 32 * 1024 * 1024;
    std::thread::Builder::new()
        .name("js_minify-boa".to_string())
        .stack_size(STACK)
        .spawn(body)
        .expect("spawn the thread Boa parses on")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

fn minified(script: &str) -> (String, Vec<u8>) {
    let bytes = m6_core::minify::minify_js(script.as_bytes());
    let text = String::from_utf8(bytes.clone()).expect("the minifier emits UTF-8");
    (text, bytes)
}

fn assert_same_behaviour(script: &str) {
    let (min_text, _) = minified(script);
    assert_eq!(
        run(script),
        run(&min_text),
        "the minified script changed behaviour\n--- original ---\n{script}\n--- minified ---\n{min_text}"
    );
}

/// The string from #357: an apostrophe and two escapes in one literal.
#[test]
fn the_string_that_started_this_keeps_its_value() {
    let script = r#"var m = "That didn't verify \u2014 tab\there."; globalThis.__result = JSON.stringify(m) + "|" + m.length;"#;
    let (min_text, min_bytes) = minified(script);
    assert_same_behaviour(script);
    assert!(
        !min_text.contains(r#"\\u2014"#),
        "the escape was doubled: {min_text}"
    );
    assert!(
        !min_bytes.contains(&0),
        "the output holds a NUL byte: {min_text}"
    );
    assert!(min_text.contains("didn't"));
}

/// Apostrophes, quotes, `\u` escapes, template literals, regex literals,
/// `</script>` in a string, control characters and unicode.
#[test]
fn a_corpus_of_tricky_strings_keeps_its_behaviour() {
    let corpus = [
        r#"var a = "it's"; var b = 'he said "no"'; globalThis.__result = JSON.stringify([a,b,a.length,b.length]);"#,
        r#"var m = "That didn't verify \u2014 tab\there."; globalThis.__result = JSON.stringify([m,m.length]);"#,
        r#"var u = "\u00e9\u2014\ud83d\ude00"; globalThis.__result = JSON.stringify([u,u.length]);"#,
        r#"var t = `a${1 + 1}b`; var q = `quote's and "double"`; globalThis.__result = JSON.stringify([t,q]);"#,
        r#"var re = /a\/b[/]c/gi; globalThis.__result = JSON.stringify([re.source,re.flags]);"#,
        r#"var s = "<\/script>"; var t = "</scri" + "pt>"; globalThis.__result = JSON.stringify([s,t]);"#,
        r#"var s = "a\r\nb"; var t = "a\rb"; var u = "line\nnext"; globalThis.__result = JSON.stringify([s,t,u,s.length,t.length]);"#,
        r#"var s = "\u0000nul"; var t = "\u0007bell"; globalThis.__result = JSON.stringify([s,t,s.length]);"#,
        r#"var s = "${x}"; var t = "`"; var u = "\\uD83D\\uDE42"; globalThis.__result = JSON.stringify([s,t,u]);"#,
        r#"var euro = "\u20ac"; var family = "\ud83d\udc68\u200d\ud83d\udc69\u200d\ud83d\udc67"; globalThis.__result = JSON.stringify([euro,family,family.length]);"#,
        r#"var lone = "\ud800"; globalThis.__result = JSON.stringify([lone,lone.length]);"#,
    ];
    for script in corpus {
        assert_same_behaviour(script);
    }
}

/// A classic script's top-level names are globals another script or an inline
/// handler can use, so they keep their names and an unreferenced declaration
/// is not dropped.
#[test]
fn top_level_names_survive() {
    let script = r#"function pageGlobal(){ return "kept"; } var pageGlobalVar = 3; function unusedGlobal(){ return 1; } globalThis.__result = pageGlobal() + pageGlobalVar;"#;
    let (min_text, _) = minified(script);
    assert_same_behaviour(script);
    for name in ["pageGlobal", "pageGlobalVar", "unusedGlobal"] {
        assert!(min_text.contains(name), "{name} was changed: {min_text}");
    }
}

/// A script the parser cannot read is served as written. That is the fallback
/// between a minifier bug and a broken or failed response.
#[test]
fn a_script_that_does_not_parse_is_unchanged() {
    for bad in [
        "function ( { this is not javascript",
        "import x from \"y\"; export default x;",
        "var s = \"unterminated",
        "let = ;",
    ] {
        assert_eq!(
            m6_core::minify::minify_js(bad.as_bytes()),
            bad.as_bytes(),
            "the fallback must return the input bytes"
        );
        assert!(m6_core::minify::try_minify_js(bad.as_bytes()).is_none());
    }
}

/// An inline script the parser cannot read is served as written. This is the
/// same fallback as the file path, through the HTML entry point.
#[test]
fn an_inline_script_that_does_not_parse_is_served_as_written() {
    let html = br#"<html><body><script>function ( { this is not javascript</script></body></html>"#;
    let out = m6_core::minify::minify_html_with(html, true);
    let text = String::from_utf8(out).expect("HTML stays UTF-8");
    assert!(
        text.contains("function ( { this is not javascript"),
        "the fallback changed the body: {text}"
    );
}

/// The inline scanner minifies classic scripts and leaves JSON, modules and
/// other script types alone.
#[test]
fn inline_scripts_are_minified_and_everything_else_is_left_alone() {
    let html = br#"<html><body><script>var m = "didn't \u2014"; globalThis.x = m;</script><script type="application/ld+json">{"a": 1}</script><script type="module">// module comment
import x from "y";
export default x;</script><script type="text/template"><b>raw</b></script></body></html>"#;
    let out = m6_core::minify::minify_inline_scripts(html);
    let text = String::from_utf8(out).expect("HTML stays UTF-8");
    assert!(text.contains(r#"{"a": 1}"#), "JSON-LD was changed: {text}");
    assert!(
        text.contains("import x"),
        "the module must be parsed as a module and kept: {text}"
    );
    assert!(
        text.contains("<b>raw</b>"),
        "the template script was changed: {text}"
    );
    assert!(
        !text.contains("globalThis.x = m"),
        "the classic script was not minified: {text}"
    );
}

/// A `type="module"` inline body is parsed as a module, not as a classic
/// script. The wrong outcome: `import` is a syntax error in classic-script
/// mode, so the body falls back on every request and the journal fills.
#[test]
fn a_module_inline_script_is_parsed_as_a_module() {
    let html = br#"<script type="module">// remove me
import x from "y";
export default x;</script>"#;
    let out = m6_core::minify::minify_inline_scripts(html);
    let text = String::from_utf8(out).expect("HTML stays UTF-8");
    assert!(
        text.contains("from\"y\""),
        "the module body was not minified: {text}"
    );
    assert!(text.contains("import x"), "the import was lost: {text}");
    assert!(
        text.contains("export default"),
        "the export was lost: {text}"
    );
}

/// A module file served by its extension is parsed as a module. The wrong
/// outcome: `.mjs` is `text/javascript`, and a module parsed as a classic
/// script fails on every GET.
#[test]
fn a_module_file_is_parsed_as_a_module() {
    let module = b"import x from \"y\";\nexport default x;\n";
    let kind = m6_core::minify::JsKind::for_path(Path::new("app.mjs"));
    assert_eq!(kind, m6_core::minify::JsKind::Module);
    let min =
        m6_core::minify::try_minify_js_with(kind, module, m6_core::minify::ParseBudget::builtin())
            .expect("a module parses");
    assert!(min.len() <= module.len(), "minifying never grows the body");
    assert_eq!(
        m6_core::minify::JsKind::for_path(Path::new("app.js")),
        m6_core::minify::JsKind::Script
    );
}

/// A `</script>` formed inside a string must not close the element.
#[test]
fn a_script_body_never_gains_a_close_tag() {
    let html = br#"<script>var s = "</scr" + "ipt>"; globalThis.s = s;</script>"#;
    let out = m6_core::minify::minify_inline_scripts(html);
    let text = String::from_utf8(out).expect("HTML stays UTF-8");
    assert_eq!(
        text.matches("</script").count(),
        1,
        "a second close tag was formed: {text}"
    );
    assert!(text.contains(r#"<\/script>"#), "{text}");
}

/// A `<script>` in an attribute value is data, not an element.
#[test]
fn a_script_in_an_attribute_is_not_an_element() {
    let html = br#"<div data-note="a <script> b"></div>"#;
    assert_eq!(m6_core::minify::minify_inline_scripts(html), html);
}

/// A third-party-sized classic script round-trips with the same behaviour on
/// a small harness, and shrinks. Boa runs it on its own stack: see
/// [`on_a_boa_stack`].
#[test]
fn a_third_party_sized_script_round_trips() {
    on_a_boa_stack(|| {
        let harness = r#"globalThis.__result = JSON.stringify([d3.range(10).join(','), d3.max([1,5,2]), d3.min([1,5,2]), d3.mean([1,2,3]), d3.format('.2f')(1.234), d3.csvParse('a,b\n1,2\n3,4').length, d3.scaleLinear().domain([0,10]).range([0,100])(5), d3.sum([1,2,3]), d3.quantileSorted([1,2,3,4],0.5), d3.utcFormat('%Y-%m-%d')(new Date(Date.UTC(2020,0,2)))]);"#;
        let (min_text, min_bytes) = minified(D3);
        assert!(
            min_bytes.len() < D3.len() * 7 / 10,
            "the fixture only fell from {} to {} bytes",
            D3.len(),
            min_bytes.len()
        );
        assert!(
            !min_text.contains("</script"),
            "the output holds a close tag"
        );
        let original = format!("{D3}\n{harness}");
        let minified_script = format!("{min_text}\n{harness}");
        assert_eq!(run(&original), run(&minified_script));
    });
}

/// Every example's JavaScript file parses, and every classic inline script in
/// an example template parses. The examples gate then builds and runs them.
///
/// The templates half reads `*.html.tmpl` as well as `*.html`. The loader reads
/// only `.tmpl` files, so an extension test for `html` alone read only the one
/// `index.html` after the rename: that left the three examples with classic
/// inline JavaScript unchecked while the test still passed. The floors below
/// fail if a later rename shrinks what is scanned again.
#[test]
fn every_example_script_minifies() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples");
    let mut files = Vec::new();
    walk(&root, &mut files);
    let mut before = 0usize;
    let mut after = 0usize;
    let mut templates = 0usize;
    let mut inline_checked = 0usize;
    let mut with_inline: Vec<&Path> = Vec::new();
    for path in &files {
        let bytes = std::fs::read(path).expect("an example file is readable");
        if path.extension().is_some_and(|ext| ext == "js") {
            before += bytes.len();
            let min = match m6_core::minify::try_minify_js_with(
                m6_core::minify::JsKind::Script,
                &bytes,
                m6_core::minify::ParseBudget::builtin(),
            ) {
                Ok(min) => min,
                // A third-party bundle over the parse budget is served as
                // written. That is the documented fallback, not a parse error:
                // the point of the budget is that the loop does not parse it.
                Err(m6_core::minify::JsFallback::OverBudget { .. }) => {
                    eprintln!(
                        "{}: {} bytes, over the parse budget, served as written",
                        path.display(),
                        bytes.len()
                    );
                    after += bytes.len();
                    continue;
                }
                Err(fallback) => {
                    panic!(
                        "example script {} does not parse: {fallback}",
                        path.display()
                    )
                }
            };
            eprintln!("{}: {} -> {} bytes", path.display(), bytes.len(), min.len());
            after += min.len();
            if let Ok(text) = std::str::from_utf8(&min) {
                assert!(
                    !text.contains("</script"),
                    "example script {} formed a close tag",
                    path.display()
                );
            }
        } else if is_template(path) {
            templates += 1;
            let text = std::str::from_utf8(&bytes).expect("a template is UTF-8");
            let mut found = false;
            for (tag, body) in inline_scripts(text) {
                if body.trim().is_empty() || !classic_tag(&tag) {
                    continue;
                }
                found = true;
                inline_checked += 1;
                match m6_core::minify::try_minify_js_with(
                    m6_core::minify::JsKind::Script,
                    body.as_bytes(),
                    m6_core::minify::ParseBudget::builtin(),
                ) {
                    Ok(_) => {}
                    Err(m6_core::minify::JsFallback::OverBudget { .. }) => {}
                    Err(fallback) => panic!(
                        "classic inline script in {} does not parse: {fallback}\n{body}",
                        path.display()
                    ),
                }
            }
            if found {
                with_inline.push(path);
            }
        }
    }
    eprintln!("example JavaScript: {before} bytes before, {after} after");
    assert!(before > 0, "no example JavaScript was found");
    assert!(
        after <= before,
        "minification grew the examples' JavaScript"
    );
    // The examples carry 55 `*.html.tmpl` templates and one `index.html`. A
    // floor, so a new example does not fail the test and a rename that takes
    // templates out of the scan does.
    assert!(
        templates >= 56,
        "the inline half scanned {templates} example templates, not at least 56. \
         A template is `*.html`, `*.htm` or `*.tmpl` here: a scan that names only \
         `html` reads only `index.html` after the rename to `.html.tmpl`"
    );
    // Three examples carry substantive classic inline JavaScript, and the
    // admin dashboard a fourth. A floor on the scripts checked fails if any of
    // them leaves the scan, which is the failure this guard exists for.
    assert!(
        inline_checked >= 4,
        "the inline half checked {inline_checked} classic inline scripts, not at least 4"
    );
    for want in [
        "05-cms/templates/login.html.tmpl",
        "05-cms/templates/cms/dashboard.html.tmpl",
        "07-dev-to-production/templates/cms/editor.html.tmpl",
    ] {
        assert!(
            with_inline.iter().any(|path| path.ends_with(want)),
            "the inline half no longer scans {want}, which carries classic inline JavaScript. \
             Scanned with inline scripts: {:?}",
            with_inline
        );
    }
}

/// Whether `path` is an example template the inline half reads.
///
/// The loader reads only `.tmpl` files and every example template is
/// `*.html.tmpl`. `html` and `htm` stay in the set because a template copied
/// from an older example still uses them.
fn is_template(path: &Path) -> bool {
    matches!(
        path.extension().and_then(std::ffi::OsStr::to_str),
        Some("html" | "htm" | "tmpl")
    )
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn inline_scripts(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = html[from..].find("<script") {
        let start = from + found;
        let Some(close_tag) = html[start..].find('>') else {
            break;
        };
        let open_end = start + close_tag + 1;
        let Some(close) = html[open_end..].find("</script") else {
            break;
        };
        out.push((
            html[start..open_end].to_string(),
            html[open_end..open_end + close].to_string(),
        ));
        from = open_end + close + "</script".len();
    }
    out
}

fn classic_tag(tag: &str) -> bool {
    let lower = tag.to_ascii_lowercase();
    !lower.contains("src=")
        && !lower.contains("type=\"module\"")
        && !lower.contains("type='module'")
        && !lower.contains("ld+json")
        && !lower.contains("application/json")
        && !lower.contains("type=\"text/")
        && !lower.contains("type='text/")
}
