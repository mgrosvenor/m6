/// Critical-path benchmarks for m6-render.
///
/// Three tiers:
///   1. Route matching and HTTP parsing (no I/O)
///   2. Template rendering and response serialisation (in-memory)
///   3. Unix socket round-trip (full HTTP/1.1 request ↔ rendered response)
///
/// Run with: cargo bench -p m6-render
use criterion::{black_box, criterion_group, Criterion};
use serde_json::{Map, Value};
use std::io::Write;
use std::os::unix::net::UnixStream;
use tempfile::TempDir;

// Helper: build an HTML response from outside the crate (template_name is pub(crate)).
fn html_response(html: String) -> Response {
    let mut r = Response::status(200);
    r.headers
        .push(("Content-Type".to_string(), "text/html; charset=utf-8".to_string()));
    r.body = html.into_bytes();
    r
}

use m6_render::app::{
    compile_pattern, find_route, match_route, route_specificity, CompiledRoute, RouteMethod,
};
use m6_render::compress::{brotli_compress, gzip_compress};
use m6_render::minify::{minify_css, minify_html, minify_js, minify_json};
use m6_render::request::{parse_cookies, parse_query_string};
use m6_render::response::Response;
use m6_render::server::write_response;

// ── Realistic content fixtures ────────────────────────────────────────────────

const HTML_2KB: &[u8] = b"<!doctype html>
<html lang=\"en\">
<head>
  <meta charset=\"utf-8\">
  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
  <title>Sample Blog Post | My Site</title>
  <link rel=\"stylesheet\" href=\"/assets/css/main.css\">
</head>
<body>
  <header>
    <nav><a href=\"/\">Home</a> <a href=\"/blog\">Blog</a> <a href=\"/about\">About</a></nav>
  </header>
  <main>
    <article>
      <h1>Understanding Rust Lifetimes</h1>
      <p class=\"meta\">Posted on 2024-01-15 by Alice</p>
      <p>Lifetimes in Rust are a way of expressing that references must live long enough.
         The borrow checker uses lifetime annotations to ensure memory safety without
         a garbage collector. In this post we explore how lifetimes work in practice.</p>
      <p>Consider a function that takes two string slices and returns the longer one.
         Without lifetimes, the compiler cannot determine how long the returned reference
         will be valid. With a lifetime annotation <code>'a</code>, we tell the compiler
         that the returned reference lives as long as the shorter of the two inputs.</p>
      <p>Lifetime elision rules allow most functions to omit explicit annotations.
         The compiler applies three rules: each parameter gets its own lifetime, if there
         is exactly one input lifetime it propagates to the output, and if one of the
         parameters is <code>&amp;self</code> its lifetime is assigned to the output.</p>
    </article>
  </main>
  <footer><p>&copy; 2024 My Site</p></footer>
</body>
</html>";

const CSS_8KB: &[u8] = b"
/* === Reset === */
*, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
html { font-size: 16px; line-height: 1.5; -webkit-text-size-adjust: 100%; }
body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif;
       color: #1a1a1a; background: #fff; min-height: 100vh; }
a { color: #0066cc; text-decoration: none; }
a:hover { text-decoration: underline; }
img, svg { display: block; max-width: 100%; }
/* === Layout === */
.container { max-width: 1200px; margin: 0 auto; padding: 0 1rem; }
header { background: #fff; border-bottom: 1px solid #e5e5e5; position: sticky; top: 0; z-index: 100; }
header nav { display: flex; gap: 1.5rem; align-items: center; padding: 1rem 0; }
header nav a { font-weight: 500; color: #333; transition: color 0.2s; }
header nav a:hover { color: #0066cc; text-decoration: none; }
main { padding: 2rem 0; }
footer { border-top: 1px solid #e5e5e5; padding: 2rem 0; text-align: center; color: #666; font-size: 0.875rem; }
/* === Typography === */
h1 { font-size: 2rem; line-height: 1.2; font-weight: 700; margin-bottom: 0.5rem; }
h2 { font-size: 1.5rem; line-height: 1.3; font-weight: 600; margin-bottom: 0.5rem; }
h3 { font-size: 1.25rem; font-weight: 600; margin-bottom: 0.5rem; }
p  { margin-bottom: 1rem; }
code { font-family: 'JetBrains Mono', 'Fira Code', 'Courier New', monospace;
       font-size: 0.875em; background: #f5f5f5; padding: 0.125em 0.375em; border-radius: 3px; }
pre  { background: #1e1e1e; color: #d4d4d4; padding: 1rem 1.25rem; border-radius: 6px;
       overflow-x: auto; margin-bottom: 1rem; }
pre code { background: none; padding: 0; font-size: 0.875rem; }
blockquote { border-left: 4px solid #0066cc; padding-left: 1rem; margin: 1rem 0;
             color: #555; font-style: italic; }
/* === Article === */
article { max-width: 720px; margin: 0 auto; }
article .meta { color: #666; font-size: 0.875rem; margin-bottom: 1.5rem; }
article h1 { margin-bottom: 0.25rem; }
/* === Cards === */
.card { border: 1px solid #e5e5e5; border-radius: 8px; padding: 1.25rem;
        transition: box-shadow 0.2s; }
.card:hover { box-shadow: 0 4px 12px rgba(0,0,0,0.08); }
.card-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(280px, 1fr)); gap: 1rem; }
/* === Forms === */
input, textarea, select { width: 100%; padding: 0.5rem 0.75rem; border: 1px solid #ccc;
                           border-radius: 4px; font-size: 1rem; font-family: inherit; }
input:focus, textarea:focus, select:focus { outline: none; border-color: #0066cc;
                                             box-shadow: 0 0 0 3px rgba(0,102,204,0.15); }
label { display: block; font-weight: 500; margin-bottom: 0.25rem; }
.form-group { margin-bottom: 1rem; }
button, .btn { display: inline-flex; align-items: center; gap: 0.5rem; padding: 0.5rem 1rem;
               border: none; border-radius: 4px; font-size: 1rem; font-weight: 500;
               cursor: pointer; transition: background 0.2s; }
.btn-primary { background: #0066cc; color: #fff; }
.btn-primary:hover { background: #0052a3; }
";

const JSON_1KB: &[u8] = b"{
  \"id\": 42,
  \"slug\": \"understanding-rust-lifetimes\",
  \"title\": \"Understanding Rust Lifetimes\",
  \"author\": { \"id\": 1, \"name\": \"Alice\", \"email\": \"alice@example.com\" },
  \"tags\": [\"rust\", \"programming\", \"memory-safety\"],
  \"published_at\": \"2024-01-15T09:00:00Z\",
  \"updated_at\": \"2024-01-16T14:30:00Z\",
  \"summary\": \"A deep dive into Rust lifetime annotations and the borrow checker.\",
  \"read_time_minutes\": 8,
  \"views\": 12483
}";

const JS_3KB: &[u8] = b"
// Navigation dropdown
(function () {
  'use strict';

  function initNavDropdowns() {
    var dropdowns = document.querySelectorAll('.nav-dropdown');
    dropdowns.forEach(function (dropdown) {
      var toggle = dropdown.querySelector('.nav-dropdown__toggle');
      var menu   = dropdown.querySelector('.nav-dropdown__menu');
      if (!toggle || !menu) { return; }

      toggle.addEventListener('click', function (event) {
        event.preventDefault();
        event.stopPropagation();
        var isOpen = dropdown.classList.contains('is-open');
        closeAllDropdowns();
        if (!isOpen) {
          dropdown.classList.add('is-open');
          menu.setAttribute('aria-expanded', 'true');
        }
      });
    });

    document.addEventListener('click', closeAllDropdowns);
    document.addEventListener('keydown', function (event) {
      if (event.key === 'Escape') { closeAllDropdowns(); }
    });
  }

  function closeAllDropdowns() {
    document.querySelectorAll('.nav-dropdown.is-open').forEach(function (el) {
      el.classList.remove('is-open');
      var menu = el.querySelector('.nav-dropdown__menu');
      if (menu) { menu.setAttribute('aria-expanded', 'false'); }
    });
  }

  function initThemeToggle() {
    var btn = document.querySelector('#theme-toggle');
    if (!btn) { return; }
    var stored = localStorage.getItem('theme') || 'light';
    document.documentElement.setAttribute('data-theme', stored);
    btn.addEventListener('click', function () {
      var current = document.documentElement.getAttribute('data-theme');
      var next    = current === 'dark' ? 'light' : 'dark';
      document.documentElement.setAttribute('data-theme', next);
      localStorage.setItem('theme', next);
    });
  }

  function initCopyButtons() {
    document.querySelectorAll('pre').forEach(function (pre) {
      var btn = document.createElement('button');
      btn.className = 'copy-btn';
      btn.textContent = 'Copy';
      btn.addEventListener('click', function () {
        var code = pre.querySelector('code');
        if (!code) { return; }
        navigator.clipboard.writeText(code.textContent).then(function () {
          btn.textContent = 'Copied!';
          setTimeout(function () { btn.textContent = 'Copy'; }, 2000);
        });
      });
      pre.style.position = 'relative';
      pre.appendChild(btn);
    });
  }

  document.addEventListener('DOMContentLoaded', function () {
    initNavDropdowns();
    initThemeToggle();
    initCopyButtons();
  });
}());";

// ── Fixtures ──────────────────────────────────────────────────────────────────

/// Minimal Tera template — one variable substitution.
const MINIMAL_TEMPLATE: &str =
    "<!doctype html><html><body><h1>{{ title }}</h1></body></html>";

fn make_routes() -> Vec<CompiledRoute> {
    let patterns = &[
        ("/", RouteMethod::Any),
        ("/about", RouteMethod::Get),
        ("/blog/{stem}", RouteMethod::Get),
        ("/api/v1/posts/{id}", RouteMethod::Any),
    ];
    let mut routes: Vec<CompiledRoute> = patterns
        .iter()
        .map(|(pat, method)| {
            let segs = compile_pattern(pat);
            let spec = route_specificity(&segs);
            CompiledRoute {
                pattern: pat.to_string(),
                method: method.clone(),
                segments: segs,
                template: None,
                params_files: vec![],
                status: 200,
                cache: "public".to_string(),
                specificity: spec,
            }
        })
        .collect();
    routes.sort_by(|a, b| b.specificity.cmp(&a.specificity));
    routes
}


// ── Percentile reporter ────────────────────────────────────────────────────────

fn report_percentiles<F: FnMut()>(label: &str, n: usize, mut f: F) {
    let warmup = n / 10;
    for _ in 0..warmup {
        f();
    }
    let mut samples: Vec<u64> = Vec::with_capacity(n);
    for _ in 0..n {
        let t0 = std::time::Instant::now();
        f();
        samples.push(t0.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    let count = samples.len() as f64;
    let p = |pct: f64| -> u64 {
        let idx = ((pct / 100.0) * count) as usize;
        samples[idx.min(samples.len() - 1)]
    };
    let avg = samples.iter().sum::<u64>() as f64 / count;
    let variance = samples
        .iter()
        .map(|&x| {
            let d = x as f64 - avg;
            d * d
        })
        .sum::<f64>()
        / count;
    let stddev = variance.sqrt();
    println!(
        "\n── {label} (n={n}) ─────────────────────────────────────────────\n\
         p0={p0}ns  p1={p1}ns  p50={p50}ns  p99={p99}ns  p100={p100}ns\n\
         avg={avg:.1}ns  stddev={stddev:.1}ns",
        p0 = p(0.0),
        p1 = p(1.0),
        p50 = p(50.0),
        p99 = p(99.0),
        p100 = p(100.0),
    );
}

// ── Benchmarks ────────────────────────────────────────────────────────────────

/// compile_pattern: convert a route string to segments.
fn bench_compile_pattern(c: &mut Criterion) {
    let mut group = c.benchmark_group("compile_pattern");
    group.sample_size(1_000);
    group.bench_function("compile_pattern", |b| {
        b.iter(|| black_box(compile_pattern(black_box("/blog/{stem}"))))
    });
    group.finish();
}

/// find_route: scan 4 routes to find the best match.
fn bench_find_route(c: &mut Criterion) {
    let routes = make_routes();
    let mut group = c.benchmark_group("find_route");
    group.sample_size(1_000);
    group.bench_function("find_route_param", |b| {
        b.iter(|| black_box(find_route(black_box("/blog/hello-world"), "GET", &routes)))
    });
    group.bench_function("find_route_exact", |b| {
        b.iter(|| black_box(find_route(black_box("/about"), "GET", &routes)))
    });
    group.finish();
}

/// match_route: single-route match with a parameterised pattern.
fn bench_match_route(c: &mut Criterion) {
    let segs = compile_pattern("/blog/{stem}");
    let route = CompiledRoute {
        pattern: "/blog/{stem}".to_string(),
        method: RouteMethod::Get,
        segments: segs.clone(),
        template: None,
        params_files: vec![],
        status: 200,
        cache: "public".to_string(),
        specificity: route_specificity(&segs),
    };
    let path_segs: Vec<&str> = "/blog/hello-world"
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    let mut group = c.benchmark_group("match_route");
    group.sample_size(1_000);
    group.bench_function("match_route", |b| {
        b.iter(|| black_box(match_route(black_box(&path_segs), black_box(&route))))
    });
    group.finish();
}

/// parse_query_string: parse a typical query string.
fn bench_parse_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_query");
    group.sample_size(1_000);
    group.bench_function("parse_query", |b| {
        b.iter(|| {
            black_box(parse_query_string(black_box(
                "page=2&sort=date&tag=rust&limit=10",
            )))
        })
    });
    group.finish();
}

/// parse_cookies: parse a typical cookie header.
fn bench_parse_cookies(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_cookies");
    group.sample_size(1_000);
    group.bench_function("parse_cookies", |b| {
        b.iter(|| {
            black_box(parse_cookies(black_box(
                "session=abc123; theme=dark; lang=en",
            )))
        })
    });
    group.finish();
}

/// Tera template rendering: render a minimal template with one variable.
fn bench_template_render(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let tmpl_path = dir.path().join("page.html");
    std::fs::write(&tmpl_path, MINIMAL_TEMPLATE).unwrap();

    let mut tera = tera::Tera::default();
    tera.add_template_file(&tmpl_path, Some("page.html"))
        .unwrap();

    let mut ctx = tera::Context::new();
    ctx.insert("title", "Hello, World!");

    let mut group = c.benchmark_group("template_render");
    group.sample_size(1_000);
    group.bench_function("template_render", |b| {
        b.iter(|| black_box(tera.render(black_box("page.html"), black_box(&ctx)).unwrap()))
    });
    group.finish();
}

/// Response::write_to: serialise a pre-built HTML response to a Vec.
fn bench_response_write(c: &mut Criterion) {
    let resp = {
        let mut r = Response::status(200);
        r.headers.push(("Content-Type".to_string(), "text/html; charset=utf-8".to_string()));
        r.headers.push(("Cache-Control".to_string(), "public".to_string()));
        r.body = MINIMAL_TEMPLATE.as_bytes().to_vec();
        r
    };
    let mut group = c.benchmark_group("response_write");
    group.sample_size(1_000);
    group.bench_function("response_write", |b| {
        b.iter(|| {
            let mut buf = Vec::with_capacity(512);
            black_box(resp.write_to(&mut buf).unwrap());
        })
    });
    group.finish();
}

/// Unix socket round-trip: full HTTP/1.1 request → rendered HTML response.
///
/// A minimal m6-render server runs in a background thread handling one route
/// (/blog/{stem}) that renders a minimal template.  This measures the total
/// backend latency that m6-http would observe for a cache-miss forwarded to
/// m6-render.
fn bench_socket_round_trip(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();

    // Write template.
    let tmpl_dir = dir.path().join("templates");
    std::fs::create_dir_all(&tmpl_dir).unwrap();
    std::fs::write(tmpl_dir.join("page.html"), MINIMAL_TEMPLATE).unwrap();

    // Write minimal config.
    let cfg_text = format!(
        r#"
[[route]]
path     = "/blog/{{stem}}"
template = "templates/page.html"

[thread_pool]
size       = 1
queue_size = 64

[params_cache]
size = 64
"#
    );
    let cfg_path = dir.path().join("m6.toml");
    std::fs::write(&cfg_path, &cfg_text).unwrap();

    // Unique socket path.
    let sock_path = dir.path().join("render.sock");
    let sock_env = sock_path.to_str().unwrap().to_string();

    let site_dir = dir.path().to_path_buf();
    let cfg_path2 = cfg_path.clone();

    std::thread::spawn(move || {
        // We can't call run_app() (it reads env args), so we replicate the
        // minimal setup needed: build FrameworkState, accept on the socket.
        // Instead, build a raw server using the public server + app modules.
        let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

        let config = m6_render::config::load(&cfg_path2, &site_dir).unwrap();

        // Build Tera directly.
        let tmpl_path = site_dir.join("templates/page.html");
        let mut tera = tera::Tera::default();
        tera.add_template_file(&tmpl_path, Some("templates/page.html"))
            .unwrap();

        let routes: Vec<CompiledRoute> = config
            .routes
            .iter()
            .map(|rc| {
                let segs = compile_pattern(&rc.path);
                let spec = route_specificity(&segs);
                CompiledRoute {
                    pattern: rc.path.clone(),
                    method: RouteMethod::Any,
                    segments: segs,
                    template: rc.template.clone(),
                    params_files: rc.params.clone(),
                    status: rc.status,
                    cache: rc.cache.clone(),
                    specificity: spec,
                }
            })
            .collect();

        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => break,
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .ok();

            let raw = match m6_render::server::parse_request(&mut stream) {
                Ok(Some(r)) => r,
                _ => continue,
            };

            let resp = if let Some((route, params)) =
                find_route(raw.path(), raw.method(), &routes)
            {
                // Build a minimal dict with path params only.
                let mut dict = Map::new();
                for (k, v) in &params {
                    dict.insert(k.clone(), Value::String(v.clone()));
                }
                // Provide a "title" for the template.
                dict.insert(
                    "title".to_string(),
                    Value::String(format!("Post: {}", raw.path())),
                );

                if let Some(template_name) = &route.template {
                    let mut ctx = tera::Context::new();
                    for (k, v) in &dict {
                        ctx.insert(k.as_str(), v);
                    }
                    match tera.render(template_name, &ctx) {
                        Ok(html) => html_response(html),
                        Err(_) => Response::status(500),
                    }
                } else {
                    Response::not_found()
                }
            } else {
                Response::not_found()
            };

            write_response(&mut stream, &resp).ok();
        }
    });

    // Wait for server to start.
    std::thread::sleep(std::time::Duration::from_millis(20));

    const RAW_REQ: &[u8] =
        b"GET /blog/hello-world HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

    let mut group = c.benchmark_group("socket_round_trip");
    group.sample_size(1_000);
    group.bench_function("socket_round_trip", |b| {
        b.iter(|| {
            let mut conn = UnixStream::connect(&sock_env).unwrap();
            conn.write_all(RAW_REQ).unwrap();
            use std::io::Read;
            let mut buf = Vec::with_capacity(512);
            conn.read_to_end(&mut buf).unwrap();
            black_box(buf.len())
        })
    });
    group.finish();
    drop(cfg_text);
}

// ── Compression benchmarks ────────────────────────────────────────────────────

fn bench_compress(c: &mut Criterion) {
    let mut group = c.benchmark_group("compress");
    group.sample_size(500);

    // Brotli levels: 1 (fast), 6 (default), 11 (max)
    for level in [1u32, 6, 11] {
        let label = format!("brotli_html_2kb_level{level}");
        group.bench_function(&label, |b| {
            b.iter(|| black_box(brotli_compress(black_box(HTML_2KB), level).unwrap()))
        });
    }
    for level in [1u32, 6, 11] {
        let label = format!("brotli_css_8kb_level{level}");
        group.bench_function(&label, |b| {
            b.iter(|| black_box(brotli_compress(black_box(CSS_8KB), level).unwrap()))
        });
    }
    // Gzip levels: 1 (fast), 6 (default), 9 (max)
    for level in [1u32, 6, 9] {
        let label = format!("gzip_html_2kb_level{level}");
        group.bench_function(&label, |b| {
            b.iter(|| black_box(gzip_compress(black_box(HTML_2KB), level).unwrap()))
        });
    }
    for level in [1u32, 6, 9] {
        let label = format!("gzip_css_8kb_level{level}");
        group.bench_function(&label, |b| {
            b.iter(|| black_box(gzip_compress(black_box(CSS_8KB), level).unwrap()))
        });
    }
    group.finish();
}

// ── Minification benchmarks ───────────────────────────────────────────────────

fn bench_minify(c: &mut Criterion) {
    let mut group = c.benchmark_group("minify");
    group.sample_size(500);
    group.bench_function("minify_html_2kb",   |b| b.iter(|| black_box(minify_html(black_box(HTML_2KB)))));
    group.bench_function("minify_css_8kb",    |b| b.iter(|| black_box(minify_css(black_box(CSS_8KB)))));
    group.bench_function("minify_json_1kb",   |b| b.iter(|| black_box(minify_json(black_box(JSON_1KB)))));
    group.bench_function("minify_js_3kb",     |b| b.iter(|| black_box(minify_js(black_box(JS_3KB)))));
    group.finish();
}

// ── Minify + compress pipeline ────────────────────────────────────────────────

fn bench_minify_then_compress(c: &mut Criterion) {
    let mut group = c.benchmark_group("minify_then_compress");
    group.sample_size(200);
    group.bench_function("html_minify_then_brotli6", |b| {
        b.iter(|| {
            let minified = minify_html(black_box(HTML_2KB));
            black_box(brotli_compress(&minified, 6).unwrap())
        })
    });
    group.bench_function("css_minify_then_brotli6", |b| {
        b.iter(|| {
            let minified = minify_css(black_box(CSS_8KB));
            black_box(brotli_compress(&minified, 6).unwrap())
        })
    });
    group.bench_function("js_minify_then_brotli6", |b| {
        b.iter(|| {
            let minified = minify_js(black_box(JS_3KB));
            black_box(brotli_compress(&minified, 6).unwrap())
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_compile_pattern,
    bench_find_route,
    bench_match_route,
    bench_parse_query,
    bench_parse_cookies,
    bench_template_render,
    bench_response_write,
    bench_socket_round_trip,
    bench_compress,
    bench_minify,
    bench_minify_then_compress,
);

// ── Custom main: criterion + raw percentile report ────────────────────────────

fn main() {
    benches();

    const N: usize = 100_000;
    const N_SLOW: usize = 10_000;

    println!(
        "\n\
        ════════════════════════════════════════════════════════════════\n\
        m6-render  Raw percentile report  (release mode)\n\
        ════════════════════════════════════════════════════════════════"
    );

    let routes = make_routes();

    report_percentiles("compile_pattern", N, || {
        black_box(compile_pattern(black_box("/blog/{stem}")));
    });

    report_percentiles("find_route (param)", N, || {
        black_box(find_route(black_box("/blog/hello-world"), "GET", &routes));
    });

    report_percentiles("find_route (exact)", N, || {
        black_box(find_route(black_box("/about"), "GET", &routes));
    });

    {
        let segs = compile_pattern("/blog/{stem}");
        let route = CompiledRoute {
            pattern: "/blog/{stem}".to_string(),
            method: RouteMethod::Get,
            segments: segs.clone(),
            template: None,
            params_files: vec![],
            status: 200,
            cache: "public".to_string(),
            specificity: route_specificity(&segs),
        };
        let path_segs: Vec<&str> = "/blog/hello-world"
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        report_percentiles("match_route", N, || {
            black_box(match_route(&path_segs, &route));
        });
    }

    report_percentiles("parse_query_string", N, || {
        black_box(parse_query_string(black_box(
            "page=2&sort=date&tag=rust&limit=10",
        )));
    });

    report_percentiles("parse_cookies", N, || {
        black_box(parse_cookies(black_box(
            "session=abc123; theme=dark; lang=en",
        )));
    });

    // Template render
    {
        let dir = TempDir::new().unwrap();
        let tmpl_path = dir.path().join("page.html");
        std::fs::write(&tmpl_path, MINIMAL_TEMPLATE).unwrap();
        let mut tera = tera::Tera::default();
        tera.add_template_file(&tmpl_path, Some("page.html"))
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("title", "Hello, World!");
        report_percentiles("template_render", N, || {
            black_box(tera.render(black_box("page.html"), black_box(&ctx)).unwrap());
        });
    }

    // Response serialisation
    {
        let resp = {
            let mut r = Response::status(200);
            r.headers.push(("Content-Type".to_string(), "text/html; charset=utf-8".to_string()));
            r.headers.push(("Cache-Control".to_string(), "public".to_string()));
            r.body = MINIMAL_TEMPLATE.as_bytes().to_vec();
            r
        };
        report_percentiles("response_write", N, || {
            let mut buf = Vec::with_capacity(512);
            black_box(resp.write_to(&mut buf).unwrap());
        });
    }

    // Socket round-trip
    {
        let dir2 = TempDir::new().unwrap();
        let tmpl_dir2 = dir2.path().join("templates");
        std::fs::create_dir_all(&tmpl_dir2).unwrap();
        std::fs::write(tmpl_dir2.join("page.html"), MINIMAL_TEMPLATE).unwrap();

        let cfg_text2 = r#"
[[route]]
path     = "/blog/{stem}"
template = "templates/page.html"

[thread_pool]
size       = 1
queue_size = 64

[params_cache]
size = 64
"#;
        let cfg_path2 = dir2.path().join("m6.toml");
        std::fs::write(&cfg_path2, cfg_text2).unwrap();

        let sock_path2 = dir2.path().join("render2.sock");
        let sock_str2 = sock_path2.to_str().unwrap().to_string();
        let site_dir2 = dir2.path().to_path_buf();

        std::thread::spawn(move || {
            let listener = std::os::unix::net::UnixListener::bind(&sock_path2).unwrap();
            let config = m6_render::config::load(&cfg_path2, &site_dir2).unwrap();
            let tmpl_path = site_dir2.join("templates/page.html");
            let mut tera = tera::Tera::default();
            tera.add_template_file(&tmpl_path, Some("templates/page.html"))
                .unwrap();
            let routes2: Vec<CompiledRoute> = config
                .routes
                .iter()
                .map(|rc| {
                    let segs = compile_pattern(&rc.path);
                    let spec = route_specificity(&segs);
                    CompiledRoute {
                        pattern: rc.path.clone(),
                        method: RouteMethod::Any,
                        segments: segs,
                        template: rc.template.clone(),
                        params_files: rc.params.clone(),
                        status: rc.status,
                        cache: rc.cache.clone(),
                        specificity: spec,
                    }
                })
                .collect();
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .ok();
                let raw = match m6_render::server::parse_request(&mut stream) {
                    Ok(Some(r)) => r,
                    _ => continue,
                };
                let resp = if let Some((route, params)) =
                    find_route(raw.path(), raw.method(), &routes2)
                {
                    let mut dict = Map::new();
                    for (k, v) in &params {
                        dict.insert(k.clone(), Value::String(v.clone()));
                    }
                    dict.insert(
                        "title".to_string(),
                        Value::String(format!("Post: {}", raw.path())),
                    );
                    if let Some(tmpl_name) = &route.template {
                        let mut ctx = tera::Context::new();
                        for (k, v) in &dict {
                            ctx.insert(k.as_str(), v);
                        }
                        match tera.render(tmpl_name, &ctx) {
                            Ok(html) => html_response(html),
                            Err(_) => Response::status(500),
                        }
                    } else {
                        Response::not_found()
                    }
                } else {
                    Response::not_found()
                };
                write_response(&mut stream, &resp).ok();
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(20));

        const RAW_REQ: &[u8] =
            b"GET /blog/hello-world HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        report_percentiles("socket_round_trip (end-to-end, render)", N_SLOW, || {
            let mut conn = UnixStream::connect(&sock_str2).unwrap();
            conn.write_all(RAW_REQ).unwrap();
            use std::io::Read;
            let mut buf = Vec::with_capacity(512);
            conn.read_to_end(&mut buf).unwrap();
            black_box(buf.len());
        });
    }

    // ── Compression ──────────────────────────────────────────────────────────
    const N_COMP: usize = 2_000;
    println!("\n── Compression paths (n={N_COMP}) ──────────────────────────────────────────");

    for level in [1u32, 6, 11] {
        report_percentiles(&format!("brotli html 2KB level={level}"), N_COMP, || {
            black_box(brotli_compress(black_box(HTML_2KB), level).unwrap());
        });
    }
    for level in [1u32, 6, 9] {
        report_percentiles(&format!("gzip html 2KB level={level}"), N_COMP, || {
            black_box(gzip_compress(black_box(HTML_2KB), level).unwrap());
        });
    }
    for level in [1u32, 6, 11] {
        report_percentiles(&format!("brotli css 8KB level={level}"), N_COMP, || {
            black_box(brotli_compress(black_box(CSS_8KB), level).unwrap());
        });
    }
    for level in [1u32, 6, 9] {
        report_percentiles(&format!("gzip css 8KB level={level}"), N_COMP, || {
            black_box(gzip_compress(black_box(CSS_8KB), level).unwrap());
        });
    }

    // ── Minification ─────────────────────────────────────────────────────────
    const N_MIN: usize = 5_000;
    println!("\n── Minification paths (n={N_MIN}) ───────────────────────────────────────────");
    report_percentiles("minify html 2KB",  N_MIN, || { black_box(minify_html(black_box(HTML_2KB))); });
    report_percentiles("minify css 8KB",   N_MIN, || { black_box(minify_css(black_box(CSS_8KB))); });
    report_percentiles("minify json 1KB",  N_MIN, || { black_box(minify_json(black_box(JSON_1KB))); });
    report_percentiles("minify js 3KB",    N_MIN, || { black_box(minify_js(black_box(JS_3KB))); });

    // ── Minify + compress pipeline ────────────────────────────────────────────
    const N_PIPE: usize = 2_000;
    println!("\n── Minify+compress pipeline (n={N_PIPE}) ────────────────────────────────────");
    report_percentiles("html: minify → brotli-6", N_PIPE, || {
        let m = minify_html(black_box(HTML_2KB));
        black_box(brotli_compress(&m, 6).unwrap());
    });
    report_percentiles("css:  minify → brotli-6", N_PIPE, || {
        let m = minify_css(black_box(CSS_8KB));
        black_box(brotli_compress(&m, 6).unwrap());
    });
    report_percentiles("js:   minify → brotli-6", N_PIPE, || {
        let m = minify_js(black_box(JS_3KB));
        black_box(brotli_compress(&m, 6).unwrap());
    });

    println!("\n════════════════════════════════════════════════════════════════\n");
}
