use std::cell::RefCell;

use m6_auth::{AuthError, Db};
use m6_core::http::{RawRequest, RawResponse};
// Form parsing, percent coding and cookie lookup are core's. This file had its
// own of each; `url_decode` in particular was a second copy of the Latin-1
// defect that reached production through a contact form, and it had to be
// found and fixed twice. That is the argument for consolidating, so they are
// gone rather than merely correct.
use m6_core::log::{info, warn};
use m6_core::request::{cookie, parse_form_body, url_encode_path};

use crate::jwt::{hash_token, now_secs, AccessClaims, RefreshClaims};
use crate::keys::KeyMaterial;
use crate::rate_limit::RateLimiter;

/// The signing keys, token lifetimes and login throttle the loop owns.
pub struct AppState {
    pub keys: KeyMaterial,
    pub access_ttl: u64,
    pub refresh_ttl: u64,
    pub issuer: String,
    /// Borrowed only for the length of one call. The loop is one thread, and no
    /// borrow is held across a call that returns to it, so a borrow cannot fail
    /// at run time.
    pub rate_limiter: RefCell<RateLimiter>,
}

// The handlers are reached by route: `Site` matches the path and the method,
// and the loop's one `match` calls the wrapper for the route's index.
// The wrappers exist so the handler functions themselves stay private and
// unchanged -- they are the security-carrying part of this service, and a
// migration is the wrong time to touch them.

/// A request the loop must try again once the database's lock is free.
#[derive(Debug)]
pub struct Busy {
    /// The user a login verified before its refresh-token write met the lock,
    /// so the retry skips bcrypt. Empty on every other route.
    pub verified: Option<m6_auth::User>,
}

pub fn dispatch_login(
    req: &RawRequest,
    state: &AppState,
    db: &Db,
    peer_ip: &str,
    verified: Option<&m6_auth::User>,
) -> Result<RawResponse, Busy> {
    handle_login(req, state, db, peer_ip, verified)
}

pub fn dispatch_refresh(req: &RawRequest, state: &AppState, db: &Db) -> Result<RawResponse, Busy> {
    handle_refresh(req, state, db)
}

pub fn dispatch_logout(req: &RawRequest, state: &AppState, db: &Db) -> Result<RawResponse, Busy> {
    handle_logout(req, state, db)
}

pub fn dispatch_public_key(state: &AppState) -> RawResponse {
    handle_public_key(state)
}

pub fn dispatch_check(req: &RawRequest, state: &AppState, db: &Db) -> RawResponse {
    handle_check(req, state, db)
}

// ─── POST /auth/login ─────────────────────────────────────────────────────────

fn handle_login(
    req: &RawRequest,
    state: &AppState,
    db: &Db,
    peer_ip: &str,
    verified: Option<&m6_auth::User>,
) -> Result<RawResponse, Busy> {
    let ct = req.content_type().unwrap_or("").to_ascii_lowercase();
    let is_form = ct.contains("application/x-www-form-urlencoded");
    let is_json = ct.contains("application/json");

    // Rate limiting (applied regardless of content type).
    //
    // A read, not a read-and-increment. The counter now moves only when a
    // password was actually wrong, so a caller that always succeeds is never
    // throttled. See `rate_limit::RateLimiter` for what counting successes cost.
    {
        let rl = state.rate_limiter.borrow();
        if rl.is_blocked(peer_ip) {
            warn!(ip = %peer_ip, "rate limit exceeded on login");
            if is_json {
                return Ok(RawResponse::new(429)
                    .header("Retry-After", "60")
                    .content_type("application/json")
                    .body(r#"{"error":"rate_limited"}"#));
            } else {
                return Ok(RawResponse::new(429)
                    .header("Retry-After", "60")
                    .body("Too Many Requests"));
            }
        }
    }

    if is_json {
        handle_login_json(req, state, db, peer_ip, verified)
    } else if is_form {
        handle_login_form(req, state, db, peer_ip, verified)
    } else {
        // Default to form handling for unknown content types
        handle_login_form(req, state, db, peer_ip, verified)
    }
}

/// Count a wrong password against `peer_ip`'s budget.
fn note_login_failure(state: &AppState, peer_ip: &str) {
    state.rate_limiter.borrow_mut().record_failure(peer_ip);
}

/// Forget `peer_ip`'s failures after a correct password.
fn note_login_success(state: &AppState, peer_ip: &str) {
    state.rate_limiter.borrow_mut().clear(peer_ip);
}

fn form_field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

use m6_core::is_same_origin_path;

fn validate_next(next: Option<&str>) -> String {
    match next {
        Some(n) if is_same_origin_path(n) => n.to_string(),
        _ => "/".to_string(),
    }
}

/// What the login sequence shared by the form and JSON handlers did.
enum LoginOutcome {
    /// The password was right and both tokens were issued.
    Tokens(String, String),
    /// No user has that password, or the username does not exist.
    InvalidCredentials,
    /// The database is locked. The user is the one the attempt verified, so the
    /// loop's retry skips bcrypt.
    Busy(Option<m6_auth::User>),
    /// The answer the sequence built for a failure that is not a lock.
    Response(RawResponse),
}

/// Take the user a retry already verified, or verify the password, then finish
/// the login. The form and JSON handlers keep only their own parse and answer.
fn verified_login(
    state: &AppState,
    db: &Db,
    username: &str,
    password: &str,
    peer_ip: &str,
    verified: Option<&m6_auth::User>,
) -> LoginOutcome {
    // The user a first attempt verified, kept for the loop when the token store
    // meets a lock. Empty on a retry, which already holds it.
    let mut verified_here: Option<m6_auth::User> = None;
    let user: &m6_auth::User = match verified {
        Some(user) => user,
        None => match db.user_verify_password(username, password) {
            Ok(Some(u)) => &*verified_here.insert(u),
            Ok(None) => {
                warn!(ip = %peer_ip, reason = "invalid_credentials", "login failure");
                note_login_failure(state, peer_ip);
                return LoginOutcome::InvalidCredentials;
            }
            Err(e) if e.is_busy() => return LoginOutcome::Busy(None),
            Err(e) => {
                warn!(error = %e, "db error on login");
                return LoginOutcome::Response(internal_error());
            }
        },
    };

    match finish_login(state, db, user, peer_ip) {
        Ok((access_jwt, refresh_jwt)) => LoginOutcome::Tokens(access_jwt, refresh_jwt),
        Err(LoginFailure::Busy) => LoginOutcome::Busy(verified_here),
        Err(LoginFailure::Response(resp)) => LoginOutcome::Response(resp),
    }
}

fn handle_login_form(
    req: &RawRequest,
    state: &AppState,
    db: &Db,
    peer_ip: &str,
    verified: Option<&m6_auth::User>,
) -> Result<RawResponse, Busy> {
    let fields = parse_form_body(&req.body);
    let username = form_field(&fields, "username").unwrap_or("");
    let password = form_field(&fields, "password").unwrap_or("");
    let next_raw = form_field(&fields, "next");
    let next = validate_next(next_raw);

    match verified_login(state, db, username, password, peer_ip, verified) {
        LoginOutcome::Tokens(access_jwt, refresh_jwt) => {
            let session_cookie = format!(
                "session={}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={}",
                access_jwt, state.access_ttl
            );
            let refresh_cookie = format!(
                "refresh={}; HttpOnly; Secure; SameSite=Lax; Path=/auth/refresh; Max-Age={}",
                refresh_jwt, state.refresh_ttl
            );

            Ok(RawResponse::new(302)
                .header("Location", next)
                .header("Set-Cookie", session_cookie)
                .header("Set-Cookie", refresh_cookie))
        }
        LoginOutcome::InvalidCredentials => {
            let next_enc = url_encode_path(&next);
            Ok(RawResponse::new(302).header(
                "Location",
                format!("/login?error=invalid&next={}", next_enc),
            ))
        }
        LoginOutcome::Busy(verified) => Err(Busy { verified }),
        LoginOutcome::Response(resp) => Ok(resp),
    }
}

fn handle_login_json(
    req: &RawRequest,
    state: &AppState,
    db: &Db,
    peer_ip: &str,
    verified: Option<&m6_auth::User>,
) -> Result<RawResponse, Busy> {
    #[derive(serde::Deserialize)]
    struct LoginReq {
        username: String,
        password: String,
    }

    let body: LoginReq = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(_) => {
            return Ok(RawResponse::new(400)
                .content_type("application/json")
                .body(r#"{"error":"bad_request"}"#))
        }
    };

    match verified_login(state, db, &body.username, &body.password, peer_ip, verified) {
        LoginOutcome::Tokens(access_jwt, refresh_jwt) => {
            let resp_body = serde_json::json!({
                "access_token":  access_jwt,
                "refresh_token": refresh_jwt,
                "expires_in":    state.access_ttl,
            });

            Ok(RawResponse::new(200)
                .content_type("application/json")
                .body(resp_body.to_string()))
        }
        LoginOutcome::InvalidCredentials => Ok(RawResponse::new(401)
            .content_type("application/json")
            .body(r#"{"error":"invalid_credentials"}"#)),
        LoginOutcome::Busy(verified) => Err(Busy { verified }),
        LoginOutcome::Response(resp) => Ok(resp),
    }
}

// ─── POST /auth/refresh ───────────────────────────────────────────────────────

fn handle_refresh(req: &RawRequest, state: &AppState, db: &Db) -> Result<RawResponse, Busy> {
    let ct = req.content_type().unwrap_or("").to_ascii_lowercase();
    let is_json = ct.contains("application/json");

    if is_json {
        handle_refresh_json(req, state, db)
    } else {
        handle_refresh_browser(req, state, db)
    }
}

fn handle_refresh_browser(
    req: &RawRequest,
    state: &AppState,
    db: &Db,
) -> Result<RawResponse, Busy> {
    let token = match req
        .header("cookie")
        .and_then(|h| cookie(h, "refresh"))
        .map(|v| v.to_string())
    {
        Some(t) => t,
        None => return Ok(RawResponse::new(302).header("Location", "/login")),
    };

    match do_refresh(state, db, &token) {
        Ok((access_jwt, user_id)) => {
            info!(user_id = %user_id, "token refresh");
            let location = req
                .header("referer")
                .filter(|r| is_same_origin_path(r))
                .unwrap_or("/")
                .to_string();

            let session_cookie = format!(
                "session={}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={}",
                access_jwt, state.access_ttl
            );
            Ok(RawResponse::new(302)
                .header("Location", location)
                .header("Set-Cookie", session_cookie))
        }
        Err(e) if is_busy(&e) => Err(Busy { verified: None }),
        Err(_) => Ok(RawResponse::new(302).header("Location", "/login")),
    }
}

fn handle_refresh_json(req: &RawRequest, state: &AppState, db: &Db) -> Result<RawResponse, Busy> {
    #[derive(serde::Deserialize)]
    struct RefreshReq {
        refresh_token: String,
    }

    let body: RefreshReq = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(_) => {
            return Ok(RawResponse::new(400)
                .content_type("application/json")
                .body(r#"{"error":"bad_request"}"#))
        }
    };

    match do_refresh(state, db, &body.refresh_token) {
        Ok((access_jwt, user_id)) => {
            info!(user_id = %user_id, "token refresh");
            let resp_body = serde_json::json!({
                "access_token": access_jwt,
                "expires_in":   state.access_ttl,
            });
            Ok(RawResponse::new(200)
                .content_type("application/json")
                .body(resp_body.to_string()))
        }
        Err(e) if is_busy(&e) => Err(Busy { verified: None }),
        Err(_) => Ok(RawResponse::new(401)
            .content_type("application/json")
            .body(r#"{"error":"invalid_token"}"#)),
    }
}

/// Whether a refresh's `anyhow` error wraps a locked-database `AuthError`.
/// `do_refresh`'s `?` passes the `AuthError` through unchanged.
fn is_busy(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<AuthError>()
        .is_some_and(AuthError::is_busy)
}

fn do_refresh(state: &AppState, db: &Db, token: &str) -> anyhow::Result<(String, String)> {
    // A refresh token is an opaque secret, verified by its stored hash alone.
    // The hash row carries revocation (a revoked token has no row) and expiry
    // (`expires_at`). A JWT signature check against the current key would reject
    // every token minted before a rotation, because only the current key is held.
    // See `keys.rs` for the rotation semantics.
    let token_hash = hash_token(token);
    let user_id = db
        .refresh_token_verify(&token_hash)?
        .ok_or_else(|| anyhow::anyhow!("token not found or expired"))?;

    // Load user to get fresh groups/roles
    let user = db
        .user_get_by_id(&user_id)?
        .ok_or_else(|| anyhow::anyhow!("user not found"))?;

    let now = now_secs();
    let access_claims = AccessClaims {
        iss: state.issuer.clone(),
        sub: user.id.clone(),
        exp: now + state.access_ttl as i64,
        iat: now,
        username: user.username.clone(),
        groups: user.groups.clone(),
        roles: user.roles.clone(),
    };

    // Read the keys directly: the state is built once at start and shared.
    let access_jwt = state.keys.jwt.encode_access(&access_claims)?;
    Ok((access_jwt, user.id))
}

// ─── POST /auth/logout ────────────────────────────────────────────────────────

fn handle_logout(req: &RawRequest, state: &AppState, db: &Db) -> Result<RawResponse, Busy> {
    let is_api = req
        .header("authorization")
        .map(|v| v.to_ascii_lowercase().starts_with("bearer "))
        .unwrap_or(false);

    if is_api {
        handle_logout_api(req, state, db)
    } else {
        handle_logout_browser(req, db)
    }
}

fn handle_logout_browser(req: &RawRequest, db: &Db) -> Result<RawResponse, Busy> {
    if let Some(token) = req
        .header("cookie")
        .and_then(|h| cookie(h, "refresh"))
        .map(|v| v.to_string())
    {
        let token_hash = hash_token(&token);
        // A failed revoke is ignored, as it always was, except for a lock: the
        // loop tries the request again rather than answering over a database
        // write that did not happen.
        if let Err(e) = db.refresh_token_revoke(&token_hash) {
            if e.is_busy() {
                return Err(Busy { verified: None });
            }
        }
    }
    info!("logout (browser)");

    let clear_session = "session=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0";
    let clear_refresh = "refresh=; HttpOnly; Secure; SameSite=Lax; Path=/auth/refresh; Max-Age=0";

    Ok(RawResponse::new(302)
        .header("Location", "/")
        .header("Set-Cookie", clear_session)
        .header("Set-Cookie", clear_refresh))
}

fn handle_logout_api(req: &RawRequest, state: &AppState, db: &Db) -> Result<RawResponse, Busy> {
    // Extract user_id from bearer token, revoke all their refresh tokens
    if let Some(auth) = req.header("authorization") {
        let token = auth
            .trim_start_matches("Bearer ")
            .trim_start_matches("bearer ");
        let decode_result = state.keys.jwt.decode_access(token).ok();
        if let Some(data) = decode_result {
            let user_id = &data.claims.sub;
            if let Err(e) = db.refresh_tokens_revoke_all(user_id) {
                if e.is_busy() {
                    return Err(Busy { verified: None });
                }
            }
            info!(user_id = %user_id, "logout (api)");
        }
    }
    Ok(RawResponse::new(204))
}

// ─── GET /auth/public-key ─────────────────────────────────────────────────────

fn handle_public_key(state: &AppState) -> RawResponse {
    let pem = state.keys.public_key_pem.clone();
    RawResponse::new(200)
        .content_type("application/x-pem-file")
        .body(pem)
}

// ─── POST /check ─────────────────────────────────────────────────────────────

/// The longest an answer may be cached for, in milliseconds.
///
/// The edge applies its own `[auth] decision_ttl_ms` on top, and the token's
/// remaining life caps the answer below this. A reload of this service does not
/// change the value, so an answer is never extended by a restart.
const CHECK_TTL_MS: u64 = 5_000;

/// Answer the current identity for an access token.
///
/// The token proves which user is asking. The roles and groups come from the
/// database, so a role change is visible without a new token, which is the
/// reason this route exists. The answer carries a relative `ttl_ms` and no
/// timestamp: the edge never compares wall clocks, so a clock step there cannot
/// extend a decision.
///
/// A locked database is 503 at once. The edge has its own 50 ms deadline and
/// will have given up long before this service's one-second deferral would
/// answer, so deferring here would only waste the connection. A 503 is also not
/// a 401: the credential may be valid, and the edge must not cache a refusal
/// the authority did not make.
fn handle_check(req: &RawRequest, state: &AppState, db: &Db) -> RawResponse {
    let Some(token) = bearer_token(req) else {
        return unauthorised();
    };
    // Verifies the signature, the issuer and the expiry. The claims are used
    // only for `sub` and `exp`; the identity itself is the database's.
    let Ok(data) = state.keys.jwt.decode_access(token) else {
        return unauthorised();
    };
    let user = match db.user_get_by_id(&data.claims.sub) {
        Ok(Some(user)) => user,
        // A deleted user is a revoked user. An empty role list would be an
        // answer that the edge could cache and act on, so it is refused.
        Ok(None) => return unauthorised(),
        Err(e) => return db_failure(e),
    };
    let groups = match db.user_groups(&user.username) {
        Ok(groups) => groups,
        Err(e) => return db_failure(e),
    };

    let ttl_ms = remaining_ms(data.claims.exp).min(CHECK_TTL_MS);
    let body = serde_json::json!({
        "sub":      user.id,
        "username": user.username,
        "roles":    user.roles,
        "groups":   groups,
        "ttl_ms":   ttl_ms,
    });
    RawResponse::new(200)
        .header("Cache-Control", "no-store")
        .content_type("application/json")
        .body(body.to_string())
}

/// The bearer token from `Authorization`, or `None`.
fn bearer_token(req: &RawRequest) -> Option<&str> {
    let (scheme, token) = req.header("authorization")?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// How long the token has left, in milliseconds, rounded down.
///
/// Milliseconds on both sides: `exp` is whole seconds, so subtracting whole
/// seconds first would report up to 999 ms more life than the token has, and an
/// answer that outlives its token is the one way this design can be wrong.
fn remaining_ms(exp: i64) -> u64 {
    let now_ms = m6_core::platform::wall()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    exp.saturating_mul(1_000).saturating_sub(now_ms).max(0) as u64
}

/// Map a database failure to an answer.
///
/// A lock is transient and the credential may be valid, so it is 503 and never
/// a 401: the caller caches a 401 and must not cache this.
fn db_failure(error: AuthError) -> RawResponse {
    if error.is_busy() {
        return unavailable();
    }
    warn!(error = %error, "db error on check");
    internal_error()
}

fn unauthorised() -> RawResponse {
    RawResponse::new(401)
        .content_type("application/json")
        .body(r#"{"error":"unauthorised"}"#)
}

fn unavailable() -> RawResponse {
    RawResponse::new(503)
        .content_type("application/json")
        .body(r#"{"error":"unavailable"}"#)
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Why a login whose password was right has not answered.
enum LoginFailure {
    /// The refresh-token write met a locked database: the loop tries again.
    Busy,
    /// The answer to send.
    Response(RawResponse),
}

/// Complete a login whose password was right: clear the IP's failure count,
/// issue both tokens, store the refresh hash, and log the success. The form and
/// JSON handlers share this sequence so the two cannot drift apart.
fn finish_login(
    state: &AppState,
    db: &Db,
    user: &m6_auth::User,
    peer_ip: &str,
) -> Result<(String, String), LoginFailure> {
    note_login_success(state, peer_ip);

    let (access_jwt, refresh_jwt) =
        issue_tokens(state, &user.id, &user.username, &user.groups, &user.roles).map_err(|e| {
            warn!(error = %e, "failed to issue tokens");
            LoginFailure::Response(internal_error())
        })?;

    let refresh_hash = hash_token(&refresh_jwt);
    let now = now_secs();
    if let Err(e) = db.refresh_token_store(&user.id, &refresh_hash, now + state.refresh_ttl as i64)
    {
        // The login keeps the user it verified, so the retry skips bcrypt.
        if e.is_busy() {
            return Err(LoginFailure::Busy);
        }
        warn!(error = %e, "failed to store refresh token");
        return Err(LoginFailure::Response(internal_error()));
    }

    info!(username = %user.username, ip = %peer_ip, "login success");
    Ok((access_jwt, refresh_jwt))
}

fn issue_tokens(
    state: &AppState,
    user_id: &str,
    username: &str,
    groups: &[String],
    roles: &[String],
) -> anyhow::Result<(String, String)> {
    let now = now_secs();

    let access_claims = AccessClaims {
        iss: state.issuer.clone(),
        sub: user_id.to_string(),
        exp: now + state.access_ttl as i64,
        iat: now,
        username: username.to_string(),
        groups: groups.to_vec(),
        roles: roles.to_vec(),
    };

    let refresh_claims = RefreshClaims {
        iss: state.issuer.clone(),
        sub: user_id.to_string(),
        exp: now + state.refresh_ttl as i64,
        iat: now,
        username: username.to_string(),
        token_type: "refresh".to_string(),
    };

    let access_jwt = state.keys.jwt.encode_access(&access_claims)?;
    let refresh_jwt = state.keys.jwt.encode_refresh(&refresh_claims)?;
    Ok((access_jwt, refresh_jwt))
}

pub fn internal_error() -> RawResponse {
    RawResponse::new(500)
        .content_type("application/json")
        .body(r#"{"error":"internal_error"}"#)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::test_key_material;
    use crate::rate_limit::RateLimiter;

    /// A retried login carries the user the first attempt verified, so it must
    /// not run bcrypt again: a request with the wrong password issues tokens,
    /// which it can do only by skipping the verify.
    #[test]
    fn a_retried_login_does_not_verify_the_password_again() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let db = Db::open(&dir.path().join("auth.db")).expect("open the database");
        let user = db
            .user_create("alice", "correct_pass", &["user"])
            .expect("create the user");
        let state = AppState {
            keys: test_key_material(),
            access_ttl: 900,
            refresh_ttl: 2592000,
            issuer: "test.example.com".to_string(),
            rate_limiter: RefCell::new(RateLimiter::with_limits(5, 900)),
        };
        let req = RawRequest {
            method: "POST".to_string(),
            path: "/auth/login".to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: br#"{"username":"alice","password":"wrong"}"#.to_vec(),
        };

        let answer = dispatch_login(&req, &state, &db, "192.0.2.1", Some(&user))
            .expect("a retry of a verified login is not busy");
        assert_eq!(
            answer.status, 200,
            "the retry skipped the verify and issued tokens"
        );
        let body = String::from_utf8_lossy(&answer.body);
        assert!(body.contains("access_token"), "no access token: {body}");
        assert!(body.contains("refresh_token"), "no refresh token: {body}");
    }

    /// A lock is transient and the credential may be valid, so `/check` must
    /// answer 503 and not a 401 the caller would cache as a refusal.
    ///
    /// A real lock cannot reach this branch over the wire: every connection is
    /// in WAL mode, where a writer never blocks a reader. The classification
    /// is tested here, and the route's 401 for a dead credential over the wire.
    #[test]
    fn a_busy_database_error_is_503_not_401() {
        let busy = AuthError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(5),
            Some("database is locked".to_string()),
        ));
        assert_eq!(db_failure(busy).status, 503);

        let other = AuthError::Db(rusqlite::Error::InvalidQuery);
        assert_eq!(db_failure(other).status, 500);
    }
}
