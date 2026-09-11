/// HTTP response type and constructors.
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::request::Request;

/// An HTTP response ready for serialisation.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
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
            body: vec![],
            template_name: Some(template.to_string()),
            template_dict: Some(dict.clone()),
        })
    }

    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".to_string(), location.to_string())],
            body: vec![],
            template_name: None,
            template_dict: None,
        }
    }

    pub fn redirect_permanent(location: &str) -> Self {
        Self {
            status: 301,
            headers: vec![("Location".to_string(), location.to_string())],
            body: vec![],
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
            body,
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
            body: s.into().into_bytes(),
            template_name: None,
            template_dict: None,
        }
    }

    /// Replace the body, keeping status and headers.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
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
            body: s.as_bytes().to_vec(),
            template_name: None,
            template_dict: None,
        }
    }

    pub fn status(code: u16) -> Self {
        Self {
            status: code,
            headers: vec![],
            body: vec![],
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
    pub fn cookie(mut self, name: &str, value: &str, max_age: i64) -> Self {
        let cookie = if max_age == 0 {
            format!("{}=; Max-Age=0; Path=/; HttpOnly", name)
        } else {
            format!(
                "{}={}; Max-Age={}; Path=/; HttpOnly",
                name, value, max_age
            )
        };
        self.headers.push(("Set-Cookie".to_string(), cookie));
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
        let cookie = format!(
            "_flash={}; Max-Age=120; Path=/; HttpOnly",
            cookie_val
        );
        let mut s = self;
        s.headers.push(("Set-Cookie".to_string(), cookie));
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
    pub fn send<W: std::io::Write>(
        &self,
        resp: &mut crate::h1::Responder<'_, W>,
    ) -> anyhow::Result<()> {
        let has = |name: &str| self.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));

        let etag;
        let mut hdrs: Vec<(&str, &str)> =
            self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        if !has("content-type") && !self.body.is_empty() {
            hdrs.push(("Content-Type", "text/html; charset=utf-8"));
        }
        // A rendered page has no filesystem mtime to hang a Last-Modified off
        // of, but its body is a plain byte string — a content hash gives
        // conditional-GET (see m6-http's cache-hit `is_not_modified` check) a
        // real signal to compare against once this response is cached.
        if !has("etag") && !self.body.is_empty() {
            etag = format!("\"{:x}\"", content_hash(&self.body));
            hdrs.push(("ETag", etag.as_str()));
        }

        resp.send(self.status, &hdrs, &self.body)?;
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
