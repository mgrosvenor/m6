/// HTTP request type and request-dictionary building.
// HashMap removed — headers are stored as Vec for small-N linear-scan performance.
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// A parsed HTTP/1.1 request.
///
/// Headers are stored as `Vec<(name, value)>` with lowercase names.
/// Linear scan beats HashMap for the 4-8 headers typical in proxied requests.
/// The request type is `m6_core::http::RawRequest`, parsed by the one parser
/// in `m6_core::h1`.
///
/// This crate used to carry its own, with its own parser in `server.rs`.
/// Measured against h1spec it scored 15/32 against the shared parser's 27/32.
///
/// It also lowercased header names at parse time and documented that `header`
/// takes a lowercase name, which is an invariant established in one file and
/// relied on in another. The shared type keeps names as sent and compares
/// case-insensitively, so there is no invariant to remember and no way to get
/// it wrong.
pub use crate::http::RawRequest;

/// The full request context exposed to handlers and file I/O helpers.
#[derive(Clone)]
pub struct Request {
    pub(crate) raw: RawRequest,
    /// Merged request dictionary.
    ///
    /// A `Dict` rather than a `Map`: the static half is shared with every
    /// other request on this route rather than copied into each one. Cloning a
    /// `Request` therefore no longer copies the site's content.
    pub(crate) dict: crate::dict::Dict,
    /// Site directory (absolute).
    ///
    /// Shared rather than owned: it is the same path for the life of a reload,
    /// and building a `Request` used to allocate a fresh `PathBuf` for it on
    /// every request.
    pub(crate) site_dir: std::sync::Arc<PathBuf>,
    /// The matched route's pattern, when one matched.
    pub(crate) route_pattern: Option<String>,
    /// The matched route's own config keys that core does not define.
    ///
    /// Empty for a route registered in code, which has no config entry of its
    /// own, and for a request that reached a handler some other way.
    pub(crate) route_settings: Option<std::sync::Arc<Map<String, Value>>>,
}

impl Request {
    pub fn new(
        raw: RawRequest,
        dict: impl Into<crate::dict::Dict>,
        site_dir: impl Into<std::sync::Arc<PathBuf>>,
    ) -> Self {
        let dict = dict.into();
        let site_dir = site_dir.into();
        Self { raw, dict, site_dir, route_pattern: None, route_settings: None }
    }

    /// Attach the matched route, so a handler can read the config that sent
    /// the request to it.
    ///
    /// Separate from `new` rather than a fourth argument because a `Request`
    /// is legitimately built without a route in tests and in callers that
    /// never route, and because both fields are cheap clones: the pattern is
    /// a short string already cloned with the route, and the settings are an
    /// `Arc`.
    pub fn with_route(mut self, route: &crate::app::CompiledRoute) -> Self {
        self.route_pattern = Some(route.pattern.clone());
        self.route_settings = Some(std::sync::Arc::clone(&route.settings));
        self
    }

    // ---------- matched route ----------

    /// The pattern of the route that matched, e.g. `/assets/{*relpath}`.
    pub fn route_pattern(&self) -> Option<&str> {
        self.route_pattern.as_deref()
    }

    /// A key from the matched route's `[[route]]` table that core does not
    /// define.
    ///
    /// This is how a handler registered with `App::handler` reads its own
    /// per-route configuration. Core does not know what `root` means; the
    /// handler does, and the route says it:
    ///
    /// ```toml
    /// [[route]]
    /// path = "/assets/{*relpath}"
    /// handler = "files"
    /// root = "assets/"
    /// ```
    pub fn route_setting(&self, key: &str) -> Option<&Value> {
        self.route_settings.as_ref()?.get(key)
    }

    /// `route_setting` as a string, for the common case.
    pub fn route_str(&self, key: &str) -> Option<&str> {
        self.route_setting(key)?.as_str()
    }

    /// `route_setting` as a boolean, with a default for an absent key.
    ///
    /// A key that is present but is not a boolean returns the default and is
    /// not silently coerced: `tail = "yes"` is a config error, and reading it
    /// as `true` would hide it.
    pub fn route_bool(&self, key: &str, default: bool) -> bool {
        match self.route_setting(key) {
            Some(Value::Bool(b)) => *b,
            _ => default,
        }
    }

    // ---------- raw HTTP access ----------

    pub fn method(&self) -> &str {
        self.raw.method()
    }

    pub fn path(&self) -> &str {
        self.raw.path()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.raw.header(name)
    }

    pub fn content_type(&self) -> Option<&str> {
        self.raw.content_type()
    }

    pub fn body_raw(&self) -> &[u8] {
        &self.raw.body
    }

    pub fn body_json(&self) -> Result<Value> {
        serde_json::from_slice(&self.raw.body)
            .map_err(|e| Error::BadRequest(format!("invalid JSON body: {e}")))
    }

    pub fn field(&self, name: &str) -> Result<String> {
        // First try POST form fields in dict.
        if let Some(v) = self.dict.get(name) {
            if let Some(s) = v.as_str() {
                return Ok(s.to_string());
            }
        }
        // Then try query param.
        for (k, v) in parse_query_string(self.raw.query()) {
            if k == name {
                return Ok(v);
            }
        }
        Err(Error::BadRequest(format!("missing field `{name}`")))
    }

    // ---------- request dictionary ----------

    pub fn dict(&self) -> &crate::dict::Dict {
        &self.dict
    }

    // ---------- file I/O helpers ----------

    /// Resolve a site-relative path to absolute, validating against path traversal.
    pub fn site_path(&self, rel: &str) -> PathBuf {
        self.site_dir.join(rel)
    }

    fn validated_path(&self, rel: &str) -> Result<PathBuf> {
        let abs = self.site_dir.join(rel);
        // Ensure the canonicalised path stays within site_dir.
        // We check for `..` components.
        for comp in Path::new(rel).components() {
            if comp == Component::ParentDir {
                return Err(Error::BadRequest(
                    "path traversal not allowed".to_string(),
                ));
            }
        }
        Ok(abs)
    }

    /// Read a JSON file relative to site directory.
    pub fn read_json(&self, rel: &str) -> Result<Value> {
        let path = self.validated_path(rel)?;
        let data = std::fs::read(&path)
            .with_context(|| format!("reading {}", path.display()))
            .map_err(Error::Other)?;
        serde_json::from_slice(&data)
            .with_context(|| format!("parsing JSON from {}", path.display()))
            .map_err(Error::Other)
    }

    /// Write a JSON value to a file relative to site directory (overwrites).
    pub fn write_json(&self, rel: &str, data: &Value) -> Result<()> {
        let path = self.validated_path(rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating dirs for {}", path.display()))
                .map_err(Error::Other)?;
        }
        let bytes = serde_json::to_vec_pretty(data)
            .context("serialising JSON")
            .map_err(Error::Other)?;
        std::fs::write(&path, bytes)
            .with_context(|| format!("writing {}", path.display()))
            .map_err(Error::Other)
    }

    /// Write a JSON value atomically (write to temp file, rename).
    pub fn write_json_atomic(&self, rel: &str, data: &Value) -> Result<()> {
        let path = self.validated_path(rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating dirs for {}", path.display()))
                .map_err(Error::Other)?;
        }
        let bytes = serde_json::to_vec_pretty(data)
            .context("serialising JSON")
            .map_err(Error::Other)?;

        // Write to a temp file in the same directory, then rename.
        let tmp_path = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp_path)
                .with_context(|| format!("creating temp file {}", tmp_path.display()))
                .map_err(Error::Other)?;
            f.write_all(&bytes)
                .with_context(|| format!("writing temp file {}", tmp_path.display()))
                .map_err(Error::Other)?;
            f.sync_all()
                .context("syncing temp file")
                .map_err(Error::Other)?;
        }
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("renaming {} → {}", tmp_path.display(), path.display()))
            .map_err(Error::Other)
    }

    /// List all *.json files in a directory, returning their parsed contents.
    pub fn list_json(&self, rel: &str) -> Result<Vec<Value>> {
        let path = self.validated_path(rel)?;
        let mut out = Vec::new();
        let entries = std::fs::read_dir(&path)
            .with_context(|| format!("reading dir {}", path.display()))
            .map_err(Error::Other)?;

        for entry in entries {
            let entry = entry.context("reading dir entry").map_err(Error::Other)?;
            let ep = entry.path();
            if ep.extension().and_then(|e| e.to_str()) == Some("json") {
                let data = std::fs::read(&ep)
                    .with_context(|| format!("reading {}", ep.display()))
                    .map_err(Error::Other)?;
                let v: Value = serde_json::from_slice(&data)
                    .with_context(|| format!("parsing {}", ep.display()))
                    .map_err(Error::Other)?;
                out.push(v);
            }
        }
        Ok(out)
    }

    /// Touch a file (update mtime), creating it if it doesn't exist.
    pub fn touch(&self, rel: &str) -> Result<()> {
        let path = self.validated_path(rel)?;
        if path.exists() {
            let now = filetime::FileTime::now();
            filetime::set_file_times(&path, now, now)
                .with_context(|| format!("touching {}", path.display()))
                .map_err(Error::Other)?;
        } else {
            std::fs::File::create(&path)
                .with_context(|| format!("creating {}", path.display()))
                .map_err(Error::Other)?;
        }
        Ok(())
    }

    /// Index operator for the request dictionary.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.dict.get(key)
    }

    /// Verify the CSRF double-submit cookie token (feature = "csrf").
    ///
    /// Compares the `csrf_token` form/query field with the `_csrf` cookie value.
    /// Returns `Err(Error::Forbidden)` if missing or mismatched.
    #[cfg(feature = "csrf")]
    pub fn verify_csrf(&self) -> crate::error::Result<()> {
        // Get cookie value.
        let cookie_token = self
            .dict
            .get("cookies")
            .and_then(|c| c.get("_csrf"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Get form/query field value.
        let field_token = self
            .dict
            .get("csrf_token")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if cookie_token.is_empty() || field_token.is_empty() || cookie_token != field_token {
            return Err(crate::error::Error::Forbidden);
        }
        Ok(())
    }

    /// Parse a named file field from a multipart/form-data body (feature = "multipart").
    #[cfg(feature = "multipart")]
    pub fn file(&self, name: &str) -> crate::error::Result<crate::multipart::Upload> {
        let ct = self
            .raw
            .content_type()
            .unwrap_or("");
        crate::multipart::parse_upload(&self.raw.body, ct, name)
    }

    /// Write raw bytes to a path relative to the site directory (feature = "multipart").
    ///
    /// Atomic: writes to a temp file then renames. Path is validated against traversal.
    #[cfg(feature = "multipart")]
    pub fn write_bytes(&self, rel: &str, data: &[u8]) -> crate::error::Result<()> {
        use anyhow::Context;
        let path = self.validated_path(rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating dirs for {}", path.display()))
                .map_err(crate::error::Error::Other)?;
        }
        let tmp_path = path.with_extension("tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp_path)
                .with_context(|| format!("creating temp file {}", tmp_path.display()))
                .map_err(crate::error::Error::Other)?;
            f.write_all(data)
                .with_context(|| format!("writing temp file {}", tmp_path.display()))
                .map_err(crate::error::Error::Other)?;
            f.sync_all()
                .context("syncing temp file")
                .map_err(crate::error::Error::Other)?;
        }
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("renaming {} → {}", tmp_path.display(), path.display()))
            .map_err(crate::error::Error::Other)
    }
}

impl std::ops::Index<&str> for Request {
    type Output = Value;
    fn index(&self, key: &str) -> &Value {
        self.dict.get(key).unwrap_or(&Value::Null)
    }
}

// ---------- Query/form parsing ----------

pub fn parse_query_string(query: &str) -> Vec<(String, String)> {
    if query.is_empty() {
        return vec![];
    }
    query
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let k = parts.next()?;
            let v = parts.next().unwrap_or("");
            Some((url_decode(k), url_decode(v)))
        })
        .collect()
}

pub fn parse_form_body(body: &[u8]) -> Vec<(String, String)> {
    // `from_utf8(..).unwrap_or("")` threw away the ENTIRE body on a single
    // invalid byte, yielding a submission with no fields and no explanation --
    // indistinguishable downstream from a genuinely empty form. A urlencoded
    // body is ASCII by construction, so this only fires on malformed or
    // hostile input, and losing one character is the right answer there rather
    // than losing everything. `url_decode` does the real UTF-8 assembly after
    // percent-decoding; this only guards the outer container.
    let s = String::from_utf8_lossy(body);
    parse_query_string(&s)
}

/// Minimal URL percent-decoding (+ → space, %XX → byte).
///
/// **Decodes into a byte buffer and interprets the whole thing as UTF-8 at the
/// end.** That ordering is the entire point of this function.
///
/// It used to build a `String` directly with `out.push(byte as char)`. In Rust
/// that cast means "the character whose code point is `byte`" -- Latin-1 -- so
/// every multi-byte UTF-8 sequence was split into one wrong character per byte.
/// Percent-encoding is defined over bytes; a byte only becomes a character once
/// the full sequence is reassembled, which cannot be done one byte at a time.
///
/// This corrupted every non-ASCII character any visitor typed, in every form
/// and every query string, before a handler ever saw it. It reached production
/// via the contact form: a curly apostrophe (U+2019, sent as `%E2%80%99`)
/// arrived as U+00E2 U+0080 U+0099, so "I'm not sure" was emailed as
/// "Ia<80><99>m not sure". Emoji, being four bytes, came out as four wrong
/// characters.
///
/// It went unnoticed for so long because ASCII is a fixed point: for any byte
/// below 0x80 the code point and the byte are the same number, so every ASCII
/// test passes against the broken version. Only non-ASCII input distinguishes
/// them, and the one test here used `%2F`.
///
/// Invalid UTF-8 becomes U+FFFD rather than failing the decode. For a public
/// form one mangled character is a far better outcome than discarding the
/// submission, and a hostile client can always send invalid bytes.
pub fn url_decode(s: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                hex_digit(bytes[i + 1]),
                hex_digit(bytes[i + 2]),
            ) {
                out.push((h << 4) | l);
                i += 3;
            } else {
                out.push(b'%');
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a single URL component, RFC 3986 unreserved only.
///
/// `/` IS encoded, as `%2F`. Use this for anything going into one field: a
/// query value, a cookie value, a path segment.
pub fn url_encode(s: &str) -> String {
    encode(s, false)
}

/// Percent-encode a path, leaving `/` alone.
///
/// The separator has to survive or the result is one escaped blob rather than
/// a path. This is what a `?next=/some/page` redirect target needs, and using
/// [`url_encode`] there instead turns the destination into `%2Fsome%2Fpage`.
///
/// Two named functions rather than one with a flag, because the caller that
/// picks wrong here produces a broken link rather than an error, and a
/// boolean argument at the call site does not say which way round it goes.
pub fn url_encode_path(s: &str) -> String {
    encode(s, true)
}

fn encode(s: &str, keep_slash: bool) -> String {
    // Encoding is defined over bytes, the same as decoding: a multi-byte
    // character becomes one `%XX` per byte, never one per code point.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => {
                out.push('%');
                out.push(char::from(HEX[(b >> 4) as usize]));
                out.push(char::from(HEX[(b & 0xf) as usize]));
            }
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// One named cookie out of a `Cookie` header value.
///
/// Returns the FIRST match. A duplicate cookie name is a client's problem and
/// the first is what a browser sends for the most specific path, so taking it
/// is the conservative read. [`parse_cookies`] builds a whole map and keeps
/// the last, which is the right answer when you want them all and the wrong
/// one when you want a session token.
pub fn cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    for part in header.split(';') {
        let part = part.trim();
        if let Some(pos) = part.find('=') {
            if part[..pos].trim() == name {
                return Some(part[pos + 1..].trim());
            }
        }
    }
    None
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse cookies from a `Cookie` header value.
pub fn parse_cookies(header: &str) -> Map<String, Value> {
    let mut map = Map::new();
    for part in header.split(';') {
        let part = part.trim();
        let mut kv = part.splitn(2, '=');
        if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
            map.insert(k.trim().to_string(), Value::String(v.trim().to_string()));
        }
    }
    map
}

/// Parse auth claims from the `X-Auth-Claims` header (base64-encoded JSON).
pub fn parse_auth_claims(header: &str) -> Map<String, Value> {
    use base64::Engine;
    let mut map = Map::new();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(header.trim())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(header.trim()));

    if let Ok(bytes) = decoded {
        if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(obj) = v.as_object() {
                if let Some(u) = obj.get("username").or_else(|| obj.get("sub")) {
                    map.insert("auth_username".to_string(), u.clone());
                }
                if let Some(sub) = obj.get("sub") {
                    map.insert("auth_sub".to_string(), sub.clone());
                }
                if let Some(g) = obj.get("groups") {
                    map.insert("auth_groups".to_string(), g.clone());
                }
                if let Some(r) = obj.get("roles") {
                    map.insert("auth_roles".to_string(), r.clone());
                }
            }
        }
    }
    map
}

/// Validate an ordinary path parameter, which is exactly one path segment.
///
/// Delegates to `m6_core::validate_path_param`, which is the one
/// implementation.
///
/// The local copy this replaces had a name-based special case: a parameter
/// called `relpath` was checked for `..` and then returned `Ok` with **no
/// character validation at all**, accepting spaces, control characters and
/// NUL. The name is still not what decides this; the route is. See
/// `validate_wildcard_param` for the segment-spanning half, and use this one
/// for a `Segment::Param`, which cannot contain a slash.
pub fn validate_path_param(name: &str, value: &str) -> Result<()> {
    crate::validate_path_param(value, false).map_err(|e| {
        Error::BadRequest(format!("path param `{name}` is invalid: {e}"))
    })?;
    Ok(())
}

/// Validate a `{*name}` wildcard capture, which spans path segments.
///
/// The same validation with `allow_slash = true`, and it exists as its own
/// function because the choice between the two belongs to the route rather
/// than to the parameter's name. `validate_path_param` used to carry a
/// comment explaining that a slash was impossible here, because "this crate's
/// router has no catch-all support ... `Segment` is only `Literal` or `Param`
/// and `match_route` requires an exact segment count". That was true when it
/// was written. `Segment::Wildcard` made it false on 2026-09-12 and nothing
/// said so: every wildcard test stopped at the matcher, so the capability
/// looked complete while `/assets/css/main.css` was answered **400** by step 4
/// of `build_dict` for containing the slash the wildcard exists to capture.
///
/// Traversal is still refused. `..` is rejected as a substring, a leading or
/// trailing slash is rejected, and the character set is unchanged, so what
/// this permits over the ordinary form is the separator and nothing else.
pub fn validate_wildcard_param(name: &str, value: &str) -> Result<()> {
    crate::validate_path_param(value, true).map_err(|e| {
        Error::BadRequest(format!("wildcard param `{name}` is invalid: {e}"))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_query() {
        let pairs = parse_query_string("a=1&b=hello+world&c=%2F");
        assert_eq!(pairs[0], ("a".to_string(), "1".to_string()));
        assert_eq!(pairs[1], ("b".to_string(), "hello world".to_string()));
        assert_eq!(pairs[2], ("c".to_string(), "/".to_string()));
    }

    /// Percent-decoding must reassemble UTF-8 from BYTES, not map each byte to
    /// a code point.
    ///
    /// `url_decode` used to do `out.push(byte as char)`, which in Rust means
    /// "the character at code point `byte`" -- i.e. Latin-1. Every multi-byte
    /// UTF-8 sequence was therefore split into one bogus character per byte,
    /// and every non-ASCII character a visitor typed was corrupted on the way
    /// in, before any handler saw it.
    ///
    /// It reached production through the contact form: a curly apostrophe
    /// (U+2019, sent as `%E2%80%99`) became U+00E2 U+0080 U+0099, so "I'm not
    /// sure" was emailed as "Ia<80><99>m not sure".
    ///
    /// The bug survived because the only test here used `%2F`. ASCII bytes and
    /// their code points are numerically identical, so every ASCII case passes
    /// against the broken implementation. Non-ASCII is the only input that can
    /// tell the two apart.
    #[test]
    fn url_decode_reassembles_multibyte_utf8() {
        let cases: &[(&str, &str)] = &[
            // The exact production failure.
            ("I%E2%80%99m", "I\u{2019}m"),
            // 2-byte.
            ("caf%C3%A9", "caf\u{e9}"),
            // 3-byte.
            ("%E2%82%AC20", "\u{20ac}20"),
            // 4-byte: emoji. Astral plane, the case most likely to be typed on
            // a phone and least likely to be tested.
            ("%F0%9F%98%80", "\u{1F600}"),
            // Emoji mixed with text and a `+` space.
            ("hi+%F0%9F%91%8B+there", "hi \u{1F44B} there"),
            // Non-Latin scripts.
            ("%D0%9F%D1%80%D0%B8%D0%B2%D0%B5%D1%82", "\u{041f}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442}"),
            ("%E6%97%A5%E6%9C%AC%E8%AA%9E", "\u{65e5}\u{672c}\u{8a9e}"),
            // Combining mark: must not be reordered or split.
            ("e%CC%81", "e\u{0301}"),
            // ASCII still works (this is what the old test covered).
            ("a=1", "a=1"),
            ("%2F", "/"),
        ];
        for (input, want) in cases {
            assert_eq!(
                url_decode(input),
                *want,
                "url_decode({input:?}) corrupted the text -- bytes were mapped \
                 to code points instead of being decoded as UTF-8"
            );
        }
    }

    /// The same path a real submission takes.
    #[test]
    fn form_body_preserves_emoji_and_punctuation() {
        let body = b"name=Ren%C3%A9&message=I%E2%80%99m+here+%F0%9F%8E%89";
        let pairs = parse_form_body(body);
        assert_eq!(pairs[0], ("name".to_string(), "Ren\u{e9}".to_string()));
        assert_eq!(
            pairs[1],
            ("message".to_string(), "I\u{2019}m here \u{1F389}".to_string())
        );
    }

    /// Invalid percent-encoded bytes must not take the whole form with them.
    /// `String::from_utf8_lossy` substitutes U+FFFD for the bad sequence and
    /// keeps everything else, which is the right trade for a public form: one
    /// mangled character beats a silently empty submission.
    #[test]
    fn invalid_utf8_degrades_to_replacement_not_an_empty_form() {
        let pairs = parse_form_body(b"a=%FF%FE&b=ok");
        assert_eq!(pairs.len(), 2, "a bad byte must not drop the other fields");
        assert_eq!(pairs[1], ("b".to_string(), "ok".to_string()));
        assert!(
            pairs[0].1.contains('\u{FFFD}'),
            "expected replacement characters, got {:?}",
            pairs[0].1
        );
    }

    /// A `%` immediately before a multi-byte character must not panic.
    ///
    /// Decoding by slicing the `&str` (`&s[i+1..i+3]`) panics when those
    /// offsets land inside a character, and a hostile client arranges that by
    /// putting a `%` before any non-ASCII byte. In a login handler that is a
    /// remote crash. Indexing the byte array, as this does, cannot.
    ///
    /// Moved here from m6-auth-server when its second copy of `url_decode` was
    /// deleted. The property belongs with the one implementation.
    #[test]
    fn percent_before_multibyte_does_not_panic() {
        for input in ["%\u{e9}", "%\u{1F600}x", "abc%\u{4e2d}\u{6587}", "%", "%A", "%ZZ",
                      "%%", "a%", "%F0%9F%98"] {
            let _ = url_decode(input);
        }
    }

    /// Also from m6-auth-server. A password is the worst place for the
    /// Latin-1 defect: the user sees only "invalid credentials" and nothing
    /// says the password was corrupted rather than wrong.
    #[test]
    fn a_password_shaped_field_survives_decoding() {
        assert_eq!(url_decode("p%C3%A4ssw%C3%B6rd+123"), "p\u{e4}ssw\u{f6}rd 123");
        assert_eq!(url_decode("plain%2Fascii"), "plain/ascii");
    }

    #[test]
    fn url_encode_is_over_bytes_not_code_points() {
        // One %XX per byte. A three-byte character becomes three escapes, not
        // one, which is the same rule decoding obeys in reverse.
        assert_eq!(url_encode("I\u{2019}m"), "I%E2%80%99m");
        assert_eq!(url_encode("caf\u{e9}"), "caf%C3%A9");
        assert_eq!(url_encode("\u{1F600}"), "%F0%9F%98%80");
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(url_encode("ok-1.txt~"), "ok-1.txt~");
    }

    /// The distinction the two functions exist for.
    #[test]
    fn only_the_path_form_keeps_the_separator() {
        assert_eq!(url_encode("/blog/post"), "%2Fblog%2Fpost");
        assert_eq!(url_encode_path("/blog/post"), "/blog/post");
        // And the path form still escapes everything else.
        assert_eq!(url_encode_path("/blog/a b"), "/blog/a%20b");
        assert_eq!(url_encode_path("/caf\u{e9}/x"), "/caf%C3%A9/x");
    }

    /// Encode then decode must be the identity, including for the input that
    /// broke the contact form.
    #[test]
    fn encode_decode_round_trips() {
        for case in ["I\u{2019}m not sure", "caf\u{e9}", "\u{1F600}\u{1F44B}",
                     "a b&c=d", "/a/b?x=1", "\u{65e5}\u{672c}\u{8a9e}"] {
            assert_eq!(url_decode(&url_encode(case)), case, "round trip: {case:?}");
        }
    }

    #[test]
    fn cookie_reads_one_by_name() {
        let h = "session=abc123; theme=dark; empty=";
        assert_eq!(cookie(h, "session"), Some("abc123"));
        assert_eq!(cookie(h, "theme"), Some("dark"));
        assert_eq!(cookie(h, "empty"), Some(""));
        assert_eq!(cookie(h, "absent"), None);
        // A name that is a prefix of another must not match it.
        assert_eq!(cookie("sessionid=x", "session"), None);
    }

    /// `cookie` takes the first, `parse_cookies` keeps the last. Both are
    /// defensible and they differ, so the difference is pinned rather than
    /// left for someone to discover through a session bug.
    #[test]
    fn duplicate_cookie_names_resolve_differently_on_purpose() {
        let h = "sid=first; sid=second";
        assert_eq!(cookie(h, "sid"), Some("first"));
        assert_eq!(parse_cookies(h).get("sid").unwrap().as_str(), Some("second"));
    }

    #[test]
    fn test_parse_cookies() {
        let m = parse_cookies("session=abc123; theme=dark");
        assert_eq!(m.get("session").unwrap().as_str().unwrap(), "abc123");
        assert_eq!(m.get("theme").unwrap().as_str().unwrap(), "dark");
    }

    #[test]
    fn test_path_param_validation() {
        assert!(validate_path_param("stem", "hello-world").is_ok());
        assert!(validate_path_param("stem", "style.css").is_ok());
        assert!(validate_path_param("stem", "hello..world").is_err());
        assert!(validate_path_param("stem", "hello/world").is_err());
        assert!(validate_path_param("relpath", "../etc/passwd").is_err());
    }

    /// `relpath` is not special here, and the assertion that it was has been
    /// removed rather than relaxed.
    ///
    /// The old test asserted `validate_path_param("relpath", "a/b/c").is_ok()`,
    /// pinning a name-based branch that skipped character validation entirely.
    /// That branch accepted spaces, control characters and NUL.
    ///
    /// It was also unreachable in the sense it was written for. This crate's
    /// router cannot produce a parameter containing a slash:
    ///
    ///   - `Segment` has only `Literal` and `Param`. There is no catch-all
    ///     variant and `compile_pattern` never creates one.
    ///   - `match_route` requires `path_segs.len() == route.segments.len()`.
    ///   - `path_segs` comes from `path.split('/')`, so each element is one
    ///     segment with no slash by construction.
    ///   - `server.rs` does no percent-decoding of the path, so `%2F` stays
    ///     literal and never becomes a separator before the split.
    ///
    /// `{relpath}` appears only in `m6-file` route configs, which does have a
    /// catch-all and validates it with `allow_slash = true`.
    ///
    /// If this crate ever gains catch-all routing, this test should come back
    /// as `validate_path_param(value, true)` at the call site rather than as a
    /// name-based exemption.
    #[test]
    fn every_param_is_character_checked_regardless_of_name() {
        for name in ["stem", "relpath", "anything"] {
            assert!(validate_path_param(name, "a b").is_err(), "{name}: space");
            assert!(validate_path_param(name, "a\u{0}b").is_err(), "{name}: NUL");
            assert!(validate_path_param(name, "a/b/c").is_err(), "{name}: slash");
            assert!(validate_path_param(name, "ok-1.txt").is_ok(), "{name}: valid");
        }
    }

    #[test]
    fn test_write_json_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let req = Request::new(
            RawRequest {
                version: "HTTP/1.1".to_string(),
                method: "GET".to_string(),
                path: "/".to_string(),
                query: None,
                headers: vec![],
                body: vec![],
            },
            Map::new(),
            dir.path().to_path_buf(),
        );
        req.write_json_atomic("test.json", &json!({"key": "value"})).unwrap();
        let v = req.read_json("test.json").unwrap();
        assert_eq!(v["key"], "value");
    }

    #[cfg(feature = "multipart")]
    #[test]
    fn test_write_bytes_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let req = Request::new(
            RawRequest {
                version: "HTTP/1.1".to_string(),
                method: "POST".to_string(),
                path: "/upload".to_string(),
                query: None,
                headers: vec![],
                body: vec![],
            },
            Map::new(),
            dir.path().to_path_buf(),
        );
        let data = b"binary-content";
        req.write_bytes("output.bin", data).unwrap();
        let read_back = std::fs::read(dir.path().join("output.bin")).unwrap();
        assert_eq!(read_back, data);
    }

    #[cfg(feature = "csrf")]
    #[test]
    fn test_verify_csrf_ok() {
        let token = "csrf-token-abc".to_string();
        let mut dict = Map::new();
        let mut cookies = Map::new();
        cookies.insert("_csrf".to_string(), json!(token));
        dict.insert("cookies".to_string(), json!(cookies));
        dict.insert("csrf_token".to_string(), json!(token));
        let req = Request::new(
            RawRequest {
                version: "HTTP/1.1".to_string(),
                method: "POST".to_string(),
                path: "/form".to_string(),
                query: None,
                headers: vec![],
                body: vec![],
            },
            dict,
            std::path::PathBuf::from("/tmp"),
        );
        assert!(req.verify_csrf().is_ok());
    }

    /// The mismatch case. Moved here from `m6_render::app` in Phase 5, where it
    /// had not compiled since Phase 4 and nothing noticed, because the default
    /// feature set leaves `csrf` off and the test runner never enabled it.
    #[cfg(feature = "csrf")]
    #[test]
    fn verify_csrf_rejects_a_mismatched_token() {
        let mut dict = Map::new();
        let mut cookies = Map::new();
        cookies.insert("_csrf".to_string(), json!("token-a"));
        dict.insert("cookies".to_string(), json!(cookies));
        dict.insert("csrf_token".to_string(), json!("token-b"));
        let req = Request::new(
            RawRequest {
                version: "HTTP/1.1".to_string(),
                method: "POST".to_string(),
                path: "/submit".to_string(),
                query: None,
                headers: vec![],
                body: vec![],
            },
            dict,
            std::path::PathBuf::from("/tmp"),
        );
        assert!(matches!(req.verify_csrf(), Err(Error::Forbidden)));
    }
}
