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
    pub(crate) template_name: Option<String>,
    /// Extra template context supplied by the handler via render_with.
    pub(crate) template_dict: Option<Map<String, Value>>,
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

    /// Internal: render a template from a dict directly.
    pub(crate) fn render_dict(
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

    /// Write the response directly to `w` — no intermediate Vec allocation.
    /// Uses BufWriter at the call site for efficient syscall batching.
    pub fn write_to<W: std::io::Write>(&self, w: &mut W) -> anyhow::Result<()> {
        let reason = reason_phrase(self.status);
        write!(w, "HTTP/1.1 {} {}\r\n", self.status, reason)?;

        let mut has_content_type = false;
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case("content-type") {
                has_content_type = true;
            }
            w.write_all(k.as_bytes())?;
            w.write_all(b": ")?;
            w.write_all(v.as_bytes())?;
            w.write_all(b"\r\n")?;
        }
        write!(w, "Content-Length: {}\r\n", self.body.len())?;
        if !has_content_type && !self.body.is_empty() {
            w.write_all(b"Content-Type: text/html; charset=utf-8\r\n")?;
        }
        w.write_all(b"\r\n")?;
        w.write_all(&self.body)?;
        Ok(())
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
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
