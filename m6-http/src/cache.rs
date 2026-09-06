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
    // RFC 9110 15.4.5: a 304 carries the metadata a 200 would have sent, so a
    // client can update its stored response from it.
    //
    // This kept only ETag, Last-Modified and Cache-Control. `Vary` in
    // particular was dropped, which is the damaging one: a client or shared
    // cache updating its stored entry from this 304 would lose the knowledge
    // that the response varies by `Accept-Encoding` and could then reuse a
    // brotli body for a gzip-only request. `Date` was missing too, so the
    // recipient had nothing to compute age from.
    //
    // Content-Length is deliberately NOT carried: RFC 9110 8.6 allows it on a
    // 304 only when it equals the 200's length, and getting that wrong is
    // worse than omitting it. The serialisers set framing for a bodyless
    // response themselves.
    cached_headers.iter()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            k == "etag"
                || k == "last-modified"
                || k == "cache-control"
                || k == "vary"
                || k == "date"
                || k == "expires"
                || k == "content-location"
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

/// Whether a request with this method may be **answered from** the cache.
///
/// GET and HEAD only. A HEAD is deliberately answered from the GET entry with
/// the body stripped at serialisation — that sharing is what makes HEAD cheap,
/// and it is the reason the key carries no method component.
///
/// This pairing is the safety property that matters, so it is worth stating
/// plainly. The key namespace holds GET representations and nothing else,
/// because [`method_may_write_cache`] admits only GET. No unsafe or unknown
/// method can therefore read a cached entry or store one, which is exactly
/// what putting the method in the key would have bought — obtained by
/// construction instead, and pinned by tests rather than left implicit in a
/// string encoding.
///
/// Before this existed, `cacheable` was derived from the route's auth
/// requirement alone and nothing anywhere inspected the method: every verb
/// including TRACE and invented ones like FOO was served the cached page.
pub fn method_may_read_cache(method: &str) -> bool {
    // Case-SENSITIVE. RFC 9110 9.1: the method token is case-sensitive, so
    // `get` is not GET.
    //
    // These were `eq_ignore_ascii_case`, and that disagreed with the
    // configured allow-list, which compares exactly. The cache lookup runs
    // BEFORE the method gate, so a request with method `get` matched here,
    // was served from cache, and never reached the check that would have
    // refused it -- the gate was bypassed entirely by changing the case.
    // Two comparisons of the same thing must not disagree.
    method == "GET" || method == "HEAD"
}

/// Whether a response to this method may be **stored in** the cache.
///
/// GET only — deliberately narrower than [`method_may_read_cache`].
///
/// HEAD must never store, and the ordering here is a real hazard rather than
/// tidiness. While HEAD wrongly returned a full body, the entry a HEAD stored
/// was byte-identical to a GET entry, so the shared key was harmless. Fix the
/// HEAD body without this and a HEAD would store a *bodyless* response under
/// the key a subsequent GET reads, turning a protocol violation into silent
/// content loss. The two changes belong in the same commit.
pub fn method_may_write_cache(method: &str) -> bool {
    // Case-sensitive, for the same reason as `method_may_read_cache`.
    method == "GET"
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

/// How far ahead of the advertised lifetime this cache refreshes an entry.
///
/// An edge that expires at exactly `max-age` is always refreshing one step
/// behind whoever asked: the browser's own copy goes stale at the same instant
/// ours does, so its revalidation arrives to find us stale too. Expiring one
/// second early means the background refresh has already landed by the time
/// anything downstream comes asking — a `max-age=60` response becomes a
/// 59-second local lifetime on a 60-second refresh cycle.
const REFRESH_MARGIN: std::time::Duration = std::time::Duration::from_secs(1);

/// Freshness lifetime from `Expires`, relative to the response's own `Date`.
///
/// RFC 9111 4.2.1: when no `max-age`/`s-maxage` is present, the freshness
/// lifetime is `Expires - Date`. Using our own clock instead of the
/// response's `Date` would silently lengthen or shorten the lifetime by
/// whatever the two servers' clocks disagree by.
///
/// A missing `Date` falls back to now, which is the best available reading
/// and matches what a recipient is expected to do when one is absent. An
/// `Expires` at or before `Date` means already stale, hence zero rather than
/// a negative that would wrap.
fn expires_lifetime(headers: &[(String, String)]) -> Option<std::time::Duration> {
    let get = |name: &str| {
        headers.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| httpdate::parse_http_date(v.trim()).ok())
    };
    let expires = get("expires")?;
    let date = get("date").unwrap_or_else(std::time::SystemTime::now);
    Some(expires.duration_since(date).unwrap_or(std::time::Duration::ZERO))
}

/// Ceiling on an entry stored with no explicit freshness directive.
///
/// A response carrying bare `Cache-Control: public` used to produce
/// `expires_at = None` and stay fresh **forever**. That is the deliberate
/// CDN-style model the deploy pipeline relies on — content lives until
/// `invalidate-cache.sh` clears it — but "forever" is not a defensible
/// reading of RFC 9111 4.2.2, which permits a *heuristic* freshness lifetime,
/// not an unbounded one. An entry whose invalidation is missed for any reason
/// is then served indefinitely with no upper bound at all.
///
/// A day keeps the model intact — deploys invalidate far more often than
/// this, so in normal operation it never expires anything the pipeline was not
/// going to clear anyway — while making "the invalidation was missed" a
/// bounded fault instead of a permanent one. Nothing this site serves relies
/// on it: every route and asset carries an explicit `max-age`.
const HEURISTIC_MAX_LIFETIME: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// What a response's own `Cache-Control` asks this cache to do.
struct Directives {
    /// Freshness lifetime. `None` = none specified: the entry lives until an
    /// explicit invalidation, which is the deliberate CDN-style model the
    /// deploy pipeline relies on (see `evict_path`/`clear` and
    /// deploy/invalidate-cache.sh), not an oversight.
    lifetime: Option<std::time::Duration>,
    /// How long past `lifetime` this entry may still be served while a
    /// refresh runs in the background (RFC 5861 `stale-while-revalidate`).
    /// Zero — the default when the directive is absent — means never serve
    /// stale.
    stale_while_revalidate: std::time::Duration,
    /// `no-cache` was present: reuse requires revalidation first.
    ///
    /// Tracked separately from a zero `lifetime` rather than inferred from
    /// one, because the two are genuinely different. `max-age=0` means "stale
    /// immediately" and pairs perfectly sensibly with stale-while-revalidate
    /// — it is the normal way to say "always refresh, never make anyone
    /// wait". `no-cache` forbids unrevalidated reuse outright.
    no_cache: bool,
}

/// A parsed `Cache-Control` field, combining every field line.
///
/// Replaces substring matching over one field line at a time, which was wrong
/// in two ways that both mattered:
///
///   * `contains("public")` returned true for `public-cache-extension`, and
///     any unknown extension token containing the word.
///   * The old loop returned on the FIRST field line that mentioned something
///     it recognised, so `Cache-Control: public` followed by a separate
///     `Cache-Control: no-store` line stored the response. RFC 9110 5.2 says
///     multiple field lines of a list-based field are equivalent to one
///     comma-joined line, so a directive anywhere must be seen.
///
/// Commas inside a quoted value (`no-cache="Set-Cookie"`, a legal form) do not
/// split a directive.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CacheControl {
    pub no_store: bool,
    pub no_cache: bool,
    pub private: bool,
    pub public: bool,
    pub must_revalidate: bool,
    pub max_age: Option<u64>,
    pub s_maxage: Option<u64>,
    pub stale_while_revalidate: Option<u64>,
}

impl CacheControl {
    /// Parse every `Cache-Control` field line in `headers` as one directive list.
    pub fn parse(headers: &[(String, String)]) -> Self {
        let mut cc = CacheControl::default();
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("cache-control") {
                continue;
            }
            for (dname, dval) in split_directives(value) {
                let secs = || dval.as_deref().and_then(|v| v.trim().parse::<u64>().ok());
                match dname.as_str() {
                    "no-store"               => cc.no_store = true,
                    "no-cache"               => cc.no_cache = true,
                    "private"                => cc.private = true,
                    "public"                 => cc.public = true,
                    "must-revalidate"        => cc.must_revalidate = true,
                    "max-age"                => cc.max_age = secs(),
                    "s-maxage"               => cc.s_maxage = secs(),
                    "stale-while-revalidate" => cc.stale_while_revalidate = secs(),
                    _ => {}
                }
            }
        }
        cc
    }

    /// Freshness lifetime for a SHARED cache. `s-maxage` wins over `max-age`,
    /// which is exactly what it is for. `no-cache` means zero.
    pub fn shared_lifetime(&self) -> Option<std::time::Duration> {
        if self.no_cache {
            return Some(std::time::Duration::ZERO);
        }
        self.s_maxage.or(self.max_age).map(std::time::Duration::from_secs)
    }
}

/// Split one `Cache-Control` field value into `(lowercased-name, value)` pairs,
/// respecting quoted-string values so a comma inside quotes does not split.
fn split_directives(value: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in value.chars() {
        match ch {
            '\\' if in_quotes && !escaped => { escaped = true; cur.push(ch); }
            '"' if !escaped => { in_quotes = !in_quotes; cur.push(ch); }
            ',' if !in_quotes => { push_directive(&mut out, &cur); cur.clear(); }
            _ => { escaped = false; cur.push(ch); }
        }
    }
    push_directive(&mut out, &cur);
    out
}

fn push_directive(out: &mut Vec<(String, Option<String>)>, raw: &str) {
    let t = raw.trim();
    if t.is_empty() {
        return;
    }
    match t.split_once('=') {
        Some((n, v)) => {
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|r| r.strip_suffix('"')).unwrap_or(v);
            out.push((n.trim().to_ascii_lowercase(), Some(v.to_string())));
        }
        None => out.push((t.to_ascii_lowercase(), None)),
    }
}

/// Parse the directives this cache acts on out of a response's `Cache-Control`.
///
/// `s-maxage` wins over `max-age` when both are present: this is a shared
/// cache, and that is exactly what `s-maxage` is for.
fn directives(headers: &[(String, String)]) -> Directives {
    let cc = CacheControl::parse(headers);
    Directives {
        lifetime: cc.shared_lifetime(),
        stale_while_revalidate: cc
            .stale_while_revalidate
            .map(std::time::Duration::from_secs)
            .unwrap_or(std::time::Duration::ZERO),
        no_cache: cc.no_cache,
    }
}

/// A stored entry: the response, when it goes stale, and how long past that it
/// may still be served while a background refresh runs.
///
/// `expires_at == None` means "no freshness lifetime given" — lives until
/// explicit invalidation.
struct CacheEntry {
    response: CachedResponse,
    /// When this entry was stored, and the age it already had on arrival.
    ///
    /// RFC 9111 5.1 requires a shared cache to send `Age` on a stored
    /// response, and 4.2.3 defines it as time since the response was
    /// *generated* -- not since we happened to store it. Neither existed:
    /// nothing emitted `Age` at all, so a downstream cache had no way to know
    /// how old what we handed it already was, and treated a minute-old
    /// response as brand new.
    ///
    /// `upstream_age` captures the `Age` the origin already declared, so an
    /// entry that arrived one hop old does not restart the clock here.
    stored_at: std::time::Instant,
    upstream_age: std::time::Duration,
    expires_at: Option<std::time::Instant>,
    /// Instant past which even a stale serve is refused. Equal to
    /// `expires_at` when the response did not permit stale serving at all.
    serve_stale_until: Option<std::time::Instant>,
}

/// The result of a cache lookup.
pub enum Lookup {
    /// Inside its freshness lifetime — serve it, nothing else to do.
    ///
    /// The `Duration` is the response's current age (RFC 9111 4.2.3): time
    /// since it was generated, which includes any `Age` it already carried
    /// when it arrived here. The caller emits it as the `Age` header. It is
    /// returned alongside rather than written into the stored headers because
    /// the value changes every second, while the stored headers are shared
    /// behind an `Arc` and cloned only where the caller is already building an
    /// owned header vector — so this costs nothing on the hot path.
    Fresh(CachedResponse, std::time::Duration),
    /// Past freshness but inside the stale-while-revalidate window. Serve it
    /// immediately — the whole point is that no visitor ever waits on an
    /// origin round trip — and queue a background refresh so the *next*
    /// request gets the new copy. Costs at most one stale serve per entry per
    /// expiry.
    Stale(CachedResponse, std::time::Duration),
    /// Nothing usable: absent, or stale beyond what the response permits.
    Miss,
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
    /// Only a *fresh* entry — a stale one reads as absent. This is the right
    /// question for "is this already cached?" callers (hint dedup, the
    /// background-refresh guard), for which a stale entry is precisely one
    /// that does still need fetching. Serving paths want `lookup` instead, so
    /// they can serve stale rather than making a visitor wait.
    pub fn get<Q>(&self, key: &Q) -> Option<CachedResponse>
    where
        CacheKey: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        match self.lookup(key) {
            Lookup::Fresh(r, _) => Some(r),
            Lookup::Stale(..) | Lookup::Miss => None,
        }
    }

    /// Look up an entry, distinguishing fresh from servable-but-stale.
    ///
    /// Expiry is evaluated here, on read, rather than swept by a background
    /// task: a stale entry costs nothing until someone asks for it, and the
    /// refresh it triggers overwrites it. That keeps expiry off the write path
    /// entirely and needs no timer thread.
    pub fn lookup<Q>(&self, key: &Q) -> Lookup
    where
        CacheKey: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let Ok(map) = self.map.read() else { return Lookup::Miss };
        let Some(entry) = map.get(key) else { return Lookup::Miss };
        let now = std::time::Instant::now();
        // Current age: how long we have held it, plus whatever age it already
        // had on arrival. Resetting to zero at each hop is what makes a chain
        // of caches report content as fresher than it is.
        let age = entry.upstream_age + now.saturating_duration_since(entry.stored_at);
        // Every branch decides before cloning, so a miss costs no copy.
        let Some(expires_at) = entry.expires_at else {
            return Lookup::Fresh(entry.response.clone(), age);
        };
        if now < expires_at {
            Lookup::Fresh(entry.response.clone(), age)
        } else if entry.serve_stale_until.is_some_and(|until| now < until) {
            Lookup::Stale(entry.response.clone(), age)
        } else {
            Lookup::Miss
        }
    }

    /// Store a response. Only call if the response should be cached.
    ///
    /// Deadlines are derived from the response's own `Cache-Control` here
    /// rather than passed in, so every caller gets correct expiry without
    /// having to know about it.
    pub fn insert(&self, key: CacheKey, response: CachedResponse) {
        let d = directives(&response.headers);
        let now = std::time::Instant::now();
        // Any `Age` the upstream already declared. A response that reached us
        // one hop old must not have its age reset to zero here, or each hop
        // would silently make the content look fresher than it is.
        let upstream_age = response.headers.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("age"))
            .and_then(|(_, v)| v.trim().parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or_default();
        // The margin is why a max-age=60 response expires locally at 59s. It
        // saturates rather than wrapping, so a zero lifetime stays zero.
        // Freshness, in the precedence RFC 9111 4.2.1 requires:
        //   1. s-maxage   (shared caches; handled in shared_lifetime)
        //   2. max-age
        //   3. Expires minus Date
        //   4. a heuristic
        //
        // `Expires` was not consulted at all, so a response using the older
        // header -- still perfectly valid, and what a lot of software emits --
        // fell straight through to "no freshness given" and was treated as
        // fresh indefinitely. Exactly backwards: it carried an explicit
        // expiry and we ignored it.
        //
        // Measured against the response's own `Date` rather than our clock,
        // per 4.2.1, so a skewed server does not get a longer or shorter
        // lifetime here than it asked for.
        let expires_at = Some(match d.lifetime.or_else(|| expires_lifetime(&response.headers)) {
            Some(l) => now + l.saturating_sub(REFRESH_MARGIN),
            None => now + HEURISTIC_MAX_LIFETIME,
        });
        // `no-cache` forbids a stale serve outright and outranks any
        // stale-while-revalidate the same response happens to carry —
        // otherwise the two together would produce exactly the unrevalidated
        // reuse `no-cache` exists to prevent. Note this is keyed on `no_cache`
        // and not on a zero lifetime: `max-age=0, stale-while-revalidate=N` is
        // the normal, valid way to say "always refresh, never make anyone
        // wait", and must keep its stale window.
        let stale_window = if d.no_cache {
            std::time::Duration::ZERO
        } else {
            d.stale_while_revalidate
        };
        let serve_stale_until = expires_at.map(|e| e + stale_window);
        if let Ok(mut map) = self.map.write() {
            map.insert(key, CacheEntry {
                response, stored_at: now, upstream_age, expires_at, serve_stale_until,
            });
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
    if !status_is_storable(status) {
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

    // Directive precedence, evaluated over ALL field lines at once rather than
    // returning on whichever line was seen first. `no-store` and `private`
    // forbid storage in a shared cache no matter where they appear, so a
    // response carrying `public` on one line and `no-store` on another is
    // refused -- the old loop stored it.
    let cc = CacheControl::parse(headers);
    if cc.no_store || cc.private {
        return false;
    }
    cc.public
}

/// A 206 must never be stored by this cache.
///
/// `should_cache` used to admit every 2xx. A 206 is a *partial* representation
/// described by its `Content-Range`, and this cache has no range awareness: no
/// range component in the key, no way to combine partials, and no way to
/// answer a later full GET from one. Storing it means a subsequent request for
/// the whole resource can be served a fragment as though it were complete.
///
/// Split out from the status range so the reason is stated where the decision
/// is made, rather than being implicit in an inequality.
fn status_is_storable(status: u16) -> bool {
    if status == 206 {
        return false;
    }
    (200..300).contains(&status)
}

/// Whether the REQUEST permits this response to be stored.
///
/// RFC 9111 5.2.1.5: a request carrying `Cache-Control: no-store` must not
/// have its response written to cache. This was ignored entirely -- request
/// directives were never parsed at all, so a client explicitly asking for its
/// exchange not to be retained had that request stored and replayed to others.
///
/// Deliberately only `no-store`. `no-cache` on a *request* means "revalidate
/// before reuse", not "do not store", and treating the two alike would throw
/// away hit rate for no correctness gain.
pub fn request_permits_storage(req_headers: &[(String, String)]) -> bool {
    !CacheControl::parse(req_headers).no_store
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

    fn lookup_path(cache: &Cache, path: &str) -> Lookup {
        let mut buf = [0u8; 512];
        let lk = make_lookup_key(path, None, "", &mut buf);
        cache.lookup(lk)
    }

    fn cc(v: &str) -> Vec<(String, String)> {
        vec![("Cache-Control".to_string(), v.to_string())]
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
        assert!(matches!(lookup_path(&cache, "/hello"), Lookup::Miss));
    }

    #[test]
    fn test_cache_serves_within_max_age() {
        let cache = Cache::new();
        cache.insert(
            CacheKey::new("/hello", None, ""),
            cached_with_cc("public, max-age=3600"),
        );
        assert!(matches!(lookup_path(&cache, "/hello"), Lookup::Fresh(..)));
    }

    /// No freshness lifetime means the entry lives until it is explicitly
    /// invalidated — the pre-existing behaviour, which the deploy pipeline's
    /// invalidate-cache.sh depends on.
    #[test]
    fn test_cache_without_max_age_never_expires() {
        let cache = Cache::new();
        cache.insert(CacheKey::new("/hello", None, ""), cached_with_cc("public"));
        assert!(matches!(lookup_path(&cache, "/hello"), Lookup::Fresh(..)));
    }

    /// The whole point of stale-while-revalidate: once past freshness the
    /// entry is still served (so no visitor waits on origin), flagged so the
    /// caller knows to refresh it in the background.
    #[test]
    fn test_expired_entry_is_served_stale_within_swr_window() {
        let cache = Cache::new();
        cache.insert(
            CacheKey::new("/hello", None, ""),
            cached_with_cc("public, max-age=0, stale-while-revalidate=3600"),
        );
        match lookup_path(&cache, "/hello") {
            Lookup::Stale(r, _) => assert_eq!(r.body, b"world" as &[u8]),
            _ => panic!("expected a stale serve, not a miss"),
        }
        // `get` is the fresh-only question, so it must still say no — that is
        // what makes the background-refresh guard proceed for this entry.
        assert!(get_path(&cache, "/hello").is_none());
    }

    /// Without the directive there is no stale window, so an expired entry is
    /// a hard miss. Stale serving is opt-in per response, exactly like max-age.
    #[test]
    fn test_expired_without_swr_directive_is_a_miss() {
        let cache = Cache::new();
        cache.insert(
            CacheKey::new("/hello", None, ""),
            cached_with_cc("public, max-age=0"),
        );
        assert!(matches!(lookup_path(&cache, "/hello"), Lookup::Miss));
    }

    /// `no-cache` forbids reuse without revalidation. m6-http has no
    /// revalidation path, so a stale-while-revalidate on the same response
    /// must not talk it into serving stale anyway.
    #[test]
    fn test_no_cache_is_never_served_stale_even_with_swr() {
        let cache = Cache::new();
        cache.insert(
            CacheKey::new("/hello", None, ""),
            cached_with_cc("public, no-cache, stale-while-revalidate=3600"),
        );
        assert!(matches!(lookup_path(&cache, "/hello"), Lookup::Miss));
    }

    /// The refresh margin: a max-age=60 response is stored with a 59-second
    /// lifetime, so the background refresh lands before anything downstream
    /// considers its own copy stale.
    #[test]
    fn test_refresh_margin_shortens_lifetime() {
        let d = directives(&cc("public, max-age=60"));
        assert_eq!(d.lifetime, Some(std::time::Duration::from_secs(60)));
        assert_eq!(
            d.lifetime.unwrap() - REFRESH_MARGIN,
            std::time::Duration::from_secs(59)
        );
    }

    /// The margin saturates rather than wrapping — a zero lifetime must not
    /// underflow into a near-infinite one.
    #[test]
    fn test_refresh_margin_saturates_at_zero() {
        let cache = Cache::new();
        cache.insert(
            CacheKey::new("/hello", None, ""),
            cached_with_cc("public, max-age=0"),
        );
        assert!(matches!(lookup_path(&cache, "/hello"), Lookup::Miss));
    }

    /// This is a shared cache, so `s-maxage` overrides `max-age` when both are
    /// present — even when it appears second.
    #[test]
    fn test_s_maxage_overrides_max_age() {
        assert_eq!(
            directives(&cc("public, max-age=3600, s-maxage=0")).lifetime,
            Some(std::time::Duration::ZERO)
        );
    }

    /// `no-cache` means revalidate before every reuse. There is no upstream
    /// revalidation path here, so it has to read as a zero lifetime.
    #[test]
    fn test_no_cache_is_immediately_stale() {
        assert_eq!(
            directives(&cc("public, no-cache, max-age=600")).lifetime,
            Some(std::time::Duration::ZERO)
        );
    }

    /// `max-age` must not be matched inside `s-maxage`, nor
    /// `stale-while-revalidate` be read as either of them.
    #[test]
    fn test_directive_names_are_not_confused_for_each_other() {
        assert_eq!(
            directives(&cc("s-maxage=42")).lifetime,
            Some(std::time::Duration::from_secs(42))
        );
        // stale-while-revalidate alone sets no lifetime.
        let d = directives(&cc("public, stale-while-revalidate=99"));
        assert_eq!(d.lifetime, None);
        assert_eq!(d.stale_while_revalidate, std::time::Duration::from_secs(99));
    }

    #[test]
    fn test_freshness_absent_and_unparseable() {
        assert_eq!(directives(&[]).lifetime, None);
        assert_eq!(directives(&cc("public")).lifetime, None);
        assert_eq!(directives(&cc("max-age=abc")).lifetime, None);
        assert_eq!(
            directives(&cc("public")).stale_while_revalidate,
            std::time::Duration::ZERO
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

    // ── Encoding fidelity ────────────────────────────────────────────────
    //
    // A shared cache keyed partly on Accept-Encoding is only correct while the
    // stored body is actually encoded the way its key implies. Getting that
    // wrong does not surface as a miss -- it silently hands the wrong bytes to
    // every client that shares the key. It happened: a background refresh
    // fetched without Accept-Encoding, got an identity body, and stored it
    // under the key for `gzip, deflate, br, zstd`, so browsers downloaded a
    // 44KB stylesheet where 7KB brotli was advertised, refreshing itself back
    // into that state every 60s.

    fn encoded(enc: &str, body: &'static [u8]) -> CachedResponse {
        let mut headers = vec![("content-type".to_string(), "text/css".to_string())];
        if !enc.is_empty() {
            headers.push(("content-encoding".to_string(), enc.to_string()));
        }
        CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(headers),
            body: bytes::Bytes::from_static(body),
            hints: std::sync::Arc::new(vec![]),
        }
    }

    fn content_encoding_of(r: &CachedResponse) -> Option<&str> {
        r.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
            .map(|(_, v)| v.as_str())
    }

    /// Round-trip: whatever encoding went in comes back out. A cache that
    /// dropped or rewrote Content-Encoding would leave clients unable to
    /// decode the body at all.
    #[test]
    fn test_stored_content_encoding_survives_round_trip() {
        let cache = Cache::new();
        for (key_enc, resp_enc) in [
            ("gzip, deflate, br, zstd", "br"),
            ("gzip, deflate", "gzip"),
            ("br", "br"),
            ("", ""),
        ] {
            let key = CacheKey::new("/assets/css/style.css", None, key_enc);
            cache.insert(key.clone(), encoded(resp_enc, b"body"));
            let got = cache.get(&key).expect("just inserted");
            assert_eq!(
                content_encoding_of(&got),
                if resp_enc.is_empty() { None } else { Some(resp_enc) },
                "content-encoding must survive for key {key_enc:?}"
            );
        }
    }

    /// The exact live failure: an identity body written under the key a browser
    /// uses. Distinct Accept-Encoding strings are distinct keys, so a correct
    /// entry under `br` does nothing to protect the browser's key -- which is
    /// precisely why hand-probing with `Accept-Encoding: br` looked healthy
    /// while every real visitor got the uncompressed body.
    #[test]
    fn test_identity_body_under_a_browser_key_does_not_mask_itself() {
        let cache = Cache::new();
        let browser = CacheKey::new("/assets/css/style.css", None, "gzip, deflate, br, zstd");
        let probe = CacheKey::new("/assets/css/style.css", None, "br");

        cache.insert(probe.clone(), encoded("br", b"small"));
        cache.insert(browser.clone(), encoded("", b"this-is-the-large-identity-body"));

        // Probing the `br` key reports health it cannot vouch for.
        assert_eq!(content_encoding_of(&cache.get(&probe).unwrap()), Some("br"));
        // The key browsers actually use is the broken one.
        assert_eq!(content_encoding_of(&cache.get(&browser).unwrap()), None);
        assert_ne!(
            cache.get(&probe).unwrap().body,
            cache.get(&browser).unwrap().body,
            "the two keys are independent -- one being correct proves nothing about the other"
        );
    }

    /// A refresh overwriting an entry must not silently change its encoding.
    /// This is the shape of the regression: same key, same URL, body that was
    /// compressed yesterday and is not today.
    #[test]
    fn test_refresh_overwriting_with_a_different_encoding_is_observable() {
        let cache = Cache::new();
        let key = CacheKey::new("/assets/css/style.css", None, "gzip, deflate, br, zstd");

        cache.insert(key.clone(), encoded("br", b"compressed"));
        let before = content_encoding_of(&cache.get(&key).unwrap()).map(str::to_string);
        assert_eq!(before.as_deref(), Some("br"));

        // What the broken refresh did.
        cache.insert(key.clone(), encoded("", b"identity-and-much-larger"));
        let after = content_encoding_of(&cache.get(&key).unwrap()).map(str::to_string);

        assert_ne!(
            before, after,
            "an encoding change across a refresh is exactly the corruption to catch"
        );
        assert_eq!(after, None);
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

#[cfg(test)]
mod method_gate_tests {
    use super::*;

    #[test]
    fn only_get_and_head_may_read() {
        for m in ["GET", "HEAD"] {
            assert!(method_may_read_cache(m), "{m} should be able to read cache");
        }
        // Case-sensitive: `get` is a different (unregistered) method, and must
        // not slip past the method gate by reaching the cache first.
        for m in ["get", "head", "Get", "Head"] {
            assert!(!method_may_read_cache(m), "{m} must not be treated as GET/HEAD");
        }
        // Every verb below was served the cached page before this gate existed,
        // including TRACE and an entirely invented method.
        for m in ["POST", "PUT", "DELETE", "PATCH", "OPTIONS", "TRACE", "CONNECT", "FOO", ""] {
            assert!(!method_may_read_cache(m), "{m} must not read cache");
        }
    }

    /// Narrower than the read gate, and deliberately so: a HEAD now produces a
    /// bodyless response, so letting one store would put an empty body under
    /// the key a later GET reads.
    #[test]
    fn only_get_may_write() {
        assert!(method_may_write_cache("GET"));
        assert!(!method_may_write_cache("get"), "case-sensitive: `get` is not GET");
        for m in ["HEAD", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "TRACE", "FOO", ""] {
            assert!(!method_may_write_cache(m), "{m} must not write cache");
        }
    }

    /// The safety property that replaces putting the method in the key: the
    /// key namespace can only ever hold GET representations, because nothing
    /// else is admitted to write. Anything that may read is therefore reading
    /// a GET entry, which is exactly what HEAD wants and what no other method
    /// is allowed to attempt.
    #[test]
    fn everything_that_may_write_may_also_read() {
        for m in ["GET", "HEAD", "POST", "PUT", "DELETE", "TRACE", "FOO"] {
            if method_may_write_cache(m) {
                assert!(method_may_read_cache(m), "{m} can write but not read — key namespace would split");
            }
        }
    }

    /// A HEAD must not be able to displace the GET entry it shares a key with.
    /// Constructed as the real code does it: the key ignores the method, so
    /// this only holds because the write gate refuses HEAD.
    #[test]
    fn a_head_cannot_poison_the_get_entry() {
        let cache = Cache::new();
        cache.insert(CacheKey::new("/page", None, "br"), CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![]),
            body: bytes::Bytes::from_static(b"full GET body"),
            hints: std::sync::Arc::new(vec![]),
        });

        // What a HEAD would store if it were allowed to: same key, empty body.
        assert!(!method_may_write_cache("HEAD"),
                "if this ever becomes true, the entry below overwrites the GET body");

        let mut b2 = [0u8; 512];
        let got = cache.get(make_lookup_key("/page", None, "br", &mut b2)).expect("entry present");
        assert_eq!(&got.body[..], b"full GET body");
    }
}

#[cfg(test)]
mod cache_control_tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn parses_a_simple_list() {
        let cc = CacheControl::parse(&h(&[("cache-control", "public, max-age=60")]));
        assert!(cc.public);
        assert_eq!(cc.max_age, Some(60));
        assert!(!cc.no_store);
    }

    /// RFC 9110 5.2: several field lines of a list-based field are equivalent
    /// to one comma-joined line. The old code returned on the FIRST line that
    /// mentioned something it recognised, so this exact pairing was stored.
    #[test]
    fn no_store_on_a_later_field_line_still_wins() {
        let hs = h(&[("cache-control", "public"), ("cache-control", "no-store")]);
        assert!(CacheControl::parse(&hs).no_store);
        assert!(!should_cache(200, &hs), "public on an earlier line must not beat a later no-store");
    }

    /// `contains("public")` matched any extension token containing the word.
    #[test]
    fn an_extension_token_containing_public_is_not_the_public_directive() {
        let cc = CacheControl::parse(&h(&[("cache-control", "public-cache-extension")]));
        assert!(!cc.public, "matched a directive named `public-cache-extension`");
        assert!(!should_cache(200, &h(&[("cache-control", "public-cache-extension")])));
    }

    #[test]
    fn private_and_no_store_both_forbid_storage() {
        for v in ["private", "no-store", "public, private", "max-age=60, no-store"] {
            assert!(!should_cache(200, &h(&[("cache-control", v)])), "{v} must not be stored");
        }
    }

    /// A comma inside a quoted value must not split the directive list, and a
    /// quoted delta-seconds must still parse.
    #[test]
    fn quoted_values_do_not_split_the_list() {
        let cc = CacheControl::parse(&h(&[(
            "cache-control",
            "no-cache=\"Set-Cookie, X-Thing\", max-age=30, public",
        )]));
        assert!(cc.no_cache);
        assert_eq!(cc.max_age, Some(30), "a comma inside quotes split the list");
        assert!(cc.public);
    }

    #[test]
    fn quoted_delta_seconds_parses() {
        let cc = CacheControl::parse(&h(&[("cache-control", "max-age=\"60\"")]));
        assert_eq!(cc.max_age, Some(60));
    }

    #[test]
    fn directive_names_are_case_insensitive() {
        let cc = CacheControl::parse(&h(&[("Cache-Control", "PUBLIC, Max-Age=15")]));
        assert!(cc.public);
        assert_eq!(cc.max_age, Some(15));
    }

    /// s-maxage is the shared-cache lifetime and outranks max-age.
    #[test]
    fn s_maxage_wins_for_a_shared_cache() {
        let cc = CacheControl::parse(&h(&[("cache-control", "max-age=10, s-maxage=99")]));
        assert_eq!(cc.shared_lifetime(), Some(std::time::Duration::from_secs(99)));
    }

    #[test]
    fn no_cache_means_zero_lifetime() {
        let cc = CacheControl::parse(&h(&[("cache-control", "public, max-age=600, no-cache")]));
        assert_eq!(cc.shared_lifetime(), Some(std::time::Duration::ZERO));
    }

    /// A 206 describes a PARTIAL representation. This cache has no range
    /// awareness, so storing one lets a later full GET be served a fragment as
    /// though it were the whole resource.
    #[test]
    fn a_206_is_never_stored() {
        assert!(!should_cache(206, &h(&[("cache-control", "public, max-age=60")])));
    }

    #[test]
    fn ordinary_2xx_still_stored() {
        for s in [200u16, 203, 204] {
            assert!(should_cache(s, &h(&[("cache-control", "public")])), "{s} should store");
        }
        for s in [199u16, 300, 301, 404, 500] {
            assert!(!should_cache(s, &h(&[("cache-control", "public")])), "{s} should not store");
        }
    }

    /// RFC 9111 5.2.1.5: a request carrying no-store must not have its
    /// response written to cache. Request directives were not parsed at all.
    #[test]
    fn request_no_store_forbids_storage() {
        assert!(!request_permits_storage(&h(&[("cache-control", "no-store")])));
        assert!(!request_permits_storage(&h(&[("Cache-Control", "No-Store")])));
    }

    /// Request `no-cache` means "revalidate before reuse", NOT "do not store".
    /// Treating them alike would cost hit rate for no correctness gain.
    #[test]
    fn request_no_cache_does_not_forbid_storage() {
        assert!(request_permits_storage(&h(&[("cache-control", "no-cache")])));
        assert!(request_permits_storage(&h(&[])));
    }
}

#[cfg(test)]
mod age_tests {
    use super::*;

    fn resp(headers: &[(&str, &str)]) -> CachedResponse {
        CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(
                headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ),
            body: bytes::Bytes::from_static(b"x"),
            hints: std::sync::Arc::new(vec![]),
        }
    }

    /// A freshly stored entry is age ~0.
    #[test]
    fn a_fresh_entry_starts_at_about_zero() {
        let c = Cache::new();
        c.insert(CacheKey::new("/a", None, ""), resp(&[("cache-control", "public, max-age=60")]));
        match c.lookup("/a\u{1}\u{1}") {
            Lookup::Fresh(_, age) => assert!(age.as_secs() < 2, "age was {age:?}"),
            other => panic!("expected Fresh, got {}", match other {
                Lookup::Stale(..) => "Stale", _ => "Miss" }),
        }
    }

    /// RFC 9111 4.2.3: age is time since the response was GENERATED, so an
    /// `Age` the upstream already declared carries forward. Resetting it to
    /// zero at each hop is what makes a chain of caches report stale content
    /// as new.
    #[test]
    fn upstream_age_is_carried_forward() {
        let c = Cache::new();
        c.insert(
            CacheKey::new("/b", None, ""),
            resp(&[("cache-control", "public, max-age=600"), ("age", "120")]),
        );
        match c.lookup("/b\u{1}\u{1}") {
            Lookup::Fresh(_, age) => {
                assert!(age.as_secs() >= 120, "upstream Age was dropped; got {age:?}");
                assert!(age.as_secs() < 125, "age inflated beyond the upstream value: {age:?}");
            }
            _ => panic!("expected Fresh"),
        }
    }

    /// A malformed upstream Age must not poison the calculation.
    #[test]
    fn unparseable_upstream_age_is_ignored() {
        let c = Cache::new();
        c.insert(
            CacheKey::new("/c", None, ""),
            resp(&[("cache-control", "public, max-age=60"), ("age", "not-a-number")]),
        );
        match c.lookup("/c\u{1}\u{1}") {
            Lookup::Fresh(_, age) => assert!(age.as_secs() < 2, "age was {age:?}"),
            _ => panic!("expected Fresh"),
        }
    }

    /// An entry with no freshness directive still reports an age.
    #[test]
    fn an_entry_without_a_lifetime_still_reports_age() {
        let c = Cache::new();
        c.insert(CacheKey::new("/d", None, ""), resp(&[("cache-control", "public"), ("age", "7")]));
        match c.lookup("/d\u{1}\u{1}") {
            Lookup::Fresh(_, age) => assert!(age.as_secs() >= 7),
            _ => panic!("expected Fresh"),
        }
    }
}

#[cfg(test)]
mod heuristic_freshness_tests {
    use super::*;

    fn resp(cc: &str) -> CachedResponse {
        CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![("cache-control".into(), cc.into())]),
            body: bytes::Bytes::from_static(b"x"),
            hints: std::sync::Arc::new(vec![]),
        }
    }

    /// A bare `public` used to store with `expires_at = None` and stay fresh
    /// forever. It is still long-lived — the deploy pipeline's invalidation
    /// model depends on that — but it now has an upper bound, so a missed
    /// invalidation is a bounded fault rather than a permanent one.
    #[test]
    fn bare_public_is_bounded_not_eternal() {
        let c = Cache::new();
        c.insert(CacheKey::new("/p", None, ""), resp("public"));
        // Still fresh now, which is the behaviour the deploy model relies on.
        assert!(matches!(c.lookup("/p\u{1}\u{1}"), Lookup::Fresh(..)));
        // But it has a finite deadline rather than none at all.
        let map = c.map.read().unwrap();
        let e = map.get("/p\u{1}\u{1}").expect("stored");
        assert!(e.expires_at.is_some(), "bare `public` must not be fresh forever");
    }

    /// An explicit lifetime is still honoured exactly and is not replaced by
    /// the heuristic.
    #[test]
    fn an_explicit_max_age_is_unaffected() {
        let c = Cache::new();
        c.insert(CacheKey::new("/q", None, ""), resp("public, max-age=60"));
        let map = c.map.read().unwrap();
        let e = map.get("/q\u{1}\u{1}").expect("stored");
        let ttl = e.expires_at.unwrap().saturating_duration_since(std::time::Instant::now());
        assert!(ttl.as_secs() <= 60, "explicit max-age was overridden: {ttl:?}");
        assert!(ttl.as_secs() > 30, "explicit max-age was truncated: {ttl:?}");
    }
}

#[cfg(test)]
mod expires_tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn at(offset_secs: i64) -> String {
        let t = if offset_secs >= 0 {
            SystemTime::now() + Duration::from_secs(offset_secs as u64)
        } else {
            SystemTime::now() - Duration::from_secs((-offset_secs) as u64)
        };
        httpdate::fmt_http_date(t)
    }

    fn stored(headers: Vec<(String, String)>) -> Option<std::time::Instant> {
        let c = Cache::new();
        c.insert(CacheKey::new("/e", None, ""), CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(headers),
            body: bytes::Bytes::from_static(b"x"),
            hints: std::sync::Arc::new(vec![]),
        });
        let map = c.map.read().unwrap();
        map.get("/e\u{1}\u{1}").and_then(|e| e.expires_at)
    }

    /// `Expires` was ignored entirely, so a response carrying an explicit
    /// expiry in the older header fell through to "no freshness given" and
    /// was treated as fresh indefinitely — exactly backwards.
    #[test]
    fn expires_sets_the_lifetime() {
        let e = stored(vec![
            ("cache-control".into(), "public".into()),
            ("date".into(), at(0)),
            ("expires".into(), at(120)),
        ])
        .expect("stored with a deadline");
        let ttl = e.saturating_duration_since(std::time::Instant::now());
        assert!(ttl.as_secs() > 60 && ttl.as_secs() <= 120, "ttl was {ttl:?}, expected ~120s");
    }

    /// Measured against the response's own Date, not our clock, so clock skew
    /// between servers does not change the lifetime it asked for.
    #[test]
    fn lifetime_is_relative_to_the_response_date() {
        // A server an hour fast: Date and Expires are both shifted, but the
        // interval between them is still 60s.
        let e = stored(vec![
            ("cache-control".into(), "public".into()),
            ("date".into(), at(3600)),
            ("expires".into(), at(3660)),
        ])
        .expect("stored");
        let ttl = e.saturating_duration_since(std::time::Instant::now());
        assert!(ttl.as_secs() <= 60, "skew leaked into the lifetime: {ttl:?}");
    }

    /// max-age outranks Expires (RFC 9111 4.2.1).
    #[test]
    fn max_age_wins_over_expires() {
        let e = stored(vec![
            ("cache-control".into(), "public, max-age=30".into()),
            ("date".into(), at(0)),
            ("expires".into(), at(86400)),
        ])
        .expect("stored");
        let ttl = e.saturating_duration_since(std::time::Instant::now());
        assert!(ttl.as_secs() <= 30, "Expires overrode max-age: {ttl:?}");
    }

    /// An Expires already in the past means stale, not a wrapped negative.
    #[test]
    fn a_past_expires_is_immediately_stale() {
        let e = stored(vec![
            ("cache-control".into(), "public".into()),
            ("date".into(), at(0)),
            ("expires".into(), at(-3600)),
        ])
        .expect("stored");
        assert!(
            e <= std::time::Instant::now() + Duration::from_secs(1),
            "a past Expires should not produce a live deadline"
        );
    }

    /// Garbage in Expires must not be mistaken for an expiry.
    #[test]
    fn an_unparseable_expires_falls_back_to_the_heuristic() {
        let e = stored(vec![
            ("cache-control".into(), "public".into()),
            ("expires".into(), "not-a-date".into()),
        ])
        .expect("stored");
        let ttl = e.saturating_duration_since(std::time::Instant::now());
        assert!(ttl.as_secs() > 3600, "should have fallen back to the heuristic, got {ttl:?}");
    }
}

#[cfg(test)]
mod not_modified_header_tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// RFC 9110 15.4.5: a 304 carries the metadata a 200 would have, so the
    /// client can update its stored response from it.
    #[test]
    fn a_304_carries_the_selected_representation_metadata() {
        let out = not_modified_headers(&h(&[
            ("etag", "\"abc\""),
            ("last-modified", "Sat, 05 Sep 2026 12:00:00 GMT"),
            ("cache-control", "public, max-age=60"),
            ("vary", "Accept-Encoding"),
            ("date", "Sat, 05 Sep 2026 12:00:01 GMT"),
        ]));
        let names: Vec<String> = out.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        for want in ["etag", "last-modified", "cache-control", "vary", "date"] {
            assert!(names.contains(&want.to_string()), "304 dropped {want}");
        }
    }

    /// The damaging omission. A client updating its stored entry from a 304
    /// that lost `Vary` no longer knows the response varies by encoding, and
    /// can then reuse a brotli body for a gzip-only request.
    #[test]
    fn vary_survives_onto_the_304() {
        let out = not_modified_headers(&h(&[("vary", "Accept-Encoding"), ("etag", "\"x\"")]));
        assert!(
            out.iter().any(|(k, v)| k.eq_ignore_ascii_case("vary") && v == "Accept-Encoding"),
            "Vary was dropped from the 304"
        );
    }

    /// Content-bearing headers must not ride along on a bodyless response.
    #[test]
    fn content_headers_are_not_carried() {
        let out = not_modified_headers(&h(&[
            ("etag", "\"x\""),
            ("content-length", "54361"),
            ("content-type", "text/html"),
            ("content-encoding", "br"),
        ]));
        let names: Vec<String> = out.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        for unwanted in ["content-length", "content-type", "content-encoding"] {
            assert!(!names.contains(&unwanted.to_string()), "304 carried {unwanted}");
        }
    }
}
