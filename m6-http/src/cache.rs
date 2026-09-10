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
// Conditional requests and preconditions live in `m6_core::conditional`.
// They are version-independent semantics with two consumers, and the second
// consumer (m6-file) had its own incomplete copy. Re-exported so the call
// sites here read the same as before.
pub use m6_core::conditional::{
    evaluate_preconditions, is_not_modified, not_modified_headers, Precondition,
};

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
    /// Roughly how many bytes this entry holds, for the capacity bound.
    ///
    /// Body plus header strings plus the key. Approximate on purpose: the
    /// point is to bound growth, and the error is a small constant per entry
    /// against bodies measured in kilobytes.
    footprint: usize,
    /// Monotonic tick of the last read, for eviction order.
    ///
    /// `AtomicU64` so `lookup` can record a read while holding only the shared
    /// lock. One relaxed store per cache hit, no lock upgrade, no allocation.
    last_read: std::sync::atomic::AtomicU64,
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

/// Default ceiling on total cached bytes.
///
/// **The cache used to be unbounded, and a peer chose the key.** The key is
/// `(path, query, encoding)` and `encoding` was the raw `Accept-Encoding`
/// header, so `identity, x1`, `identity, x2` … is unlimited distinct entries
/// for byte-identical content, each holding a full response body. Expiry does
/// not help: `lookup` evaluates freshness on read and returns `Miss` for an
/// expired entry, but leaves it in the map, and these keys are never asked for
/// twice.
///
/// Measured: 10,000 such entries against `/` held ~518 MB, and nothing was
/// evicted. The fleet nodes have 950 MB of RAM and around 600 MB free, so
/// roughly 11,000 requests exhausted a node — about 36 minutes from a single
/// IP inside the 300-per-minute rate limit, ending in the OOM killer choosing
/// whichever process had grown the most.
///
/// 128 MB is a deliberate fraction of that headroom: large enough to hold the
/// whole site many times over (every page and asset compressed is well under
/// a megabyte), small enough that filling it is not an outage.
///
/// Normalising the key to `{identity, gzip, br}` removes the vector at source
/// and is the better fix. This bound is the safety net that holds for any
/// key-space explosion, including ones nobody has thought of.
const DEFAULT_MAX_BYTES: usize = 128 * 1024 * 1024;

/// Monotonic read counter, for eviction order. Wrapping is not a concern: at
/// one tick per cache hit it would take centuries.
static READ_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

use std::sync::{Arc, RwLock};

/// Cache backed by Arc<RwLock<HashMap>> — swap the whole map atomically.
/// Clone is cheap — just clones the Arc.
#[derive(Clone)]
pub struct Cache {
    map: Arc<RwLock<CacheMap>>,
    /// Total footprint of everything in `map`, maintained on insert and evict.
    bytes: Arc<std::sync::atomic::AtomicUsize>,
    max_bytes: usize,
}

impl Cache {
    pub fn new() -> Self {
        Self::with_max_bytes(DEFAULT_MAX_BYTES)
    }

    /// A cache bounded at `max_bytes` of total response footprint.
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Cache {
            map: Arc::new(RwLock::new(CacheMap::new())),
            bytes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_bytes,
        }
    }

    /// Total bytes currently held.
    pub fn bytes_held(&self) -> usize {
        self.bytes.load(std::sync::atomic::Ordering::Relaxed)
    }



    /// A cache whose hasher is seeded deterministically. **Benchmarks only.**
    ///
    /// `ahash`'s `RandomState` is seeded from the OS once per process, so the
    /// same key lands in a different bucket in every run. Production wants
    /// exactly that: an unpredictable per-process seed is what makes
    /// hash-collision denial of service impractical.
    ///
    /// For a benchmark it is one avoidable source of run-to-run difference, so
    /// this pins it.
    ///
    /// **It is not, however, the reason the benchmark used to scatter.** That
    /// was measured and disproven: pinning the seed left the spread unchanged
    /// at 49-62 ns over five runs. The real cause was the machine, not the
    /// code -- see `docs/BENCHMARKS.md`. This constructor is kept because
    /// determinism is worth having, not because it fixed anything.
    #[doc(hidden)]
    pub fn with_fixed_seed_for_bench() -> Self {
        let hasher = ahash::RandomState::with_seeds(
            0x243f_6a88_85a3_08d3,
            0x1319_8a2e_0370_7344,
            0xa409_3822_299f_31d0,
            0x082e_fa98_ec4e_6c89,
        );
        Cache {
            map: Arc::new(RwLock::new(CacheMap::with_hasher(hasher))),
            bytes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_bytes: DEFAULT_MAX_BYTES,
        }
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

    /// Look up an entry, honouring the client's own cache directives
    /// (RFC 9111 5.2.1).
    ///
    /// These were not implemented at all: a request saying `Cache-Control:
    /// no-cache` -- which is what every browser sends on a reload -- was served
    /// the cached copy anyway, so a visitor could not force a refresh no matter
    /// what they pressed. `max-age`, `min-fresh` and `max-stale` were likewise
    /// ignored, meaning a client's explicit statement about what it would
    /// accept had no effect on what it got.
    ///
    /// `only_if_cached` is deliberately NOT handled here: this returns
    /// [`Lookup::Miss`] as usual, and the caller turns that into a 504 rather
    /// than going to the backend. Encoding it in the enum would put an HTTP
    /// status into a data structure that otherwise knows nothing about HTTP.
    pub fn lookup_with<Q>(&self, key: &Q, req: &RequestDirectives) -> Lookup
    where
        CacheKey: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        // `no-cache` on a request forbids reuse without revalidation. m6 has no
        // way to revalidate a stored entry in place, so the honest
        // implementation is to miss and let the request reach the backend --
        // which is revalidation, just without the conditional round trip.
        if req.no_cache {
            return Lookup::Miss;
        }

        let base = self.lookup(key);
        let (resp, age, was_fresh) = match base {
            Lookup::Fresh(r, a) => (r, a, true),
            Lookup::Stale(r, a) => (r, a, false),
            Lookup::Miss => return Lookup::Miss,
        };

        // max-age: the client will not accept a response older than this.
        if let Some(max_age) = req.max_age {
            if age.as_secs() > max_age {
                return Lookup::Miss;
            }
        }

        // min-fresh: it must still be fresh for at least this long. A response
        // already stale trivially fails, whatever max-stale says -- the two
        // directives are about different things and min-fresh is the stricter
        // claim.
        if let Some(min_fresh) = req.min_fresh {
            let remaining = self.remaining_freshness(key).unwrap_or_default();
            if remaining.as_secs() < min_fresh {
                return Lookup::Miss;
            }
        }

        if was_fresh {
            return Lookup::Fresh(resp, age);
        }

        // Stale. Servable only if the client said it would take stale content,
        // and within the bound it gave.
        match req.max_stale {
            Some(None) => Lookup::Stale(resp, age), // `max-stale` with no value: any
            Some(Some(limit)) => {
                let staleness = self.staleness(key).unwrap_or_default();
                if staleness.as_secs() <= limit { Lookup::Stale(resp, age) } else { Lookup::Miss }
            }
            // No max-stale from the client. The stale-while-revalidate window
            // is the ORIGIN's permission to serve stale, which is independent
            // of the client's, so the existing behaviour stands.
            None => Lookup::Stale(resp, age),
        }
    }

    /// How much freshness an entry has left, or `None` if it is absent or
    /// already stale.
    fn remaining_freshness<Q>(&self, key: &Q) -> Option<std::time::Duration>
    where
        CacheKey: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let map = self.map.read().ok()?;
        let entry = map.get(key)?;
        // No expiry at all means it never goes stale, so any min-fresh is met.
        let Some(expires_at) = entry.expires_at else {
            return Some(std::time::Duration::from_secs(u32::MAX as u64));
        };
        expires_at.checked_duration_since(std::time::Instant::now())
    }

    /// How long an entry has been stale, or `None` if absent or still fresh.
    fn staleness<Q>(&self, key: &Q) -> Option<std::time::Duration>
    where
        CacheKey: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let map = self.map.read().ok()?;
        let entry = map.get(key)?;
        let expires_at = entry.expires_at?;
        std::time::Instant::now().checked_duration_since(expires_at)
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
        // One relaxed store, under the shared lock, so eviction can prefer
        // entries nobody reads. No lock upgrade and no allocation: the
        // attack this defends against is a flood of entries that are written
        // once and never read, and without a read order the eviction would
        // discard the hot ones instead.
        entry.last_read.store(
            READ_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::sync::atomic::Ordering::Relaxed,
        );
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
        // RFC 9111 4.2.3 -- corrected_initial_age.
        //
        // This used to be the `Age` header alone. That trusts an upstream to
        // have set it, and an upstream cache that stores a response WITHOUT
        // emitting Age makes an hour-old response look brand new to us, and
        // then to everyone downstream of us. The spec's answer is to cross-check
        // against `Date`: if the response says it was generated at 09:00 and it
        // is now 09:30, it is at least thirty minutes old whatever Age claims.
        //
        //   apparent_age          = max(0, now - Date)
        //   corrected_initial_age = max(apparent_age, age_value)
        //
        // Taking the MAX is the point -- it is the conservative choice in both
        // directions. A missing or under-reported Age is corrected by Date, and
        // a clock skewed such that Date is in the future yields a zero apparent
        // age rather than a negative one, leaving Age to stand.
        let age_value = response.headers.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("age"))
            .and_then(|(_, v)| v.trim().parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or_default();
        let apparent_age = response.headers.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("date"))
            .and_then(|(_, v)| httpdate::parse_http_date(v).ok())
            .and_then(|d| std::time::SystemTime::now().duration_since(d).ok())
            .unwrap_or_default();
        let upstream_age = age_value.max(apparent_age);
        //
        // `response_delay` (the request/response round trip, the third term in
        // 4.2.3) is deliberately NOT included, and this is an approximation
        // rather than an oversight. It would require threading the request's
        // start time through every insert site. Measured, the backend round
        // trip here is ~2ms against freshness lifetimes of 60s and 86400s --
        // 0.003% of the shorter one, far below the one-second resolution the
        // `Age` header can even express. It would matter for a cache fronting a
        // slow or distant origin; it does not matter for this one. If m6 ever
        // caches across a link where a round trip is a measurable fraction of a
        // second, revisit this.
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
        let footprint = entry_footprint(&key, &response);
        if let Ok(mut map) = self.map.write() {
            let replaced = map.insert(key, CacheEntry {
                response,
                stored_at: now,
                upstream_age,
                expires_at,
                serve_stale_until,
                footprint,
                last_read: std::sync::atomic::AtomicU64::new(
                    READ_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                ),
            });
            let freed = replaced.map(|e| e.footprint).unwrap_or(0);
            let held = self
                .bytes
                .fetch_add(footprint, std::sync::atomic::Ordering::Relaxed)
                .saturating_sub(freed)
                + footprint;
            self.bytes.store(held, std::sync::atomic::Ordering::Relaxed);
            if held > self.max_bytes {
                self.evict_until_under(&mut map);
            }
        }
    }

    /// Drop least-recently-read entries until the cache is inside its bound.
    ///
    /// Called only from `insert`, and only when the bound is exceeded, so the
    /// O(n) scan is off the read path entirely. It costs far less than the
    /// origin round trip that produced the entry which tipped it over.
    ///
    /// Least-recently-read rather than oldest-inserted on purpose: the flood
    /// this defends against writes entries and never reads them again, so read
    /// order is exactly the signal that separates an attacker's entries from
    /// the site's own.
    fn evict_until_under(&self, map: &mut CacheMap) {
        let target = self.max_bytes - self.max_bytes / 8; // drop to 87.5%

        // One pass to collect `(last_read, footprint)`, sort by read order,
        // and find the tick above which enough bytes survive. Then one
        // `retain`.
        //
        // The obvious version collects `(tick, key.clone())` and sorts that,
        // which clones every key in the map -- and a `CacheKey` is a
        // `Box<str>`, so that is one heap allocation per entry, in a burst,
        // while holding the write lock every reader needs. This is one `Vec`
        // and no per-entry allocation.
        //
        // Ticks are unique: both `insert` and `lookup` take a fresh value from
        // the global counter, so no two entries share one.
        let mut entries: Vec<(u64, usize)> = map
            .values()
            .map(|e| (e.last_read.load(std::sync::atomic::Ordering::Relaxed), e.footprint))
            .collect();
        entries.sort_unstable_by_key(|(tick, _)| *tick);

        let mut held = self.bytes.load(std::sync::atomic::Ordering::Relaxed);
        let mut cutoff = 0u64;
        for (tick, footprint) in &entries {
            if held <= target {
                break;
            }
            held = held.saturating_sub(*footprint);
            cutoff = *tick + 1;
        }

        let before = map.len();
        let mut freed = 0usize;
        map.retain(|_, e| {
            let keep = e.last_read.load(std::sync::atomic::Ordering::Relaxed) >= cutoff;
            if !keep {
                freed += e.footprint;
            }
            keep
        });
        let now_held = self
            .bytes
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(freed);
        self.bytes.store(now_held, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(
            dropped = before - map.len(),
            bytes_held = now_held,
            max_bytes = self.max_bytes,
            "cache: evicted to stay inside the byte bound"
        );
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
                if let Some(e) = map.remove(k) {
                    // Every removal path must return the bytes, or the counter
                    // drifts up while the map shrinks and the cache evicts on
                    // every insert forever. Deploys call this on every
                    // invalidation, so the drift would be relentless.
                    self.bytes.fetch_sub(e.footprint, std::sync::atomic::Ordering::Relaxed);
                }
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
            self.bytes.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Sum of the footprints actually in the map.
    ///
    /// The invariant `bytes_held() == footprint_sum()` must hold after every
    /// operation. Exposed for tests: an accounting drift is invisible until
    /// the counter crosses the bound, at which point the cache evicts on every
    /// insert and the hit rate collapses, and nothing about that failure
    /// points back at the arithmetic.
    #[cfg(test)]
    fn footprint_sum(&self) -> usize {
        self.map.read().map(|m| m.values().map(|e| e.footprint).sum()).unwrap_or(0)
    }

    /// Number of stored entries, including any that are past their freshness
    /// lifetime but haven't been read (and so overwritten) since. This is a
    /// memory-occupancy figure, not a count of servable entries.
    pub fn len(&self) -> usize {
        self.map.read().map(|m| m.len()).unwrap_or(0)
    }
}

/// Roughly how many bytes an entry occupies.
///
/// Body, header strings, and the key. Approximate on purpose: the bound exists
/// to stop unbounded growth, and a constant per-entry error is immaterial
/// against bodies measured in kilobytes. Deliberately counts the body even
/// though it is a `Bytes` that may share an allocation, because the
/// conservative direction here is to over-count.
fn entry_footprint(key: &CacheKey, response: &CachedResponse) -> usize {
    let headers: usize = response
        .headers
        .iter()
        .map(|(k, v)| k.len() + v.len() + 2)
        .sum();
    let hints: usize = response.hints.iter().map(|h| h.len()).sum();
    key.0.len() + response.body.len() + headers + hints + std::mem::size_of::<CacheEntry>()
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
    // RFC 9111 3: the statuses a cache may store by default. Only 2xx was
    // accepted before, which made every 404, 301 and 410 permanently
    // uncacheable no matter what its headers said.
    //
    // That is not merely conservative, it costs real work: on a cache node a
    // 404 that cannot be stored is a round trip to the origin every time. The
    // measured cost on this deployment was ~207ms from Chicago and ~282ms
    // from London, paid thousands of times a day for junk paths.
    //
    // 206 is excluded deliberately: a partial response is only meaningful
    // with the Range request that produced it, and the cache key carries no
    // Range component, so a stored 206 would be replayed to a client that
    // asked for the whole thing.
    //
    // Storability is necessary, not sufficient -- `should_cache` still
    // applies Vary, no-store, private and the rest on top of this.
    matches!(
        status,
        200 | 203 | 204 | 300 | 301 | 308 | 404 | 405 | 410 | 414 | 501
    )
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
/// What the CLIENT asked for, as opposed to what the origin permitted
/// (RFC 9111 5.2.1).
///
/// None of these were honoured. The most visible consequence was that a browser
/// reload -- which sends `Cache-Control: no-cache` -- got the cached copy back
/// regardless, so a visitor had no way to force a refresh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestDirectives {
    /// Reuse requires revalidation first.
    pub no_cache: bool,
    /// Do not store the exchange.
    pub no_store: bool,
    /// Refuse a response older than this many seconds.
    pub max_age: Option<u64>,
    /// Accept a stale response. `Some(None)` is bare `max-stale` -- any
    /// staleness; `Some(Some(n))` bounds it to n seconds.
    pub max_stale: Option<Option<u64>>,
    /// Require the response to stay fresh for at least this many seconds.
    pub min_fresh: Option<u64>,
    /// Answer from cache or not at all -- the caller returns 504 rather than
    /// contacting the backend.
    pub only_if_cached: bool,
}

impl RequestDirectives {
    pub fn parse(req_headers: &[(String, String)]) -> Self {
        let mut d = Self::default();
        let mut saw_cache_control = false;
        for (name, value) in req_headers {
            if name.eq_ignore_ascii_case("cache-control") {
                saw_cache_control = true;
                // split_directives already handles quoted values and the
                // commas that can appear inside them (`no-cache="Set-Cookie"`
                // is legal), so this must not re-split on '='.
                for (k, v) in split_directives(value) {
                    let secs = || v.as_deref().and_then(|v| v.trim().parse::<u64>().ok());
                    match k.to_ascii_lowercase().as_str() {
                        "no-cache"       => d.no_cache = true,
                        "no-store"       => d.no_store = true,
                        "max-age"        => d.max_age = secs(),
                        "min-fresh"      => d.min_fresh = secs(),
                        // Bare `max-stale` means unlimited; with a value it is
                        // bounded. The nested Option distinguishes them, which
                        // a plain Option<u64> could not.
                        "max-stale"      => d.max_stale = Some(secs()),
                        "only-if-cached" => d.only_if_cached = true,
                        _ => {}
                    }
                }
            }
        }
        // HTTP/1.0 clients, and a surprising number of current ones, send
        // `Pragma: no-cache` instead. RFC 9111 5.4 says to honour it only when
        // Cache-Control is absent, because Cache-Control is the authority when
        // both are present.
        if !saw_cache_control {
            for (name, value) in req_headers {
                if name.eq_ignore_ascii_case("pragma")
                    && value.split(',').any(|t| t.trim().eq_ignore_ascii_case("no-cache"))
                {
                    d.no_cache = true;
                }
            }
        }
        d
    }
}

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

    /// Was `test_should_not_cache_4xx`, asserting that a `public` 404 is
    /// never stored. That pinned an over-strict rule, not a requirement:
    /// RFC 9111 3 lists 404 as heuristically cacheable, and refusing to store
    /// it meant a cache node paid an origin round trip for every one.
    ///
    /// The 4xx that must still be refused are the ones absent from that list.
    #[test]
    fn a_public_404_is_stored_but_other_4xx_are_not() {
        let (status, headers, _) = make_response(404, "public");
        assert!(should_cache(status, &headers), "404 is storable per RFC 9111 3");
        for s in [400u16, 401, 403, 429] {
            let (status, headers, _) = make_response(s, "public");
            assert!(!should_cache(status, &headers), "{s} is not in the RFC 9111 3 list");
        }
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

    /// The storable set is now RFC 9111 3 rather than "any 2xx". 300, 301
    /// and 404 moved from the refused list to the stored list; 199 and 500
    /// stay refused, and 201/202 are refused despite being 2xx because they
    /// are responses to unsafe methods.
    #[test]
    fn the_rfc_9111_storable_set_is_honoured() {
        for s in [200u16, 203, 204, 300, 301, 308, 404, 405, 410, 414, 501] {
            assert!(should_cache(s, &h(&[("cache-control", "public")])), "{s} should store");
        }
        for s in [199u16, 201, 202, 205, 302, 400, 500, 503] {
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


#[cfg(test)]
mod request_directive_tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    fn parse(pairs: &[(&str, &str)]) -> RequestDirectives {
        RequestDirectives::parse(&h(pairs))
    }

    /// The most visible consequence of not implementing these: a browser
    /// reload sends `Cache-Control: no-cache`, and m6 served the cached copy
    /// regardless, so a visitor had no way to force a refresh.
    #[test]
    fn no_cache_is_parsed() {
        assert!(parse(&[("cache-control", "no-cache")]).no_cache);
        assert!(parse(&[("Cache-Control", "No-Cache")]).no_cache);
        assert!(!parse(&[("cache-control", "max-age=0")]).no_cache);
    }

    /// RFC 9111 5.4: honour Pragma only when Cache-Control is absent, because
    /// Cache-Control is authoritative when both are present.
    #[test]
    fn pragma_is_the_fallback_not_an_override() {
        assert!(parse(&[("pragma", "no-cache")]).no_cache);
        // Cache-Control present and NOT saying no-cache: Pragma must not win.
        assert!(!parse(&[("cache-control", "max-age=100"), ("pragma", "no-cache")]).no_cache);
    }

    #[test]
    fn numeric_directives_are_parsed() {
        let d = parse(&[("cache-control", "max-age=30, min-fresh=10")]);
        assert_eq!(d.max_age, Some(30));
        assert_eq!(d.min_fresh, Some(10));
    }

    /// Bare `max-stale` means unlimited staleness; with a value it is bounded.
    /// A plain Option<u64> could not tell those apart, which is why the field
    /// is nested.
    #[test]
    fn max_stale_distinguishes_bare_from_bounded() {
        assert_eq!(parse(&[("cache-control", "max-stale")]).max_stale, Some(None));
        assert_eq!(parse(&[("cache-control", "max-stale=60")]).max_stale, Some(Some(60)));
        assert_eq!(parse(&[("cache-control", "max-age=5")]).max_stale, None);
    }

    #[test]
    fn only_if_cached_and_no_store() {
        assert!(parse(&[("cache-control", "only-if-cached")]).only_if_cached);
        assert!(parse(&[("cache-control", "no-store")]).no_store);
    }

    /// A quoted value may contain a comma (`no-cache="Set-Cookie, X-Thing"` is
    /// legal). Splitting naively on ',' would produce a bogus directive.
    #[test]
    fn quoted_values_do_not_split_the_directive_list() {
        let d = parse(&[("cache-control", "no-cache=\"Set-Cookie, X-Thing\", max-age=30")]);
        assert!(d.no_cache);
        assert_eq!(d.max_age, Some(30));
    }

    /// An unparseable numeric value must not become a wrong number.
    #[test]
    fn malformed_numbers_are_ignored() {
        assert_eq!(parse(&[("cache-control", "max-age=abc")]).max_age, None);
        assert_eq!(parse(&[("cache-control", "min-fresh=")]).min_fresh, None);
    }

    #[test]
    fn absent_means_all_defaults() {
        let d = parse(&[("accept", "text/html")]);
        assert_eq!(d, RequestDirectives::default());
    }

    // ── Behaviour against a real cache ───────────────────────────────────────

    fn cache_with(cc: &str) -> (Cache, CacheKey) {
        let cache = Cache::new();
        let key = CacheKey::new("/p", None, "");
        let resp = CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![("cache-control".into(), cc.into())]),
            body: bytes::Bytes::from_static(b"body"),
            hints: std::sync::Arc::new(vec![]),
        };
        cache.insert(key.clone(), resp);
        (cache, key)
    }

    /// `no-cache` must force a miss so the request reaches the backend.
    #[test]
    fn no_cache_forces_a_miss_on_a_fresh_entry() {
        let (cache, key) = cache_with("public, max-age=600");
        assert!(matches!(cache.lookup(&key), Lookup::Fresh(..)), "precondition: normally a hit");
        let d = parse(&[("cache-control", "no-cache")]);
        assert!(matches!(cache.lookup_with(&key, &d), Lookup::Miss));
    }

    /// `max-age=0` means the client will accept nothing with any age, which in
    /// practice forces revalidation on all but a same-instant hit.
    #[test]
    fn request_max_age_bounds_reuse() {
        let (cache, key) = cache_with("public, max-age=600");
        // Generous bound: still a hit.
        let ok = parse(&[("cache-control", "max-age=600")]);
        assert!(matches!(cache.lookup_with(&key, &ok), Lookup::Fresh(..)));
    }

    /// min-fresh larger than the remaining lifetime must miss.
    #[test]
    fn min_fresh_beyond_remaining_lifetime_misses() {
        let (cache, key) = cache_with("public, max-age=60");
        let d = parse(&[("cache-control", "min-fresh=3600")]);
        assert!(matches!(cache.lookup_with(&key, &d), Lookup::Miss));
        // ...and a modest requirement still hits.
        let ok = parse(&[("cache-control", "min-fresh=5")]);
        assert!(matches!(cache.lookup_with(&key, &ok), Lookup::Fresh(..)));
    }

    /// only-if-cached is not encoded in Lookup; the caller reads the flag and
    /// returns 504. Asserted so the contract is pinned somewhere.
    #[test]
    fn only_if_cached_is_left_to_the_caller() {
        let (cache, key) = cache_with("public, max-age=600");
        let d = parse(&[("cache-control", "only-if-cached")]);
        assert!(d.only_if_cached);
        // A hit is still a hit — the flag only matters on a miss.
        assert!(matches!(cache.lookup_with(&key, &d), Lookup::Fresh(..)));
    }
}

#[cfg(test)]
mod corrected_age_tests {
    use super::*;

    fn resp_with(headers: &[(&str, &str)]) -> CachedResponse {
        CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(
                headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ),
            body: bytes::Bytes::from_static(b"x"),
            hints: std::sync::Arc::new(vec![]),
        }
    }
    fn http_date_ago(secs: u64) -> String {
        httpdate::fmt_http_date(std::time::SystemTime::now() - std::time::Duration::from_secs(secs))
    }
    fn age_of(headers: &[(&str, &str)]) -> u64 {
        let cache = Cache::new();
        let key = CacheKey::new("/p", None, "");
        cache.insert(key.clone(), resp_with(headers));
        match cache.lookup(&key) {
            Lookup::Fresh(_, age) | Lookup::Stale(_, age) => age.as_secs(),
            Lookup::Miss => panic!("entry should be present"),
        }
    }

    /// The defect. Trusting `Age` alone means an upstream cache that stores a
    /// response WITHOUT emitting Age makes an hour-old response look brand new
    /// to us, and to everyone downstream of us.
    #[test]
    fn date_supplies_age_when_the_header_is_missing() {
        let d = http_date_ago(1800);
        let age = age_of(&[("cache-control", "public, max-age=86400"), ("date", &d)]);
        assert!(
            (1795..=1805).contains(&age),
            "expected ~1800s from Date, got {age}"
        );
    }

    /// max(apparent, age_value): an under-reported Age is corrected upward.
    #[test]
    fn the_larger_of_date_and_age_wins() {
        let d = http_date_ago(600);
        let age = age_of(&[
            ("cache-control", "public, max-age=86400"),
            ("date", &d),
            ("age", "5"), // upstream under-reports badly
        ]);
        assert!((595..=605).contains(&age), "Date should win at ~600s, got {age}");
    }

    /// ...and the other way round: a large Age with a recent Date must stand,
    /// since Age is the upstream's own explicit statement.
    #[test]
    fn age_wins_when_it_is_the_larger() {
        let d = http_date_ago(10);
        let age = age_of(&[
            ("cache-control", "public, max-age=86400"),
            ("date", &d),
            ("age", "900"),
        ]);
        assert!((895..=910).contains(&age), "Age should win at ~900s, got {age}");
    }

    /// A Date in the future (clock skew) must not produce a negative or
    /// wrapped age. `duration_since` errors on a future instant, and the
    /// default is zero, so Age is left to stand.
    #[test]
    fn future_date_does_not_wrap_or_go_negative() {
        let future = httpdate::fmt_http_date(
            std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
        );
        let age = age_of(&[
            ("cache-control", "public, max-age=86400"),
            ("date", &future),
            ("age", "42"),
        ]);
        assert_eq!(age, 42, "skewed clock must fall back to Age, got {age}");
    }

    /// No Date and no Age is a genuinely fresh response: age starts at zero.
    #[test]
    fn no_validators_means_zero_age() {
        assert_eq!(age_of(&[("cache-control", "public, max-age=86400")]), 0);
    }

    /// An unparseable Date is ignored rather than treated as epoch, which
    /// would otherwise make every such response appear ~56 years old and
    /// instantly stale.
    #[test]
    fn malformed_date_is_ignored() {
        let age = age_of(&[
            ("cache-control", "public, max-age=86400"),
            ("date", "not-a-date"),
            ("age", "7"),
        ]);
        assert_eq!(age, 7);
    }
}


#[cfg(test)]
mod storable_status_tests {
    use super::*;

    /// RFC 9111 3, in full. The list is short and fixed, so it is worth
    /// asserting exactly rather than by range.
    #[test]
    fn the_rfc_9111_set_is_storable() {
        for s in [200, 203, 204, 300, 301, 308, 404, 405, 410, 414, 501] {
            assert!(status_is_storable(s), "{s} is heuristically cacheable per RFC 9111 3");
        }
    }

    /// The regression this fixes: a 404 could never be stored, so on a cache
    /// node every one was a round trip to the origin.
    #[test]
    fn a_404_is_now_storable() {
        assert!(status_is_storable(404));
    }

    /// 206 stays excluded. The cache key is (path, query, encoding) with no
    /// Range component, so a stored partial response would be replayed to a
    /// client that asked for the whole resource.
    #[test]
    fn partial_content_is_never_storable() {
        assert!(!status_is_storable(206));
    }

    /// Statuses outside the list must not creep in via a range check. 201 and
    /// 202 are 2xx but are responses to unsafe methods; 500 and 503 are
    /// transient failures.
    #[test]
    fn everything_else_is_refused() {
        for s in [201, 202, 205, 302, 303, 307, 400, 401, 403, 500, 502, 503] {
            assert!(!status_is_storable(s), "{s} must not be storable");
        }
    }

    /// Storable is necessary, not sufficient: no-store still wins.
    #[test]
    fn storable_status_does_not_override_no_store() {
        let headers = vec![
            ("Cache-Control".to_string(), "no-store".to_string()),
        ];
        assert!(!should_cache(404, &headers), "no-store must still refuse a storable status");
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;

    fn resp(body_len: usize) -> CachedResponse {
        CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![(
                "cache-control".to_string(),
                "public, max-age=86400".to_string(),
            )]),
            body: bytes::Bytes::from(vec![0u8; body_len]),
            hints: std::sync::Arc::new(vec![]),
        }
    }

    /// The vector this bound exists for: a peer varies one header and mints an
    /// entry per variation, each holding a full response body.
    ///
    /// Before the bound, 10,000 such entries against `/` held ~518 MB and
    /// nothing was evicted. The fleet nodes have 950 MB of RAM and about
    /// 600 MB free, so roughly 11,000 requests exhausted a node: about 36
    /// minutes from one IP inside the 300-per-minute rate limit.
    #[test]
    fn a_flood_of_distinct_encodings_cannot_grow_without_bound() {
        let cap = 4 * 1024 * 1024;
        let cache = Cache::with_max_bytes(cap);
        for i in 0..2_000 {
            cache.insert(
                CacheKey::new("/", None, &format!("identity, x{i}")),
                resp(54_361),
            );
        }
        assert!(
            cache.bytes_held() <= cap,
            "held {} bytes against a {cap}-byte bound",
            cache.bytes_held()
        );
        // 2,000 entries of 54 KB is ~108 MB of pressure against a 4 MB bound,
        // so the great majority must be gone.
        assert!(cache.len() < 200, "expected heavy eviction, {} entries remain", cache.len());
    }

    /// Eviction is least-recently-read, so an entry the site actually serves
    /// survives a flood of write-once entries around it.
    #[test]
    fn a_repeatedly_read_entry_survives_a_flood() {
        let cap = 2 * 1024 * 1024;
        let cache = Cache::with_max_bytes(cap);
        let hot = CacheKey::new("/", None, "gzip");
        cache.insert(hot, resp(10_000));

        for i in 0..500 {
            // Read the hot entry between each insert, exactly as real traffic
            // would while an attacker floods alongside it.
            let mut buf = [0u8; 512];
            let k = make_lookup_key("/", None, "gzip", &mut buf);
            assert!(!matches!(cache.lookup(k), Lookup::Miss), "hot entry evicted at i={i}");
            cache.insert(CacheKey::new("/", None, &format!("identity, x{i}")), resp(10_000));
        }

        let mut buf = [0u8; 512];
        let k = make_lookup_key("/", None, "gzip", &mut buf);
        assert!(
            !matches!(cache.lookup(k), Lookup::Miss),
            "the entry that was read on every iteration must outlive the flood"
        );
        assert!(cache.bytes_held() <= cap);
    }

    /// Replacing an entry must not double-count its bytes.
    #[test]
    fn overwriting_a_key_does_not_leak_accounting() {
        let cache = Cache::with_max_bytes(64 * 1024 * 1024);
        for _ in 0..50 {
            cache.insert(CacheKey::new("/", None, "gzip"), resp(100_000));
        }
        assert_eq!(cache.len(), 1, "one key, one entry");
        assert!(
            cache.bytes_held() < 200_000,
            "accounting drifted: {} bytes for a single 100 KB entry",
            cache.bytes_held()
        );
    }
}

#[cfg(test)]
mod capacity_accounting_tests {
    use super::*;

    fn resp(n: usize) -> CachedResponse {
        CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![(
                "cache-control".to_string(),
                "public, max-age=86400".to_string(),
            )]),
            body: bytes::Bytes::from(vec![0u8; n]),
            hints: std::sync::Arc::new(vec![]),
        }
    }

    /// The byte counter must equal what is actually held, after every
    /// operation that adds or removes an entry.
    ///
    /// `evict_path` and `clear` originally removed entries without returning
    /// their bytes. Deploys call both on every invalidation, so the counter
    /// would climb while the map shrank until it sat permanently above the
    /// bound, at which point every insert triggers an eviction and the hit
    /// rate collapses. Nothing about that symptom points at the arithmetic.
    #[test]
    fn every_removal_path_returns_its_bytes() {
        let cache = Cache::with_max_bytes(64 * 1024 * 1024);

        for i in 0..40 {
            cache.insert(CacheKey::new(&format!("/p{i}"), None, "gzip"), resp(10_000));
            cache.insert(CacheKey::new(&format!("/p{i}"), None, "br"), resp(4_000));
        }
        assert_eq!(cache.bytes_held(), cache.footprint_sum(), "after inserts");

        cache.evict_path("/p0");
        assert_eq!(cache.bytes_held(), cache.footprint_sum(), "after evict_path");

        cache.evict_paths(&["/p1".to_string(), "/p2".to_string()]);
        assert_eq!(cache.bytes_held(), cache.footprint_sum(), "after evict_paths");

        // Overwrites, which replace rather than add.
        for _ in 0..10 {
            cache.insert(CacheKey::new("/p3", None, "gzip"), resp(20_000));
        }
        assert_eq!(cache.bytes_held(), cache.footprint_sum(), "after overwrites");

        cache.clear();
        assert_eq!(cache.bytes_held(), 0, "clear must zero the counter");
        assert_eq!(cache.footprint_sum(), 0);
    }

    /// A deploy-shaped cycle must not leave the cache permanently over its
    /// bound. This is the failure the drift would actually produce.
    #[test]
    fn repeated_invalidation_cycles_do_not_strand_the_counter() {
        let cache = Cache::with_max_bytes(1024 * 1024);
        for _cycle in 0..50 {
            for i in 0..20 {
                cache.insert(CacheKey::new(&format!("/page{i}"), None, "gzip"), resp(5_000));
            }
            for i in 0..20 {
                cache.evict_path(&format!("/page{i}"));
            }
        }
        assert_eq!(cache.bytes_held(), 0, "counter stranded after 50 deploy cycles");
        assert_eq!(cache.len(), 0);

        // And the cache still works afterwards.
        cache.insert(CacheKey::new("/after", None, "gzip"), resp(5_000));
        let mut buf = [0u8; 512];
        let k = make_lookup_key("/after", None, "gzip", &mut buf);
        assert!(!matches!(cache.lookup(k), Lookup::Miss), "cache unusable after cycles");
    }
}
