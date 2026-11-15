//! The cached HTTP/1.1 hit path, proved allocation-free.
//!
//! `harness = false` means no libtest thread runs in this binary, so the
//! counting allocator reads only what the two checks here allocated. The first
//! check runs `parse_head`, a cache lookup and `write_cached_h1` one thousand
//! times and requires zero allocations; the second checks the bytes against the
//! owned writer's header set. The harness speaks libtest's protocol through
//! `libtest-mimic`, forced onto one thread so it runs in this `main` thread.

use std::sync::Arc;

use libtest_mimic::{Arguments, Failed, Trial};
use m6_core::h1::{parse_head, HeadResult};
use m6_core::testkit::alloc_count;
use m6_http_lib::cache::{
    make_lookup_key, Cache, CacheKey, CachedResponse, Lookup, RequestDirectives,
};
use m6_http_lib::http11::status_may_have_content_length;
use m6_http_lib::http11::{write_cached_h1, HitHeaders};

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

const BODY_LEN: usize = 20 * 1024;

fn main() {
    let mut args = Arguments::from_args();
    // One thread: `libtest-mimic` then runs every trial in this (the main)
    // thread, so the counting allocator reads only the trial's own
    // allocations and no other thread's.
    args.test_threads = Some(1);
    let tests = vec![
        Trial::test("a_cached_h1_hit_allocates_nothing", || {
            let f = fixtures();
            outcome(
                "a_cached_h1_hit_allocates_nothing",
                a_cached_h1_hit_allocates_nothing(&f.cache, &f.entry, &f.hit, f.request),
            )
        }),
        Trial::test("the_hit_bytes_match_build_response", || {
            let f = fixtures();
            outcome(
                "the_hit_bytes_match_build_response",
                the_hit_bytes_match_build_response(&f.cache, &f.entry, &f.hit, f.request),
            )
        }),
    ];
    libtest_mimic::run(&args, tests).exit();
}

/// A cache with one fresh entry, the hit headers and the request, built
/// outside every counted region.
struct Fixtures {
    cache: Cache,
    entry: CachedResponse,
    hit: HitHeaders,
    request: &'static [u8],
}

fn fixtures() -> Fixtures {
    let cache = Cache::new();
    let headers: Vec<(String, String)> = vec![
        (
            "content-type".to_string(),
            "text/html; charset=utf-8".to_string(),
        ),
        (
            "cache-control".to_string(),
            "public, max-age=3600".to_string(),
        ),
        ("etag".to_string(), "\"abc123\"".to_string()),
        (
            "last-modified".to_string(),
            "Mon, 01 Jan 2024 00:00:00 GMT".to_string(),
        ),
        (
            "date".to_string(),
            "Mon, 01 Jan 2024 00:00:00 GMT".to_string(),
        ),
        ("server".to_string(), "m6".to_string()),
        ("x-powered-by".to_string(), "m6".to_string()),
        ("content-language".to_string(), "en".to_string()),
    ];
    let entry = CachedResponse {
        genid: 0,
        status: 200,
        headers: m6_http_lib::cache::HeaderBlock::from_pairs(headers.clone()),
        body: bytes::Bytes::from(vec![b'x'; BODY_LEN]),
    };
    cache.insert(CacheKey::new("/", None, "gzip"), entry.clone());

    let hit = HitHeaders {
        alt_svc: Arc::<[u8]>::from(&b"alt-svc: h3=\":443\"; ma=86400"[..]),
        describedby: Some(Arc::<[u8]>::from(
            &b"link: </llms.txt>; rel=\"describedby\""[..],
        )),
    };

    let request = b"GET / HTTP/1.1\r\n\
Host: example.com\r\n\
User-Agent: curl/8.0\r\n\
Accept: */*\r\n\
Accept-Encoding: gzip\r\n\
Accept-Language: en\r\n\
Cookie: _m6sid=abc123\r\n\
\r\n";

    Fixtures {
        cache,
        entry,
        hit,
        request,
    }
}

fn outcome(name: &str, failed: bool) -> Result<(), Failed> {
    if failed {
        Err(format!("{name}: failed").into())
    } else {
        Ok(())
    }
}

fn a_cached_h1_hit_allocates_nothing(
    cache: &Cache,
    entry: &CachedResponse,
    hit: &HitHeaders,
    request: &[u8],
) -> bool {
    let mut key_buf = String::with_capacity(512);
    let mut out = Vec::with_capacity(64 * 1024);
    let start = alloc_count::now();
    for _ in 0..1_000 {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let HeadResult::Complete(head) = parse_head(request, &mut headers) else {
            println!("a_cached_h1_hit_allocates_nothing: request did not parse");
            return true;
        };
        let enc = m6_core::http::header(head.headers, "accept-encoding").unwrap_or("");
        let key = make_lookup_key(head.path, head.query, enc, &mut key_buf);
        let req_cc = RequestDirectives::default();
        match cache.lookup_with(key, &req_cc) {
            Lookup::Fresh(got, age) | Lookup::Stale(got, age) => {
                write_cached_h1(&mut out, &got, age, hit, None, head.method, true);
            }
            Lookup::Miss => {
                println!("a_cached_h1_hit_allocates_nothing: cache missed");
                return true;
            }
        }
        out.clear();
    }
    let n = alloc_count::now() - start;
    println!("a_cached_h1_hit_allocates_nothing: {n} allocation(s) in 1000 hits");
    let _ = entry;
    n != 0
}

fn the_hit_bytes_match_build_response(
    cache: &Cache,
    entry: &CachedResponse,
    hit: &HitHeaders,
    request: &[u8],
) -> bool {
    let mut key_buf = String::with_capacity(512);
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let HeadResult::Complete(head) = parse_head(request, &mut headers) else {
        println!("the_hit_bytes_match_build_response: request did not parse");
        return true;
    };
    let enc = m6_core::http::header(head.headers, "accept-encoding").unwrap_or("");
    let key = make_lookup_key(head.path, head.query, enc, &mut key_buf);
    let req_cc = RequestDirectives::default();
    let (Lookup::Fresh(got, age) | Lookup::Stale(got, age)) = cache.lookup_with(key, &req_cc)
    else {
        println!("the_hit_bytes_match_build_response: cache missed");
        return true;
    };

    let mut out = Vec::new();
    write_cached_h1(&mut out, &got, age, hit, None, head.method, true);

    // Parse the written response.
    let mut resp_headers = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut resp_headers);
    let body_start = match resp.parse(&out) {
        Ok(httparse::Status::Complete(n)) => n,
        _ => {
            println!("the_hit_bytes_match_build_response: response did not parse");
            return true;
        }
    };
    if resp.code != Some(200) {
        println!(
            "the_hit_bytes_match_build_response: wrong status {:?}",
            resp.code
        );
        return true;
    }
    if out[body_start..] != entry.body[..] {
        println!("the_hit_bytes_match_build_response: body mismatch");
        return true;
    }

    // The header set the owned writer produces: the cached headers, then the
    // four headers this writer emits, then content-length and connection.
    let mut expected: Vec<(String, String)> = entry.headers.to_pairs();
    expected.push(("alt-svc".to_string(), "h3=\":443\"; ma=86400".to_string()));
    expected.push(("vary".to_string(), "Accept-Encoding".to_string()));
    expected.push(("age".to_string(), age.as_secs().to_string()));
    expected.push((
        "link".to_string(),
        "</llms.txt>; rel=\"describedby\"".to_string(),
    ));
    expected.push(("m6-genid".to_string(), entry.genid.to_string()));
    if status_may_have_content_length(200) {
        expected.push(("content-length".to_string(), entry.body.len().to_string()));
    }
    expected.push(("connection".to_string(), "keep-alive".to_string()));

    let mut actual: Vec<(String, String)> = resp
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_ascii_lowercase(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect();
    let mut expected = expected
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v))
        .collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    if actual != expected {
        println!("the_hit_bytes_match_build_response: header set mismatch");
        println!("  expected: {expected:?}");
        println!("  actual:   {actual:?}");
        return true;
    }
    println!("the_hit_bytes_match_build_response: ok");
    false
}
