use std::sync::{Arc, Mutex, RwLock};

use m6_auth::Db;
use m6_core::http::{RawRequest, RawResponse};
use tracing::{info, warn};

use crate::jwt::{hash_token, now_secs, AccessClaims, RefreshClaims};
use crate::key_watch::KeyMaterial;
use crate::rate_limit::RateLimiter;

pub struct AppState {
    pub db:          Mutex<Db>,
    pub keys:        Arc<RwLock<KeyMaterial>>,
    pub access_ttl:  u64,
    pub refresh_ttl: u64,
    pub issuer:      String,
    pub rate_limiter: Mutex<RateLimiter>,
}

/// Main dispatcher.
pub fn dispatch(req: &RawRequest, state: &AppState, peer_ip: &str) -> RawResponse {
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/auth/login")   => handle_login(req, state, peer_ip),
        ("POST", "/auth/refresh") => handle_refresh(req, state),
        ("POST", "/auth/logout")  => handle_logout(req, state),
        ("GET",  "/auth/public-key") => handle_public_key(state),
        _ => RawResponse::new(404).body("Not Found"),
    }
}

// ─── POST /auth/login ─────────────────────────────────────────────────────────

fn handle_login(req: &RawRequest, state: &AppState, peer_ip: &str) -> RawResponse {
    let ct = req.content_type().unwrap_or("").to_ascii_lowercase();
    let is_form = ct.contains("application/x-www-form-urlencoded");
    let is_json = ct.contains("application/json");

    // Rate limiting (applied regardless of content type)
    {
        let mut rl = match state.rate_limiter.lock() {
            Ok(l) => l,
            Err(_) => return internal_error(),
        };
        if rl.check_and_increment(peer_ip) {
            warn!(ip = %peer_ip, "rate limit exceeded on login");
            if is_json {
                return RawResponse::new(429)
                    .header("Retry-After", "60")
                    .content_type("application/json")
                    .body(r#"{"error":"rate_limited"}"#);
            } else {
                return RawResponse::new(429)
                    .header("Retry-After", "60")
                    .body("Too Many Requests");
            }
        }
    }

    if is_json {
        handle_login_json(req, state, peer_ip)
    } else if is_form {
        handle_login_form(req, state, peer_ip)
    } else {
        // Default to form handling for unknown content types
        handle_login_form(req, state, peer_ip)
    }
}

fn parse_form_body(body: &[u8]) -> Vec<(String, String)> {
    let s = std::str::from_utf8(body).unwrap_or("");
    s.split('&')
        .filter_map(|pair| {
            let mut it = pair.splitn(2, '=');
            let k = it.next()?;
            let v = it.next().unwrap_or("");
            Some((url_decode(k), url_decode(v)))
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(h1), Some(h2)) = (h1, h2) {
                let hex = format!("{}{}", h1, h2);
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    out.push(byte as char);
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

fn form_field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn validate_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') => n.to_string(),
        _ => "/".to_string(),
    }
}

fn handle_login_form(req: &RawRequest, state: &AppState, peer_ip: &str) -> RawResponse {
    let fields = parse_form_body(&req.body);
    let username = form_field(&fields, "username").unwrap_or("");
    let password = form_field(&fields, "password").unwrap_or("");
    let next_raw = form_field(&fields, "next");
    let next = validate_next(next_raw);

    let db = match state.db.lock() {
        Ok(l) => l,
        Err(_) => return internal_error(),
    };

    let user = match db.user_verify_password(username, password) {
        Ok(Some(u)) => u,
        Ok(None) => {
            warn!(ip = %peer_ip, reason = "invalid_credentials", "login failure");
            let next_enc = url_encode(&next);
            return RawResponse::new(302)
                .header("Location", format!("/login?error=invalid&next={}", next_enc));
        }
        Err(e) => {
            warn!(error = %e, "db error on login");
            return internal_error();
        }
    };

    // Issue tokens
    let (access_jwt, refresh_jwt) = match issue_tokens(state, &user.id, &user.username, &user.groups, &user.roles) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "failed to issue tokens");
            return internal_error();
        }
    };

    // Store refresh token hash
    let refresh_hash = hash_token(&refresh_jwt);
    let now = now_secs();
    if let Err(e) = db.refresh_token_store(&user.id, &refresh_hash, now + state.refresh_ttl as i64) {
        warn!(error = %e, "failed to store refresh token");
        return internal_error();
    }

    info!(username = %user.username, ip = %peer_ip, "login success");

    let session_cookie = format!(
        "session={}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={}",
        access_jwt, state.access_ttl
    );
    let refresh_cookie = format!(
        "refresh={}; HttpOnly; Secure; SameSite=Strict; Path=/auth/refresh; Max-Age={}",
        refresh_jwt, state.refresh_ttl
    );

    RawResponse::new(302)
        .header("Location", next)
        .header("Set-Cookie", session_cookie)
        .header("Set-Cookie", refresh_cookie)
}

fn handle_login_json(req: &RawRequest, state: &AppState, peer_ip: &str) -> RawResponse {
    #[derive(serde::Deserialize)]
    struct LoginReq {
        username: String,
        password: String,
    }

    let body: LoginReq = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(_) => return RawResponse::new(400).content_type("application/json").body(r#"{"error":"bad_request"}"#),
    };

    let db = match state.db.lock() {
        Ok(l) => l,
        Err(_) => return internal_error(),
    };

    let user = match db.user_verify_password(&body.username, &body.password) {
        Ok(Some(u)) => u,
        Ok(None) => {
            warn!(ip = %peer_ip, reason = "invalid_credentials", "login failure");
            return RawResponse::new(401)
                .content_type("application/json")
                .body(r#"{"error":"invalid_credentials"}"#);
        }
        Err(e) => {
            warn!(error = %e, "db error on login");
            return internal_error();
        }
    };

    // Issue tokens
    let (access_jwt, refresh_jwt) = match issue_tokens(state, &user.id, &user.username, &user.groups, &user.roles) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "failed to issue tokens");
            return internal_error();
        }
    };

    // Store refresh token hash
    let refresh_hash = hash_token(&refresh_jwt);
    let now = now_secs();
    if let Err(e) = db.refresh_token_store(&user.id, &refresh_hash, now + state.refresh_ttl as i64) {
        warn!(error = %e, "failed to store refresh token");
        return internal_error();
    }

    info!(username = %user.username, ip = %peer_ip, "login success");

    let resp_body = serde_json::json!({
        "access_token":  access_jwt,
        "refresh_token": refresh_jwt,
        "expires_in":    state.access_ttl,
    });

    RawResponse::new(200)
        .content_type("application/json")
        .body(resp_body.to_string())
}

// ─── POST /auth/refresh ───────────────────────────────────────────────────────

fn handle_refresh(req: &RawRequest, state: &AppState) -> RawResponse {
    let ct = req.content_type().unwrap_or("").to_ascii_lowercase();
    let is_json = ct.contains("application/json");

    if is_json {
        handle_refresh_json(req, state)
    } else {
        handle_refresh_browser(req, state)
    }
}

fn handle_refresh_browser(req: &RawRequest, state: &AppState) -> RawResponse {
    let token = match extract_cookie(req, "refresh") {
        Some(t) => t,
        None => return RawResponse::new(302).header("Location", "/login"),
    };

    match do_refresh(state, &token) {
        Ok((access_jwt, user_id)) => {
            info!(user_id = %user_id, "token refresh");
            let location = req.header("referer")
                .filter(|r| r.starts_with('/'))
                .unwrap_or("/")
                .to_string();

            let session_cookie = format!(
                "session={}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={}",
                access_jwt, state.access_ttl
            );
            RawResponse::new(302)
                .header("Location", location)
                .header("Set-Cookie", session_cookie)
        }
        Err(_) => {
            RawResponse::new(302).header("Location", "/login")
        }
    }
}

fn handle_refresh_json(req: &RawRequest, state: &AppState) -> RawResponse {
    #[derive(serde::Deserialize)]
    struct RefreshReq {
        refresh_token: String,
    }

    let body: RefreshReq = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(_) => return RawResponse::new(400).content_type("application/json").body(r#"{"error":"bad_request"}"#),
    };

    match do_refresh(state, &body.refresh_token) {
        Ok((access_jwt, user_id)) => {
            info!(user_id = %user_id, "token refresh");
            let resp_body = serde_json::json!({
                "access_token": access_jwt,
                "expires_in":   state.access_ttl,
            });
            RawResponse::new(200)
                .content_type("application/json")
                .body(resp_body.to_string())
        }
        Err(_) => {
            RawResponse::new(401)
                .content_type("application/json")
                .body(r#"{"error":"invalid_token"}"#)
        }
    }
}

fn do_refresh(state: &AppState, token: &str) -> anyhow::Result<(String, String)> {
    // Decode and verify signature + expiry — read-lock keys for decode
    let claims = {
        let keys = state.keys.read().map_err(|_| anyhow::anyhow!("keys lock poisoned"))?;
        keys.jwt.decode_refresh(token)?.claims
    };

    // Verify hash in database
    let token_hash = hash_token(token);
    let db = state.db.lock().map_err(|_| anyhow::anyhow!("lock error"))?;
    let stored_user_id = db.refresh_token_verify(&token_hash)?
        .ok_or_else(|| anyhow::anyhow!("token not found or expired"))?;

    if stored_user_id != claims.sub {
        return Err(anyhow::anyhow!("user_id mismatch"));
    }

    // Load user to get fresh groups/roles
    let user = db.user_get_by_id(&claims.sub)?
        .ok_or_else(|| anyhow::anyhow!("user not found"))?;

    let now = now_secs();
    let access_claims = AccessClaims {
        iss:      state.issuer.clone(),
        sub:      user.id.clone(),
        exp:      now + state.access_ttl as i64,
        iat:      now,
        username: user.username.clone(),
        groups:   user.groups.clone(),
        roles:    user.roles.clone(),
    };

    // Read-lock keys for encode
    let access_jwt = {
        let keys = state.keys.read().map_err(|_| anyhow::anyhow!("keys lock poisoned"))?;
        keys.jwt.encode_access(&access_claims)?
    };
    Ok((access_jwt, user.id))
}

// ─── POST /auth/logout ────────────────────────────────────────────────────────

fn handle_logout(req: &RawRequest, state: &AppState) -> RawResponse {
    let is_api = req.header("authorization")
        .map(|v| v.to_ascii_lowercase().starts_with("bearer "))
        .unwrap_or(false);

    if is_api {
        handle_logout_api(req, state)
    } else {
        handle_logout_browser(req, state)
    }
}

fn handle_logout_browser(req: &RawRequest, state: &AppState) -> RawResponse {
    if let Some(token) = extract_cookie(req, "refresh") {
        let token_hash = hash_token(&token);
        if let Ok(db) = state.db.lock() {
            let _ = db.refresh_token_revoke(&token_hash);
        }
    }
    info!("logout (browser)");

    let clear_session = "session=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0";
    let clear_refresh  = "refresh=; HttpOnly; Secure; SameSite=Strict; Path=/auth/refresh; Max-Age=0";

    RawResponse::new(302)
        .header("Location", "/")
        .header("Set-Cookie", clear_session)
        .header("Set-Cookie", clear_refresh)
}

fn handle_logout_api(req: &RawRequest, state: &AppState) -> RawResponse {
    // Extract user_id from bearer token, revoke all their refresh tokens
    if let Some(auth) = req.header("authorization") {
        let token = auth.trim_start_matches("Bearer ").trim_start_matches("bearer ");
        let decode_result = state.keys.read().ok()
            .and_then(|keys| keys.jwt.decode_access(token).ok());
        if let Some(data) = decode_result {
            let user_id = &data.claims.sub;
            if let Ok(db) = state.db.lock() {
                let _ = db.refresh_tokens_revoke_all(user_id);
            }
            info!(user_id = %user_id, "logout (api)");
        }
    }
    RawResponse::new(204)
}

// ─── GET /auth/public-key ─────────────────────────────────────────────────────

fn handle_public_key(state: &AppState) -> RawResponse {
    let pem = match state.keys.read() {
        Ok(keys) => keys.public_key_pem.clone(),
        Err(_) => return internal_error(),
    };
    RawResponse::new(200)
        .content_type("application/x-pem-file")
        .body(pem)
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn issue_tokens(
    state: &AppState,
    user_id: &str,
    username: &str,
    groups: &[String],
    roles: &[String],
) -> anyhow::Result<(String, String)> {
    let now = now_secs();

    let access_claims = AccessClaims {
        iss:      state.issuer.clone(),
        sub:      user_id.to_string(),
        exp:      now + state.access_ttl as i64,
        iat:      now,
        username: username.to_string(),
        groups:   groups.to_vec(),
        roles:    roles.to_vec(),
    };

    let refresh_claims = RefreshClaims {
        iss:        state.issuer.clone(),
        sub:        user_id.to_string(),
        exp:        now + state.refresh_ttl as i64,
        iat:        now,
        username:   username.to_string(),
        token_type: "refresh".to_string(),
    };

    // Read-lock keys — the watcher may replace the engine during key rotation
    let keys = state.keys.read().map_err(|_| anyhow::anyhow!("keys lock poisoned"))?;
    let access_jwt  = keys.jwt.encode_access(&access_claims)?;
    let refresh_jwt = keys.jwt.encode_refresh(&refresh_claims)?;
    Ok((access_jwt, refresh_jwt))
}

/// Extract a named cookie from the Cookie header.
fn extract_cookie<'a>(req: &'a RawRequest, name: &str) -> Option<String> {
    let cookie_header = req.header("cookie")?;
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(pos) = part.find('=') {
            let k = part[..pos].trim();
            let v = part[pos + 1..].trim();
            if k == name {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn internal_error() -> RawResponse {
    RawResponse::new(500)
        .content_type("application/json")
        .body(r#"{"error":"internal_error"}"#)
}
