/// In-memory response cache with atomic Arc swap.
use std::borrow::Borrow;
use ahash::AHashMap;
use crate::analytics::{header, HeaderSource};

/// A cached HTTP response.
///
/// `headers` and `hints` are `Arc<Vec<...>>` so clone is a single atomic
/// refcount bump — no string copies on the cache-hit hot path.
///
/// `hints` contains absolute-path URLs extracted from the response body on the
/// first (cache-miss) pass.  They are used to send `103 Early Hints` on every
/// subsequent request, including cache hits, without re-scanning the body.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: std::sync::Arc<Vec<(String, String)>>,
    pub body:    bytes::Bytes,
    pub hints:   std::sync::Arc<Vec<String>>,
}

/// True if the incoming request's `If-None-Match`/`If-Modified-Since` shows
/// the client's cached copy still matches this cached response's own
/// `ETag`/`Last-Modified` — i.e. a bodyless 304 should be sent instead of
/// replaying `body`. Mirrors the conditional-GET semantics already proven in
/// `m6-file/src/handler.rs`'s static-asset handling; this is the same check
/// for the proxy/cache layer, which previously replayed cached bodies
/// unconditionally regardless of what the client already had.
pub fn is_not_modified(
    cached_headers: &[(String, String)],
    req_headers: &(impl HeaderSource + ?Sized),
) -> bool {
    let etag = cached_headers.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("etag"))
        .map(|(_, v)| v.as_str());
    let last_modified = cached_headers.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("last-modified"))
        .map(|(_, v)| v.as_str());

    if let Some(inm) = header(req_headers, "if-none-match") {
        // A real client sends exactly one ETag here, but the grammar allows
        // a comma-separated list (and "*"), so honor that.
        return match etag {
            Some(etag) => inm == "*" || inm.split(',').any(|tag| tag.trim() == etag),
            None => false,
        };
    }
    if let (Some(ims), Some(lm)) = (header(req_headers, "if-modified-since"), last_modified) {
        // HTTP-date has 1-second resolution; compare at that resolution too
        // so a cached entry that hasn't changed since the client's copy
        // doesn't spuriously look "modified" from sub-second noise.
        if let (Ok(req_time), Ok(cached_time)) =
            (httpdate::parse_http_date(ims), httpdate::parse_http_date(lm))
        {
            let secs = |t: std::time::SystemTime| {
                t.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs()
            };
            return secs(req_time) >= secs(cached_time);
        }
    }
    false
}

/// Build the minimal header set for a 304 response derived from a cached
/// entry's headers — just the validators a client needs to keep using its
/// cached copy, not the full header set (no Content-Type/Content-Encoding
/// on a bodyless response).
pub fn not_modified_headers(cached_headers: &[(String, String)]) -> Vec<(String, String)> {
    cached_headers.iter()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            k == "etag" || k == "last-modified" || k == "cache-control"
        })
        .cloned()
        .collect()
}

/// Owned cache key stored in the HashMap.
///
/// Internally stored as `path\x01query\x01encoding` in a single heap
/// allocation. Implements `Borrow<str>` so the map can be looked up with a
/// plain `&str`.
///
/// The query string is part of the key. It used to be stripped, which meant
/// `/search?q=a` and `/search?q=b` shared one entry and whichever ran first
/// was served to everyone.
#[derive(Debug, Hash, Eq, PartialEq)]
pub struct CacheKey(Box<str>);

impl CacheKey {
    /// Create an owned key (called only on cache INSERT, not lookup).
    pub fn new(path: &str, query: Option<&str>, content_encoding: &str) -> Self {
        // Defensive: callers pass an already-split path, but a stray `?` must
        // never smuggle one request's query into another's key.
        let path_stripped = &path[..path.find('?').unwrap_or(path.len())];
        let query = query.unwrap_or("");
        let mut s = String::with_capacity(
            path_stripped.len() + query.len() + content_encoding.len() + 2,
        );
        s.push_str(path_stripped);
        s.push('\x01');
        s.push_str(query);
        s.push('\x01');
        s.push_str(content_encoding);
        CacheKey(s.into_boxed_str())
    }
}

impl Clone for CacheKey {
    fn clone(&self) -> Self {
        CacheKey(self.0.clone())
    }
}

/// `Borrow<str>` makes `HashMap<CacheKey, _>::get(&str)` work without allocation.
///
/// SAFETY (soundness): `str` has the same Hash and Eq semantics as the inner
/// `Box<str>`, so the `Borrow` contract (consistent Hash/Eq) is upheld.
impl Borrow<str> for CacheKey {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Build a zero-allocation lookup key into a caller-supplied stack buffer.
///
/// Returns a `&str` slice into `buf` that equals the `CacheKey` for the given
/// `path` + `query` + `encoding` triple. If the combined length exceeds 512
/// bytes an empty string is returned, which never matches a stored key — so an
/// over-long request simply misses the cache rather than colliding with an
/// unrelated entry.
///
/// `buf` must be a `&mut [u8; 512]` on the caller's stack.
pub fn make_lookup_key<'a>(
    path: &str,
    query: Option<&str>,
    encoding: &str,
    buf: &'a mut [u8; 512],
) -> &'a str {
    let path_stripped = &path[..path.find('?').unwrap_or(path.len())];
    let query = query.unwrap_or("");
    let needed = path_stripped.len() + query.len() + encoding.len() + 2;
    if needed <= buf.len() {
        let mut at = 0;
        buf[at..at + path_stripped.len()].copy_from_slice(path_stripped.as_bytes());
        at += path_stripped.len();
        buf[at] = b'\x01';
        at += 1;
        buf[at..at + query.len()].copy_from_slice(query.as_bytes());
        at += query.len();
        buf[at] = b'\x01';
        at += 1;
        buf[at..at + encoding.len()].copy_from_slice(encoding.as_bytes());
        // SAFETY: `path_stripped`, `query` and `encoding` are valid UTF-8 (they
        // came from `&str`), and the separator byte `\x01` is valid ASCII. The
        // slice `buf[..needed]` is therefore valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(&buf[..needed]) }
    } else {
        // Fallback: key longer than 512 bytes — miss the cache.
        ""
    }
}

/// The inner cache map.
/// Freshness lifetime from a response's own `Cache-Control`, in seconds.
///
/// `s-maxage` wins over `max-age` when both are present: this is a shared
/// cache, and that is exactly what `s-maxage` is for. `no-cache` means the
/// response must be revalidated before every reuse — m6-http has no upstream
/// revalidation path, so the honest equivalent is a zero lifetime (always a
/// miss) rather than serving it unrevalidated.
///
/// `None` means no freshness lifetime was specified, which keeps the previous
/// behaviour: the entry lives until an explicit invalidation. That is a
/// deliberate CDN-style model (see `evict_path`/`clear` and
/// deploy/invalidate-cache.sh), not an oversight — this only adds expiry for
/// responses that actually asked for one.
fn freshness_secs(headers: &[(String, String)]) -> Option<u64> {
    let cc = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("cache-control"))?
        .1
        .to_ascii_lowercase();

    if cc.split(',').any(|d| d.trim() == "no-cache") {
        return Some(0);
    }

    // s-maxage first, then max-age.
    for directive in ["s-maxage", "max-age"] {
        for part in cc.split(',') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix(directive).and_then(|r| r.strip_prefix('=')) {
                if let Ok(secs) = v.trim().parse::<u64>() {
                    return Some(secs);
                }
            }
        }
    }
    None
}

/// A stored entry: the response plus the instant it goes stale.
///
/// `expires_at == None` means "no freshness lifetime given" — lives until
/// explicit invalidation.
struct CacheEntry {
    response: CachedResponse,
    expires_at: Option<std::time::Instant>,
}

type CacheMap = AHashMap<CacheKey, CacheEntry>;

use std::sync::{Arc, RwLock};

/// Cache backed by Arc<RwLock<HashMap>> — swap the whole map atomically.
/// Clone is cheap — just clones the Arc.
#[derive(Clone)]
pub struct Cache {
    map: Arc<RwLock<CacheMap>>,
}

impl Cache {
    pub fn new() -> Self {
        Cache { map: Arc::new(RwLock::new(CacheMap::new())) }
    }

    /// Get a cached response.
    ///
    /// Accepts any borrowed form of `CacheKey`:
    /// - `&str` — zero-allocation hot-path lookup via `make_lookup_key`
    /// - `&CacheKey` — legacy/test usage (blanket `Borrow<CacheKey>` impl)
    ///
    /// An entry past its freshness lifetime is treated as a miss. Expiry is
    /// checked here, on read, rather than swept by a background task: a stale
    /// entry costs nothing until someone asks for it, and the miss that
    /// follows overwrites it. This keeps expiry off the write path entirely
    /// and needs no timer thread.
    pub fn get<Q>(&self, key: &Q) -> Option<CachedResponse>
    where
        CacheKey: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let map = self.map.read().ok()?;
        let entry = map.get(key)?;
        match entry.expires_at {
            // Checked before the clone, so an expired entry costs no copy.
            Some(deadline) if std::time::Instant::now() >= deadline => None,
            _ => Some(entry.response.clone()),
        }
    }

    /// Store a response. Only call if the response should be cached.
    ///
    /// The freshness deadline is derived from the response's own
    /// `Cache-Control` here rather than passed in, so every caller gets
    /// correct expiry without having to know about it.
    pub fn insert(&self, key: CacheKey, response: CachedResponse) {
        let expires_at = freshness_secs(&response.headers)
            .map(|secs| std::time::Instant::now() + std::time::Duration::from_secs(secs));
        if let Ok(mut map) = self.map.write() {
            map.insert(key, CacheEntry { response, expires_at });
        }
    }

    /// Evict a specific path — every query and encoding variant of it.
    ///
    /// Keys are `path\x01query\x01encoding`, so the `path\x01` prefix matches
    /// all variants of that path and nothing else.
    pub fn evict_path(&self, path: &str) {
        if let Ok(mut map) = self.map.write() {
            let path_stripped = &path[..path.find('?').unwrap_or(path.len())];
            // Collect keys whose inner str starts with `path_stripped\x01`.
            let prefix = {
                let mut p = String::with_capacity(path_stripped.len() + 1);
                p.push_str(path_stripped);
                p.push('\x01');
                p
            };
            let keys: Vec<CacheKey> = map
                .keys()
                .filter(|k| {
                    let s: &str = (*k).borrow();
                    s.starts_with(prefix.as_str())
                })
                .cloned()
                .collect();
            for k in &keys {
                let s: &str = (*k).borrow();
                // Log the key's three parts for debugging.
                let mut parts = s.split('\x01');
                if let (Some(kpath), Some(query), Some(enc)) =
                    (parts.next(), parts.next(), parts.next())
                {
                    tracing::debug!(
                        path = %kpath, query = %query, encoding = %enc,
                        "cache: evicted"
                    );
                }
                map.remove(k);
            }
        }
    }

    /// Evict all entries for a list of paths.
    pub fn evict_paths(&self, paths: &[String]) {
        for path in paths {
            self.evict_path(path);
        }
    }

    /// Evict all entries.
    pub fn clear(&self) {
        if let Ok(mut map) = self.map.write() {
            map.clear();
        }
    }

    /// Number of stored entries, including any that are past their freshness
    /// lifetime but haven't been read (and so overwritten) since. This is a
    /// memory-occupancy figure, not a count of servable entries.
    pub fn len(&self) -> usize {
        self.map.read().map(|m| m.len()).unwrap_or(0)
    }
}

/// Determine whether a response should be cached.
/// Returns true if Cache-Control: public and status is 2xx.
pub fn should_cache(status: u16, headers: &[(String, String)]) -> bool {
    if status < 200 || status >= 300 {
        return false;
    }

    // `Vary` names request headers the response content depends on. The cache
    // key is `(path, query, encoding)` and cannot express any of them, so
    // storing such a response would replay one client's variant to everyone.
    // `Vary: Accept-Encoding` is the exception — encoding is already part of
    // the key.
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("vary") {
            let varies_only_on_encoding = value
                .split(',')
                .map(|f| f.trim())
                .filter(|f| !f.is_empty())
                .all(|f| f.eq_ignore_ascii_case("accept-encoding"));
            if !varies_only_on_encoding {
                return false;
            }
        }
    }

    for (name, value) in headers {
        if name.eq_ignore_ascii_case("cache-control") {
            let v = value.to_lowercase();
            if v.contains("no-store") || v.contains("private") {
                return false;
            }
            if v.contains("public") {
                return true;
            }
        }
    }
    false
}

/// Headers safe to store in a shared cache entry, with `Set-Cookie` removed.
///
/// A cacheable response's *content* can be identical for every visitor while
/// still carrying a `Set-Cookie` that is specific to whichever one request
/// happened to trigger the cache fill (e.g. a session-bootstrap cookie set
/// only on a client's first-ever hit with no session yet — see
/// `analytics::record`). Storing that header verbatim would replay one
/// visitor's cookie to every other visitor who later hits the same cache
/// entry. This is deliberately narrower than "never cache responses with
/// Set-Cookie": that would make cache warmth depend on whether the
/// triggering request happened to come from a new-vs-returning visitor,
/// which for any real mix of traffic defeats caching for exactly the
/// popular pages it matters most for. The content is shared and cacheable;
/// the cookie is not — so only the cookie is excluded from storage, not the
/// response.
pub fn strip_set_cookie(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("set-cookie")).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_response(status: u16, cc: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let headers = if cc.is_empty() {
            vec![]
        } else {
            vec![("cache-control".to_string(), cc.to_string())]
        };
        (status, headers, b"hello".to_vec())
    }

    #[test]
    fn test_should_cache_public() {
        let (status, headers, _) = make_response(200, "public, max-age=3600");
        assert!(should_cache(status, &headers));
    }

    #[test]
    fn test_should_not_cache_no_store() {
        let (status, headers, _) = make_response(200, "no-store");
        assert!(!should_cache(status, &headers));
    }

    #[test]
    fn test_should_not_cache_private() {
        let (status, headers, _) = make_response(200, "private");
        assert!(!should_cache(status, &headers));
    }

    #[test]
    fn test_should_not_cache_4xx() {
        let (status, headers, _) = make_response(404, "public");
        assert!(!should_cache(status, &headers));
    }

    #[test]
    fn test_should_not_cache_5xx() {
        let (status, headers, _) = make_response(500, "public");
        assert!(!should_cache(status, &headers));
    }

    #[test]
    fn test_cache_key_distinguishes_queries() {
        let k1 = CacheKey::new("/blog", Some("a=1"), "gzip");
        let k2 = CacheKey::new("/blog", Some("a=2"), "gzip");
        assert_ne!(k1, k2, "distinct queries must not share a cache entry");

        let no_query = CacheKey::new("/blog", None, "gzip");
        assert_ne!(k1, no_query, "a query must not collide with no query");
    }

    /// A `?` inside the `path` argument is defensive-stripped, so it can never
    /// smuggle one request's query into another request's key.
    #[test]
    fn test_cache_key_ignores_query_embedded_in_path_argument() {
        let embedded = CacheKey::new("/blog?a=1", None, "gzip");
        let bare = CacheKey::new("/blog", None, "gzip");
        assert_eq!(embedded, bare);
    }

    #[test]
    fn test_cache_key_encoding_independent() {
        let k1 = CacheKey::new("/blog", None, "gzip");
        let k2 = CacheKey::new("/blog", None, "br");
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_cache_insert_and_get() {
        let cache = Cache::new();
        let key = CacheKey::new("/hello", None, "");
        let resp = CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![]),
            body: bytes::Bytes::from_static(b"world"),
            hints: std::sync::Arc::new(vec![]),
        };
        cache.insert(key.clone(), resp);

        // Lookup via make_lookup_key (zero-alloc path)
        let mut buf = [0u8; 512];
        let lk = make_lookup_key("/hello", None, "", &mut buf);
        assert!(cache.get(lk).is_some());
        assert_eq!(cache.get(lk).unwrap().body, b"world" as &[u8]);

        // Also check legacy CacheKey::new-based borrow lookup
        let mut buf2 = [0u8; 512];
        let lk2 = make_lookup_key("/hello", None, "", &mut buf2);
        assert!(cache.get(lk2).is_some());
    }

    /// Build a cacheable response carrying `cc` as its `Cache-Control`.
    fn cached_with_cc(cc: &str) -> CachedResponse {
        let headers = if cc.is_empty() {
            vec![]
        } else {
            vec![("cache-control".to_string(), cc.to_string())]
        };
        CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(headers),
            body: bytes::Bytes::from_static(b"world"),
            hints: std::sync::Arc::new(vec![]),
        }
    }

    fn get_path(cache: &Cache, path: &str) -> Option<CachedResponse> {
        let mut buf = [0u8; 512];
        let lk = make_lookup_key(path, None, "", &mut buf);
        cache.get(lk)
    }

    /// `max-age=0` is already stale the instant it lands, so it must read back
    /// as a miss rather than being served once for free.
    #[test]
    fn test_cache_expires_at_zero_max_age() {
        let cache = Cache::new();
        cache.insert(
            CacheKey::new("/hello", None, ""),
            cached_with_cc("public, max-age=0"),
        );
        assert!(get_path(&cache, "/hello").is_none());
    }

    #[test]
    fn test_cache_serves_within_max_age() {
        let cache = Cache::new();
        cache.insert(
            CacheKey::new("/hello", None, ""),
            cached_with_cc("public, max-age=3600"),
        );
        assert!(get_path(&cache, "/hello").is_some());
    }

    /// No freshness lifetime means the entry lives until it is explicitly
    /// invalidated — the pre-existing behaviour, which the deploy pipeline's
    /// invalidate-cache.sh depends on.
    #[test]
    fn test_cache_without_max_age_never_expires() {
        let cache = Cache::new();
        cache.insert(CacheKey::new("/hello", None, ""), cached_with_cc("public"));
        assert!(get_path(&cache, "/hello").is_some());
    }

    /// This is a shared cache, so `s-maxage` overrides `max-age` when both are
    /// present — even when it appears second.
    #[test]
    fn test_s_maxage_overrides_max_age() {
        assert_eq!(
            freshness_secs(&[(
                "Cache-Control".to_string(),
                "public, max-age=3600, s-maxage=0".to_string(),
            )]),
            Some(0)
        );
    }

    /// `no-cache` means revalidate before every reuse. There is no upstream
    /// revalidation path here, so it has to read as a miss every time.
    #[test]
    fn test_no_cache_is_immediately_stale() {
        assert_eq!(
            freshness_secs(&[(
                "Cache-Control".to_string(),
                "public, no-cache, max-age=600".to_string(),
            )]),
            Some(0)
        );
    }

    /// `max-age` must not be matched inside `s-maxage` (or any other longer
    /// token) when it is the only directive being looked for.
    #[test]
    fn test_s_maxage_alone_is_not_read_as_max_age() {
        assert_eq!(
            freshness_secs(&[("Cache-Control".to_string(), "s-maxage=42".to_string())]),
            Some(42)
        );
    }

    #[test]
    fn test_freshness_absent_and_unparseable() {
        assert_eq!(freshness_secs(&[]), None);
        assert_eq!(
            freshness_secs(&[("Cache-Control".to_string(), "public".to_string())]),
            None
        );
        assert_eq!(
            freshness_secs(&[("Cache-Control".to_string(), "max-age=abc".to_string())]),
            None
        );
    }

    #[test]
    fn test_cache_evict_path() {
        let cache = Cache::new();
        let k1 = CacheKey::new("/page", None, "gzip");
        let k2 = CacheKey::new("/page", None, "br");
        let k3 = CacheKey::new("/other", None, "");
        let resp = CachedResponse { status: 200, headers: std::sync::Arc::new(vec![]), body: bytes::Bytes::new(), hints: std::sync::Arc::new(vec![]) };
        cache.insert(k1.clone(), resp.clone());
        cache.insert(k2.clone(), resp.clone());
        cache.insert(k3.clone(), resp.clone());

        cache.evict_path("/page");

        let mut buf = [0u8; 512];
        assert!(cache.get(make_lookup_key("/page", None, "gzip", &mut buf)).is_none());
        let mut buf = [0u8; 512];
        assert!(cache.get(make_lookup_key("/page", None, "br", &mut buf)).is_none());
        let mut buf = [0u8; 512];
        assert!(cache.get(make_lookup_key("/other", None, "", &mut buf)).is_some());
    }

    #[test]
    fn test_cache_evict_strips_query_from_stored_path() {
        let cache = Cache::new();
        // key stored with no query
        let k = CacheKey::new("/page", None, "");
        let resp = CachedResponse { status: 200, headers: std::sync::Arc::new(vec![]), body: bytes::Bytes::new(), hints: std::sync::Arc::new(vec![]) };
        cache.insert(k.clone(), resp);
        // evict with query — should still evict
        cache.evict_path("/page?x=1");

        let mut buf = [0u8; 512];
        let lk = make_lookup_key("/page", None, "", &mut buf);
        assert!(cache.get(lk).is_none());
    }

    #[test]
    fn test_public_cached_second_request_not_forwarded() {
        // Simulate: first request stores in cache, second request retrieves from cache
        let cache = Cache::new();
        let key = CacheKey::new("/blog/hello", None, "");
        let resp = CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![("cache-control".to_string(), "public".to_string())]),
            body: bytes::Bytes::from_static(b"cached body"),
            hints: std::sync::Arc::new(vec![]),
        };

        // First: check miss
        let mut buf = [0u8; 512];
        let lk = make_lookup_key("/blog/hello", None, "", &mut buf);
        assert!(cache.get(lk).is_none());

        // Store
        cache.insert(key.clone(), resp.clone());

        // Second: cache hit
        let mut buf = [0u8; 512];
        let lk = make_lookup_key("/blog/hello", None, "", &mut buf);
        let hit = cache.get(lk);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().body, b"cached body" as &[u8]);
    }

    #[test]
    fn test_make_lookup_key_matches_cache_key() {
        let key = CacheKey::new("/foo/bar", None, "gzip");
        let mut buf = [0u8; 512];
        let lk = make_lookup_key("/foo/bar", None, "gzip", &mut buf);
        // The borrow of CacheKey must equal the lookup key string.
        let borrowed: &str = std::borrow::Borrow::borrow(&key);
        assert_eq!(borrowed, lk);
    }

    #[test]
    fn test_make_lookup_key_distinguishes_queries() {
        let key = CacheKey::new("/foo", Some("x=1"), "br");
        let borrowed: &str = std::borrow::Borrow::borrow(&key);

        // Same query — must match.
        let mut buf = [0u8; 512];
        assert_eq!(borrowed, make_lookup_key("/foo", Some("x=1"), "br", &mut buf));

        // Different query — must not match.
        let mut buf = [0u8; 512];
        assert_ne!(borrowed, make_lookup_key("/foo", Some("y=2"), "br", &mut buf));
    }

    /// Keys are `path\x01query\x01encoding`; a path/query pair must not be able
    /// to impersonate a different one by moving the boundary.
    #[test]
    fn test_lookup_key_boundaries_are_unambiguous() {
        let mut a = [0u8; 512];
        let mut b = [0u8; 512];
        assert_ne!(
            make_lookup_key("/a", Some("b"), "gz", &mut a),
            make_lookup_key("/a\u{1}b", None, "gz", &mut b),
        );
    }

    #[test]
    fn test_evict_path_removes_all_query_variants() {
        let cache = Cache::new();
        let resp = CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![]),
            body: bytes::Bytes::new(),
            hints: std::sync::Arc::new(vec![]),
        };
        cache.insert(CacheKey::new("/page", Some("a=1"), ""), resp.clone());
        cache.insert(CacheKey::new("/page", Some("a=2"), "gzip"), resp.clone());
        cache.insert(CacheKey::new("/other", Some("a=1"), ""), resp);

        cache.evict_path("/page");

        let mut buf = [0u8; 512];
        assert!(cache.get(make_lookup_key("/page", Some("a=1"), "", &mut buf)).is_none());
        let mut buf = [0u8; 512];
        assert!(cache.get(make_lookup_key("/page", Some("a=2"), "gzip", &mut buf)).is_none());
        let mut buf = [0u8; 512];
        assert!(cache.get(make_lookup_key("/other", Some("a=1"), "", &mut buf)).is_some());
    }

    #[test]
    fn strip_set_cookie_removes_only_set_cookie() {
        let headers = vec![
            ("Content-Type".to_string(), "text/html".to_string()),
            ("Set-Cookie".to_string(), "_m6sid=abc; Path=/".to_string()),
            ("Cache-Control".to_string(), "public".to_string()),
        ];
        let stripped = strip_set_cookie(&headers);
        assert_eq!(stripped.len(), 2);
        assert!(stripped.iter().all(|(k, _)| !k.eq_ignore_ascii_case("set-cookie")));
        assert!(stripped.iter().any(|(k, _)| k == "Content-Type"));
        assert!(stripped.iter().any(|(k, _)| k == "Cache-Control"));
    }

    #[test]
    fn strip_set_cookie_removes_multiple_occurrences() {
        let headers = vec![
            ("Set-Cookie".to_string(), "a=1".to_string()),
            ("Set-Cookie".to_string(), "b=2".to_string()),
        ];
        assert!(strip_set_cookie(&headers).is_empty());
    }

    #[test]
    fn strip_set_cookie_is_a_noop_when_absent() {
        let headers = vec![("Content-Type".to_string(), "text/plain".to_string())];
        assert_eq!(strip_set_cookie(&headers), headers);
    }
}
