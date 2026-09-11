//! Building a `Set-Cookie` header value.
//!
//! There were four formatters for this, each a `format!` with a different
//! attribute set, and the differences between them are security-relevant
//! rather than cosmetic: one carries `HttpOnly`, another deliberately does
//! not; one carries `Secure` and `SameSite`, the others carry neither. Spread
//! across four `format!` calls, the question "which of our cookies are
//! HttpOnly?" has no answer short of reading all four.
//!
//! This is one place to ask it. It does not decide policy — each caller still
//! says what it wants — but the set of things a cookie can be is written down
//! once, and a new cookie is written by naming attributes rather than by
//! copying whichever nearby `format!` looked closest.
//!
//! # Attribute order
//!
//! Attributes are order-independent (RFC 6265 4.1.1), and the four call sites
//! did not agree on an order anyway. The order here is the conventional one:
//! `Max-Age`, `Domain`, `Path`, `Secure`, `HttpOnly`, `SameSite`.

use std::fmt::Write as _;

/// `SameSite` values, spelled as they go on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl SameSite {
    fn as_str(self) -> &'static str {
        match self {
            SameSite::Strict => "Strict",
            SameSite::Lax => "Lax",
            SameSite::None => "None",
        }
    }
}

/// A `Set-Cookie` value under construction.
///
/// Nothing is set by default except the name and value. `Path` in particular
/// is not implied: every existing caller wants `/`, and every one of them says
/// so, because a cookie whose path is inherited from the request URL is a
/// different cookie depending on which page set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    name: String,
    value: String,
    max_age: Option<i64>,
    domain: Option<String>,
    path: Option<String>,
    secure: bool,
    http_only: bool,
    same_site: Option<SameSite>,
}

impl Cookie {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            max_age: None,
            domain: None,
            path: None,
            secure: false,
            http_only: false,
            same_site: None,
        }
    }

    /// A cookie that deletes the one of the same name: empty value,
    /// `Max-Age=0`.
    ///
    /// The attributes still have to match the cookie being removed closely
    /// enough for the browser to consider it the same cookie, which is why
    /// this is a starting point rather than a finished header: `Path` in
    /// particular must match or the removal silently does nothing.
    pub fn removal(name: impl Into<String>) -> Self {
        Self::new(name, "").max_age(0)
    }

    pub fn max_age(mut self, seconds: i64) -> Self {
        self.max_age = Some(seconds);
        self
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Not sent over plain HTTP.
    pub fn secure(mut self) -> Self {
        self.secure = true;
        self
    }

    /// Not readable from JavaScript.
    ///
    /// Worth being deliberate about: a double-submit CSRF token that the page
    /// reads from JavaScript cannot have this, and a session token must.
    pub fn http_only(mut self) -> Self {
        self.http_only = true;
        self
    }

    pub fn same_site(mut self, policy: SameSite) -> Self {
        self.same_site = Some(policy);
        self
    }

    /// The header value, without the `Set-Cookie:` name.
    pub fn to_header_value(&self) -> String {
        let mut out = String::with_capacity(64);
        out.push_str(&self.name);
        out.push('=');
        out.push_str(&self.value);
        if let Some(m) = self.max_age {
            let _ = write!(out, "; Max-Age={m}");
        }
        if let Some(d) = &self.domain {
            let _ = write!(out, "; Domain={d}");
        }
        if let Some(p) = &self.path {
            let _ = write!(out, "; Path={p}");
        }
        if self.secure {
            out.push_str("; Secure");
        }
        if self.http_only {
            out.push_str("; HttpOnly");
        }
        if let Some(s) = self.same_site {
            let _ = write!(out, "; SameSite={}", s.as_str());
        }
        out
    }

    /// The full `(name, value)` pair, ready to push onto a header list.
    pub fn to_header(&self) -> (String, String) {
        ("Set-Cookie".to_string(), self.to_header_value())
    }
}

impl std::fmt::Display for Cookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_header_value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four call sites this replaced, pinned by their attribute SETS.
    ///
    /// The sets differ and the differences matter. `_csrf` has no `HttpOnly`
    /// and the others do; `_csrf` has `Secure` and `SameSite` and the others
    /// have neither. Before this module those four facts lived in four
    /// separate `format!` calls and there was no way to see them together.
    #[test]
    fn reproduces_the_four_call_sites() {
        // Response::cookie(name, value, max_age)
        let ordinary = Cookie::new("session", "abc123")
            .max_age(3600)
            .path("/")
            .http_only();
        assert_eq!(
            ordinary.to_header_value(),
            "session=abc123; Max-Age=3600; Path=/; HttpOnly"
        );

        // Response::flash
        let flash = Cookie::new("_flash", "payload.sig")
            .max_age(120)
            .path("/")
            .http_only();
        assert_eq!(
            flash.to_header_value(),
            "_flash=payload.sig; Max-Age=120; Path=/; HttpOnly"
        );

        // app.rs, clearing the flash after reading it
        let cleared = Cookie::removal("_flash").path("/").http_only();
        assert_eq!(cleared.to_header_value(), "_flash=; Max-Age=0; Path=/; HttpOnly");

        // app.rs, the CSRF double-submit token. No HttpOnly, deliberately.
        let csrf = Cookie::new("_csrf", "token")
            .path("/")
            .same_site(SameSite::Strict)
            .secure();
        assert_eq!(
            csrf.to_header_value(),
            "_csrf=token; Path=/; Secure; SameSite=Strict"
        );
        assert!(
            !csrf.to_header_value().contains("HttpOnly"),
            "the double-submit token is read by the page; HttpOnly would break it"
        );
    }

    /// A removal that does not match the original's Path silently does
    /// nothing: the browser treats it as a different cookie and keeps both.
    #[test]
    fn removal_is_a_starting_point_not_a_finished_header() {
        let bare = Cookie::removal("sid");
        assert_eq!(bare.to_header_value(), "sid=; Max-Age=0");
        assert!(
            !bare.to_header_value().contains("Path"),
            "Path is not implied; it must match the cookie being removed"
        );
    }

    #[test]
    fn nothing_is_set_by_default() {
        assert_eq!(Cookie::new("a", "b").to_header_value(), "a=b");
    }

    #[test]
    fn attribute_order_is_fixed() {
        let c = Cookie::new("n", "v")
            .same_site(SameSite::Lax)
            .http_only()
            .secure()
            .path("/x")
            .domain("example.com")
            .max_age(5);
        // Built in one order, emitted in another: the emitted order is the
        // module's, not the caller's.
        assert_eq!(
            c.to_header_value(),
            "n=v; Max-Age=5; Domain=example.com; Path=/x; Secure; HttpOnly; SameSite=Lax"
        );
    }

    #[test]
    fn to_header_names_the_field() {
        let (name, value) = Cookie::new("k", "v").to_header();
        assert_eq!(name, "Set-Cookie");
        assert_eq!(value, "k=v");
    }

    /// Round trip against the reader, so the two halves are tested against
    /// each other rather than each against its own idea of the format.
    #[test]
    fn what_we_write_is_what_we_read() {
        let c = Cookie::new("sid", "abc123").max_age(60).path("/").http_only();
        let header = c.to_header_value();
        // A browser sends back only `name=value`, so that is what the reader
        // is given here.
        let sent_back = header.split(';').next().unwrap();
        assert_eq!(crate::request::cookie(sent_back, "sid"), Some("abc123"));
    }
}
