//! Security regression tests for m6-auth-server (finding 8: open redirect).
//!
//! These assert the **secure** behaviour. They failed when written and pass now
//! that the finding is fixed.
//!
//! `validate_next` and the refresh handler's `Referer` check are private to the
//! binary crate, but both now delegate to `m6_core::is_same_origin_path`, so
//! these tests exercise the *real* predicate rather than a copy of it that
//! could silently drift out of sync with the code being protected.

use m6_core::is_same_origin_path;

/// `validate_next` — `src/handlers.rs`. Mirrors the caller's fallback so the
/// assertions read as the redirect actually issued.
fn validate_next(next: Option<&str>) -> String {
    match next {
        Some(n) if is_same_origin_path(n) => n.to_string(),
        _ => "/".to_string(),
    }
}

/// The `Referer` predicate in `handle_refresh_browser` — `src/handlers.rs`.
fn refresh_redirect_location(referer: Option<&str>) -> String {
    referer
        .filter(|r| is_same_origin_path(r))
        .unwrap_or("/")
        .to_string()
}

/// A `Location` is off-site if a browser resolves it to another origin.
/// `//host` is protocol-relative and `/\host` is normalised to the same thing.
fn is_offsite(location: &str) -> bool {
    let b = location.as_bytes();
    b.len() >= 2 && b[0] == b'/' && (b[1] == b'/' || b[1] == b'\\')
}

// ── Finding 8: open redirect after login ─────────────────────────────────────

/// Original defect: `?next=//evil.com` starts with `/`, so it passed the
/// `starts_with('/')` check and was used verbatim as the post-login
/// `Location`. Browsers resolve a protocol-relative URL against the current
/// scheme and navigate off-site.
///
/// Property: a post-login redirect must stay on this origin.
#[test]
fn finding_8_login_next_must_not_allow_protocol_relative_redirect() {
    let next = validate_next(Some("//evil.com/phish"));

    assert!(
        !is_offsite(&next),
        "post-login Location `{next}` redirects to another origin"
    );
    assert_eq!(next, "/", "an off-site target must fall back to the site root");
}

/// The backslash form gets the same treatment from browsers.
///
/// Property: `/\host` must not be accepted as a same-origin path.
#[test]
fn finding_8b_login_next_must_not_allow_backslash_redirect() {
    let next = validate_next(Some(r"/\evil.com"));

    assert!(
        !is_offsite(&next),
        "`{next}` is normalised to `//evil.com` by browsers and escapes the origin"
    );
    assert_eq!(next, "/");
}

/// The refresh handler applied the identical predicate to a header the client
/// fully controls, so `/auth/refresh` was a second open redirect.
///
/// Property: the refresh redirect must stay on this origin.
#[test]
fn finding_8c_refresh_referer_must_not_allow_protocol_relative_redirect() {
    let location = refresh_redirect_location(Some("//evil.com/phish"));

    assert!(
        !is_offsite(&location),
        "/auth/refresh redirects off-site to `{location}` based on a \
         client-supplied Referer"
    );
    assert_eq!(location, "/");
}

/// Guards against over-correction: ordinary same-origin paths must keep
/// working, or the fix would have broken the login flow it protects.
#[test]
fn finding_8_legitimate_relative_paths_still_accepted() {
    assert_eq!(validate_next(Some("/dashboard")), "/dashboard");
    assert_eq!(validate_next(Some("/a/b?c=d")), "/a/b?c=d");
    assert_eq!(validate_next(Some("/")), "/");
    assert_eq!(refresh_redirect_location(Some("/admin/page")), "/admin/page");

    // Absolute URLs and missing values fall back to the site root.
    assert_eq!(validate_next(Some("https://evil.com")), "/");
    assert_eq!(validate_next(None), "/");
}
