/// HTTP response type and constructors.
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::request::Request;

/// What a response's body is.
///
/// A sum type rather than a `Vec<u8>` plus an optional reader, because the
/// difference is load-bearing rather than incidental: a body core has not read
/// cannot be minified, compressed, or hashed into an ETag, and making that a
/// flag alongside the bytes would leave three functions free to take the bytes
/// that are not there. Here they cannot: `as_bytes` returns `None` for a
/// stream and the transforms have nothing to work on.
///
/// This is what `App` was missing. `m6-file` streams a file straight to the
/// wire (`send_stream`), and `lute.min.js` alone is 3.6MB that used to be read
/// into a `Vec` on every cache miss to be copied straight out again. A
/// migration onto a `Response` that could only hold bytes would have put that
/// allocation back, on the service that serves every asset on the site.
pub enum Body {
    /// Bytes already in memory.
    Bytes(Vec<u8>),
    /// Exactly `len` bytes to be read from `reader` and written to the wire.
    ///
    /// The length is promised in `Content-Length` before the first byte is
    /// read, so `Responder::send_stream` owns what happens when the file
    /// disagrees: a short read fails and an overrun is capped, rather than the
    /// framing going wrong.
    Stream {
        len: u64,
        reader: Box<dyn std::io::Read + Send>,
    },
}

impl Body {
    pub fn empty() -> Self {
        Body::Bytes(Vec::new())
    }

    /// True only for an empty byte body. A stream of zero bytes is still a
    /// stream, and the pipeline steps that skip an empty body must not treat
    /// the two as the same thing.
    pub fn is_empty(&self) -> bool {
        matches!(self, Body::Bytes(b) if b.is_empty())
    }

    /// The bytes, when core has them. `None` for a stream.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Body::Bytes(b) => Some(b),
            Body::Stream { .. } => None,
        }
    }

    /// How many bytes this body will put on the wire.
    pub fn len(&self) -> u64 {
        match self {
            Body::Bytes(b) => b.len() as u64,
            Body::Stream { len, .. } => *len,
        }
    }
}

impl Default for Body {
    fn default() -> Self {
        Body::empty()
    }
}

impl From<Vec<u8>> for Body {
    fn from(v: Vec<u8>) -> Self {
        Body::Bytes(v)
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Body::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            Body::Stream { len, .. } => write!(f, "Stream({len} bytes)"),
        }
    }
}

/// An HTTP response ready for serialisation.
///
/// Not `Clone`: a `Body::Stream` owns a reader, and duplicating a response
/// that is half-read is not a thing that can mean anything. Nothing cloned one.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
    /// The handler has produced the exact representation to send.
    ///
    /// Core's pipeline minifies and compresses what a handler returns, which is
    /// right for a handler that returns a document and wrong for one that has
    /// already negotiated. `m6-file` picks the coding, compresses, and builds an
    /// ETag naming *that* representation; re-compressing it downstream would
    /// send brotli bytes under an ETag asserting identity, which is the exact
    /// defect its `-br`/`-gz` tag suffixes exist to prevent.
    ///
    /// Explicit rather than inferred from the presence of a `Content-Encoding`
    /// or an `ETag`, because a handler that sets one of those and still wants
    /// the pipeline is a reasonable thing to be, and guessing would make which
    /// one it is depend on header order. A streamed body is verbatim by
    /// construction; this flag is for the buffered case.
    pub verbatim: bool,
    /// Template name, if this response came from template rendering.
    ///
    /// Public because core does not render. It records what was asked for and
    /// hands it to whichever crate owns templating (`m6-html`), which clears
    /// both fields once it has rendered. A `pub(crate)` field would only work
    /// while the renderer lived in the same crate as the type, which is the
    /// arrangement this migration is undoing.
    pub template_name: Option<String>,
    /// Extra template context supplied by the handler via render_with.
    pub template_dict: Option<Map<String, Value>>,
}

impl Response {
    // ---------- constructors ----------

    /// Render a Tera template with the request dictionary.
    pub fn render(template: &str, req: &Request) -> Result<Self> {
        Self::render_status(template, req, 200)
    }

    /// Render a template with extra context merged on top of the request dict.
    pub fn render_with(template: &str, req: &Request, extra: Value) -> Result<Self> {
        let mut dict = req.dict.clone();
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                dict.insert(k.clone(), v.clone());
            }
        }
        Self::render_dict(template, &dict, 200)
    }

    /// Render a template with a custom status code.
    pub fn render_status(template: &str, req: &Request, status: u16) -> Result<Self> {
        Self::render_dict(template, &req.dict, status)
    }

    /// Record a template render from a dict directly, without a `Request`.
    pub fn render_dict(
        template: &str,
        dict: &Map<String, Value>,
        status: u16,
    ) -> Result<Self> {
        Ok(Self {
            status,
            headers: vec![],
            body: Body::empty(),
            verbatim: false,
            template_name: Some(template.to_string()),
            template_dict: Some(dict.clone()),
        })
    }

    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".to_string(), location.to_string())],
            body: Body::empty(),
            verbatim: false,
            template_name: None,
            template_dict: None,
        }
    }

    pub fn redirect_permanent(location: &str) -> Self {
        Self {
            status: 301,
            headers: vec![("Location".to_string(), location.to_string())],
            body: Body::empty(),
            verbatim: false,
            template_name: None,
            template_dict: None,
        }
    }

    pub fn json(value: Value) -> Self {
        Self::json_status(value, 200)
    }

    pub fn json_status(value: Value, status: u16) -> Self {
        let body = serde_json::to_vec(&value).unwrap_or_default();
        Self {
            status,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: Body::Bytes(body),
            verbatim: false,
            template_name: None,
            template_dict: None,
        }
    }

    /// An HTML body, already rendered.
    ///
    /// For a page a service built itself rather than one that came from a
    /// template. `render` records a template name for the engine to fill in
    /// later; this is the finished bytes.
    pub fn html(s: impl Into<String>) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/html; charset=utf-8".to_string())],
            body: Body::Bytes(s.into().into_bytes()),
            verbatim: false,
            template_name: None,
            template_dict: None,
        }
    }

    /// Replace the body, keeping status and headers.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Body::Bytes(body.into());
        self
    }

    /// A body read from `reader` rather than held in memory.
    ///
    /// `len` is promised as `Content-Length` before the first byte is read, so
    /// it has to be the length the reader will actually produce; it usually
    /// comes from the same `metadata` call that dated the response, which is
    /// also what makes the two agree.
    ///
    /// A streamed response carries no automatic ETag, because supplying one
    /// would mean reading the body to hash it. Set a validator that does not
    /// need the bytes -- mtime and size are the usual pair -- or the response
    /// goes out without one.
    ///
    /// Nothing downstream transforms it: `Body::as_bytes` is `None`, so
    /// minification and compression have nothing to act on.
    pub fn stream(status: u16, len: u64, reader: impl std::io::Read + Send + 'static) -> Self {
        Self {
            status,
            headers: vec![],
            body: Body::Stream { len, reader: Box::new(reader) },
            verbatim: true,
            template_name: None,
            template_dict: None,
        }
    }

    /// Mark this response as the exact representation to send.
    ///
    /// Core will not minify or compress it. For a handler that has negotiated
    /// the content coding itself and built a validator naming the result: doing
    /// either of those again would change the bytes out from under that ETag.
    pub fn verbatim(mut self) -> Self {
        self.verbatim = true;
        self
    }

    /// Set the status, keeping everything else.
    pub fn with_status(mut self, code: u16) -> Self {
        self.status = code;
        self
    }

    pub fn text(s: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/plain; charset=utf-8".to_string())],
            body: Body::Bytes(s.as_bytes().to_vec()),
            verbatim: false,
            template_name: None,
            template_dict: None,
        }
    }

    pub fn status(code: u16) -> Self {
        Self {
            status: code,
            headers: vec![],
            body: Body::empty(),
            verbatim: false,
            template_name: None,
            template_dict: None,
        }
    }

    pub fn not_found() -> Self {
        Self::status(404)
    }

    pub fn forbidden() -> Self {
        Self::status(403)
    }

    pub fn bad_request() -> Self {
        Self::status(400)
    }

    // ---------- chained modifiers ----------

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Set a cookie with `Max-Age` in seconds (0 = delete).
    ///
    /// `Path=/` and `HttpOnly`, which is the right default for a cookie a
    /// handler sets without saying more. A cookie needing `Secure`,
    /// `SameSite`, or to be readable from the page should be built with
    /// [`crate::cookie::Cookie`] and pushed with `header`.
    pub fn cookie(mut self, name: &str, value: &str, max_age: i64) -> Self {
        let c = if max_age == 0 {
            crate::cookie::Cookie::removal(name)
        } else {
            crate::cookie::Cookie::new(name, value).max_age(max_age)
        };
        self.headers.push(c.path("/").http_only().to_header());
        self
    }

    /// Attach a flash message to this response (feature = "flash").
    ///
    /// The message is stored in a short-lived signed HMAC-SHA256 cookie.
    /// Call this on the response that performs the redirect.
    ///
    /// The signing key is `flash_secret` from the app config.
    #[cfg(feature = "flash")]
    pub fn flash(self, message: &str, secret: &[u8]) -> Self {
        use base64::Engine;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let msg_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(message.as_bytes());

        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
            .expect("HMAC accepts any key length");
        mac.update(msg_b64.as_bytes());
        let sig_bytes = mac.finalize().into_bytes();
        let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&sig_bytes);

        let cookie_val = format!("{}.{}", msg_b64, sig_b64);
        let mut s = self;
        s.headers.push(
            crate::cookie::Cookie::new("_flash", cookie_val)
                .max_age(120)
                .path("/")
                .http_only()
                .to_header(),
        );
        s
    }

    // ---------- HTTP serialisation ----------

    /// Send this response through the one HTTP/1.1 response writer.
    ///
    /// This used to be `write_to`, a third serialiser alongside m6-file's and
    /// m6-http's. It emitted no `Connection` field at all and no `Date`, and
    /// it wrote the body on a HEAD -- three different answers to the same
    /// questions in one workspace. What stays here is the part that is
    /// genuinely this type's own: the two defaults it supplies when a handler
    /// did not.
    ///
    /// Takes `self` by value rather than by reference because a `Body::Stream`
    /// owns its reader and `send_stream` has to take it. A response goes on the
    /// wire once, and the signature now says so.
    pub fn send<W: std::io::Write>(
        self,
        resp: &mut crate::h1::Responder<'_, W>,
    ) -> anyhow::Result<()> {
        let Response { status, headers, body, .. } = self;

        let has = |name: &str| crate::headers::contains(&headers[..], name);

        let etag;
        let mut hdrs: Vec<(&str, &str)> =
            headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        if !has("content-type") && !body.is_empty() {
            hdrs.push(("Content-Type", "text/html; charset=utf-8"));
        }

        match body {
            // Written straight through, and deliberately given no default
            // ETag: the default below is a hash of the bytes, and hashing a
            // stream means reading it, which is the entire thing being
            // avoided. A handler that streams knows its own validator --
            // m6-file builds one from mtime and size -- and sets it.
            Body::Stream { len, reader } => {
                resp.send_stream(status, &hdrs, len, reader)?;
            }
            Body::Bytes(bytes) => {
                // A rendered page has no filesystem mtime to hang a
                // Last-Modified off of, but its body is a plain byte string —
                // a content hash gives conditional-GET (see m6-http's
                // cache-hit `is_not_modified` check) a real signal to compare
                // against once this response is cached.
                if !has("etag") && !bytes.is_empty() {
                    etag = format!("\"{:x}\"", content_hash(&bytes));
                    hdrs.push(("ETag", etag.as_str()));
                }
                resp.send(status, &hdrs, &bytes)?;
            }
        }
        Ok(())
    }
}

fn content_hash(body: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}


/// Map an `Error` to a `Response` (status only, body set by framework).
pub fn error_to_response(err: &Error) -> Response {
    match err {
        Error::NotFound => Response::not_found(),
        Error::Forbidden => Response::forbidden(),
        Error::BadRequest(_) => Response::bad_request(),
        Error::Other(_) => {
            tracing::error!("handler error: {err}");
            Response::status(500)
        }
    }
}

#[cfg(test)]
mod body_tests {
    use super::*;
    use crate::h1::Responder;

    fn send_to_wire(r: Response, method: &str) -> (String, Vec<u8>) {
        let mut out = Vec::new();
        {
            let mut resp = Responder::new(&mut out, method, false);
            r.send(&mut resp).expect("send");
        }
        let sep = out.windows(4).position(|w| w == b"\r\n\r\n").expect("header terminator");
        let head = String::from_utf8(out[..sep].to_vec()).expect("headers are ASCII");
        (head, out[sep + 4..].to_vec())
    }

    /// The capability `App` was missing: a body written from a reader, with the
    /// length promised up front and never held in memory.
    #[test]
    fn a_streamed_body_reaches_the_wire_with_its_length() {
        let payload = b"the quick brown fox".to_vec();
        let r = Response::stream(200, payload.len() as u64, std::io::Cursor::new(payload.clone()))
            .header("Content-Type", "text/plain");

        let (head, body) = send_to_wire(r, "GET");
        assert!(head.contains("200"), "{head}");
        assert!(head.contains(&format!("Content-Length: {}", payload.len())), "{head}");
        assert_eq!(body, payload);
    }

    /// A streamed response gets no automatic ETag, because supplying one would
    /// mean reading the body to hash it, which is the whole thing being
    /// avoided. A handler that streams sets its own.
    #[test]
    fn a_streamed_body_is_not_hashed_for_an_etag() {
        let payload = b"abcdef".to_vec();
        let r = Response::stream(200, payload.len() as u64, std::io::Cursor::new(payload));
        let (head, _) = send_to_wire(r, "GET");
        assert!(!head.to_lowercase().contains("etag"), "{head}");

        // A byte body still gets one, which is the behaviour that existed
        // before and that m6-http's cache-hit revalidation depends on.
        let (head, _) = send_to_wire(Response::text("abcdef"), "GET");
        assert!(head.to_lowercase().contains("etag"), "{head}");
    }

    /// The property that makes the transforms safe: a stream has no bytes to
    /// hand to a minifier or a compressor. This is enforced by the type rather
    /// than by a flag, which is why `Body` is a sum type and not a `Vec<u8>`
    /// beside an optional reader.
    #[test]
    fn a_stream_offers_no_bytes_to_transform() {
        let s = Body::Stream { len: 4, reader: Box::new(std::io::Cursor::new(b"abcd".to_vec())) };
        assert!(s.as_bytes().is_none());
        assert_eq!(s.len(), 4);
        // And is not "empty", which is the test the pipeline steps use to skip
        // work: a zero-length stream is still a stream.
        assert!(!s.is_empty());
        let zero = Body::Stream { len: 0, reader: Box::new(std::io::empty()) };
        assert!(!zero.is_empty());

        let b = Body::Bytes(b"abcd".to_vec());
        assert_eq!(b.as_bytes(), Some(&b"abcd"[..]));
        assert!(Body::empty().is_empty());
    }

    /// A HEAD reports what the GET would send and sends no body, and that rule
    /// belongs to the responder, so it has to hold for a stream too.
    #[test]
    fn a_head_of_a_streamed_response_reports_the_length_and_no_body() {
        let payload = b"0123456789".to_vec();
        let r = Response::stream(200, payload.len() as u64, std::io::Cursor::new(payload));
        let (head, body) = send_to_wire(r, "HEAD");
        assert!(head.contains("Content-Length: 10"), "{head}");
        assert!(body.is_empty(), "a HEAD must not carry a body");
    }

    /// `Response::stream` is verbatim by construction; a buffered response
    /// says so explicitly.
    #[test]
    fn streams_are_verbatim_and_buffered_responses_opt_in() {
        assert!(Response::stream(200, 0, std::io::empty()).verbatim);
        assert!(!Response::text("hi").verbatim);
        assert!(Response::text("hi").verbatim().verbatim);
    }
}
