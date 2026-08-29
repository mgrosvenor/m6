/// In-memory response cache with atomic Arc swap.
use std::borrow::Borrow;
use ahash::AHashMap;

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
type CacheMap = AHashMap<CacheKey, CachedResponse>;

use std::sync::{Arc, RwLock};

/// Cache backed by Arc<RwLock<HashMap>> — swap the whole map atomically.
/// Clone is cheap — just clones the Arc.
#[derive(Clone)]
pub struct Cache {
    map: Arc<RwLock<CacheMap>>,
}

impl Cache {
    pub fn new() -> Self {
        Cache { map: Arc::new(RwLock::new(AHashMap::<CacheKey, CachedResponse>::new())) }
    }

    /// Get a cached response.
    ///
    /// Accepts any borrowed form of `CacheKey`:
    /// - `&str` — zero-allocation hot-path lookup via `make_lookup_key`
    /// - `&CacheKey` — legacy/test usage (blanket `Borrow<CacheKey>` impl)
    pub fn get<Q>(&self, key: &Q) -> Option<CachedResponse>
    where
        CacheKey: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.map.read().ok()?.get(key).cloned()
    }

    /// Store a response. Only call if the response should be cached.
    pub fn insert(&self, key: CacheKey, response: CachedResponse) {
        if let Ok(mut map) = self.map.write() {
            map.insert(key, response);
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

    /// Number of entries.
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
