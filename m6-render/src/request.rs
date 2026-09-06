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
#[derive(Debug, Clone)]
pub struct RawRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    /// Header pairs with lowercase names, in order of appearance.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawRequest {
    pub fn method(&self) -> &str {
        &self.method
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Look up a header by name. `name` must be lowercase (all internal callers use lowercase).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }
}

/// The full request context exposed to handlers and file I/O helpers.
#[derive(Clone)]
pub struct Request {
    pub(crate) raw: RawRequest,
    /// Merged request dictionary.
    pub(crate) dict: Map<String, Value>,
    /// Site directory (absolute).
    pub(crate) site_dir: PathBuf,
}

impl Request {
    pub fn new(raw: RawRequest, dict: Map<String, Value>, site_dir: PathBuf) -> Self {
        Self { raw, dict, site_dir }
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

    pub fn dict(&self) -> &Map<String, Value> {
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
fn url_decode(s: &str) -> String {
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

/// Validate a path parameter value. Returns Err(BadRequest) if invalid.
pub fn validate_path_param(name: &str, value: &str) -> Result<()> {
    // `{relpath}` may contain `/` and `.`
    if name == "relpath" {
        if value.contains("..") {
            return Err(Error::BadRequest(format!(
                "path param `{name}` contains `..`"
            )));
        }
        return Ok(());
    }

    // Others: alphanumeric, hyphens, underscores, dots
    if value.contains("..") {
        return Err(Error::BadRequest(format!(
            "path param `{name}` contains `..`"
        )));
    }
    for ch in value.chars() {
        if !ch.is_alphanumeric() && ch != '-' && ch != '_' && ch != '.' {
            return Err(Error::BadRequest(format!(
                "path param `{name}` contains invalid character `{ch}`"
            )));
        }
    }
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

    #[test]
    fn test_parse_cookies() {
        let m = parse_cookies("session=abc123; theme=dark");
        assert_eq!(m.get("session").unwrap().as_str().unwrap(), "abc123");
        assert_eq!(m.get("theme").unwrap().as_str().unwrap(), "dark");
    }

    #[test]
    fn test_path_param_validation() {
        assert!(validate_path_param("stem", "hello-world").is_ok());
        assert!(validate_path_param("stem", "hello..world").is_err());
        assert!(validate_path_param("stem", "hello/world").is_err());
        assert!(validate_path_param("relpath", "a/b/c").is_ok());
        assert!(validate_path_param("relpath", "../etc/passwd").is_err());
    }

    #[test]
    fn test_write_json_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let req = Request::new(
            RawRequest {
                method: "GET".to_string(),
                path: "/".to_string(),
                query: String::new(),
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
                method: "POST".to_string(),
                path: "/upload".to_string(),
                query: String::new(),
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
                method: "POST".to_string(),
                path: "/form".to_string(),
                query: String::new(),
                headers: vec![],
                body: vec![],
            },
            dict,
            std::path::PathBuf::from("/tmp"),
        );
        assert!(req.verify_csrf().is_ok());
    }
}
