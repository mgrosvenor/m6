/// JWT authentication: local verification, no network calls.
use base64::Engine;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

/// How long a `require` route waits for `POST /check` before answering 503.
///
/// The measured parts of one check add to about 111 µs, so this fires only on
/// an authority that has stopped answering. A page carrying one protected
/// request stalls at most this long on a refused credential.
pub const CHECK_DEADLINE: std::time::Duration = std::time::Duration::from_millis(50);

/// Reconstruct the logical `Cookie` header from a request's header list.
///
/// HTTP/2 (and HTTP/3) clients are RFC-compliant to send multiple separate
/// `cookie` header *fields* instead of one combined `a=1; b=2` field (RFC
/// 7540 §8.1.2.5 — it's specifically recommended for better HPACK/QPACK
/// compression, since each crumb can then be referenced independently from
/// the dynamic table). A `.find()` for "the cookie header" only ever sees
/// the first such field and silently drops every cookie after it — the
/// exact bug this was: `curl` always sends one pre-joined field so it never
/// tripped this, but a browser splitting cookies across fields could make
/// an auth cookie in a later field invisible to a naive single-field
/// lookup. The spec requires the receiver to join multiple fields with
/// `"; "` before treating it as a single logical header — that's this.
pub fn combined_cookie_header(
    headers: &(impl crate::analytics::HeaderSource + ?Sized),
) -> Option<String> {
    let mut parts = headers.find_all("cookie").peekable();
    parts.peek()?;
    Some(parts.collect::<Vec<_>>().join("; "))
}

/// JWT claims we care about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: Option<String>,
    pub iss: Option<String>,
    pub exp: Option<u64>,
    pub groups: Option<Vec<String>>,
    pub roles: Option<Vec<String>>,
    /// Keep all extra claims for forwarding.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("no token provided")]
    NoToken,
    #[error("invalid token: {0}")]
    Invalid(#[from] jsonwebtoken::errors::Error),
    #[error("token expired")]
    Expired,
    #[error("insufficient claims: required {0}")]
    InsufficientClaims(String),
}

/// Loaded public key for JWT verification.
pub struct PublicKey {
    decoding_key: DecodingKey,
    algorithm: Algorithm,
}

impl PublicKey {
    /// Load from a PEM file. Supports RS256 (RSA) and ES256 (EC).
    pub fn from_pem_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let pem = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("reading public key {}: {}", path.display(), e))?;

        // Try RS256 first, then ES256
        if let Ok(key) = DecodingKey::from_rsa_pem(&pem) {
            return Ok(PublicKey {
                decoding_key: key,
                algorithm: Algorithm::RS256,
            });
        }
        if let Ok(key) = DecodingKey::from_ec_pem(&pem) {
            return Ok(PublicKey {
                decoding_key: key,
                algorithm: Algorithm::ES256,
            });
        }
        anyhow::bail!("public key at {} is neither RSA nor EC PEM", path.display())
    }

    /// Verify a JWT token and return the claims.
    pub fn verify(&self, token: &str) -> Result<Claims, AuthError> {
        let mut validation = Validation::new(self.algorithm);
        // We check exp manually to give a better error
        validation.validate_exp = true;
        // Don't require any audience
        validation.set_required_spec_claims(&["exp"]);

        let data =
            decode::<Claims>(token, &self.decoding_key, &validation).map_err(AuthError::Invalid)?;

        Ok(data.claims)
    }
}

/// A `require` string: the claim list it reads and the name that must be in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Require<'a> {
    Role(&'a str),
    Group(&'a str),
}

impl<'a> Require<'a> {
    /// Parse `"role:<name>"` or `"group:<name>"`. `None` for any other form.
    /// The one definition of the format, read by the request-path check and
    /// by config validation.
    pub fn parse(require: &'a str) -> Option<Self> {
        if let Some(group) = require.strip_prefix("group:") {
            return Some(Self::Group(group));
        }
        require.strip_prefix("role:").map(Self::Role)
    }
}

/// Check if claims satisfy the `require` string.
/// Format: `group:<name>` or `role:<name>`. Any other form denies.
pub fn check_require(claims: &Claims, require: &str) -> bool {
    let (list, name) = match Require::parse(require) {
        Some(Require::Group(g)) => (&claims.groups, g),
        Some(Require::Role(r)) => (&claims.roles, r),
        None => return false,
    };
    list.as_ref().is_some_and(|l| l.iter().any(|v| v == name))
}

/// Extract JWT token from Authorization header or session cookie.
/// Returns None if not found.
pub fn extract_token<'a>(
    auth_header: Option<&'a str>,
    cookie_header: Option<&'a str>,
) -> Option<&'a str> {
    // Authorization: Bearer <token> takes precedence
    if let Some(auth) = auth_header {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    // Fall back to session cookie
    if let Some(cookies) = cookie_header {
        for cookie in cookies.split(';') {
            let cookie = cookie.trim();
            if let Some(val) = cookie.strip_prefix("session=") {
                let val = val.trim();
                if !val.is_empty() {
                    return Some(val);
                }
            }
        }
    }
    None
}

/// What the one token check decided.
#[derive(Debug)]
pub enum Decision {
    /// The credential verified and carries the route's requirement.
    Allow(Claims),
    /// No credential, or one that does not verify. A 401.
    NoCredential,
    /// The credential verified but lacks the required role or group. A 403.
    Deny,
}

impl Decision {
    /// The refusal this decision is, or `None` when it allows.
    ///
    /// Every route that gates on a token answers through this one definition,
    /// so the status, headers and body are identical wherever the refusal
    /// comes from: 401 with `WWW-Authenticate` for no credential or one that
    /// does not verify, 403 for the wrong role or group.
    pub fn refusal(&self) -> Option<m6_core::http::TokenRefusal> {
        match self {
            Decision::Allow(_) => None,
            Decision::NoCredential => Some(m6_core::http::token_refusal(false)),
            Decision::Deny => Some(m6_core::http::token_refusal(true)),
        }
    }
}

/// The one token check. Every token-gated route calls this exactly once.
///
/// The claim matching against a `require` string is here and nowhere else:
/// route `require`, `/perf`, `/traffic` and `/genid` all pass their own
/// `require` to it. A missing public key fails closed as `NoCredential`, so a
/// misconfigured node never serves what it meant to gate. It never redirects:
/// a browser redirect is the route block's choice, made on its own refusal.
pub fn check(key: Option<&PublicKey>, headers: &[(String, String)], require: &str) -> Decision {
    m6_core::log::debug!("auth check");
    let auth = m6_core::headers::get(headers, "authorization");
    let cookies = combined_cookie_header(headers);
    let Some(token) = extract_token(auth, cookies.as_deref()) else {
        return Decision::NoCredential;
    };
    let Some(key) = key else {
        return Decision::NoCredential;
    };
    match key.verify(token) {
        // An invalid, expired or forged token is no credential at all.
        Err(_) => Decision::NoCredential,
        Ok(claims) if check_require(&claims, require) => Decision::Allow(claims),
        Ok(_) => Decision::Deny,
    }
}

/// Encode claims as base64 JSON for X-Auth-Claims header.
pub fn encode_claims_header(claims: &Claims) -> String {
    let json = serde_json::to_string(claims).unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}

/// The bytes of one `POST /check`, written into `buf`.
///
/// Hand-built: a check is one small request on the socket-backend dispatch, so
/// it costs no `HttpRequest` and no allocation once `buf` has capacity. No
/// credential sends no `Authorization`, which is the start probe's request.
pub fn encode_check_request(buf: &mut Vec<u8>, credential: Option<&str>) {
    buf.clear();
    buf.extend_from_slice(b"POST /check HTTP/1.1\r\nHost: localhost\r\n");
    if let Some(credential) = credential {
        buf.extend_from_slice(b"Authorization: Bearer ");
        buf.extend_from_slice(credential.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf.extend_from_slice(b"Content-Length: 0\r\nConnection: close\r\n\r\n");
}

/// Check if Accept header indicates a browser (text/html).
pub fn is_browser_request(accept: Option<&str>) -> bool {
    accept.map(|a| a.contains("text/html")).unwrap_or(false)
}

/// Extract the `refresh` cookie value if present.
pub fn extract_refresh_cookie(cookie_header: Option<&str>) -> Option<&str> {
    let cookies = cookie_header?;
    for cookie in cookies.split(';') {
        let cookie = cookie.trim();
        if let Some(val) = cookie.strip_prefix("refresh=") {
            let val = val.trim();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::test_keys::{bearer, keypair, mint};
    use super::*;

    #[test]
    fn test_extract_token_from_bearer() {
        let token = extract_token(Some("Bearer mytoken123"), None);
        assert_eq!(token, Some("mytoken123"));
    }

    #[test]
    fn test_combined_cookie_header_single_field() {
        let headers = vec![("cookie".to_string(), "a=1; b=2".to_string())];
        assert_eq!(
            combined_cookie_header(&headers),
            Some("a=1; b=2".to_string())
        );
    }

    #[test]
    fn test_combined_cookie_header_split_across_fields() {
        // What HTTP/2 clients are RFC-compliant to send: one cookie per
        // field instead of one joined field. This is the exact shape that
        // broke session-cookie auth — a naive single-field `.find()` only
        // ever saw whichever crumb happened to be in the first field.
        let headers = vec![
            ("cookie".to_string(), "_m6sid=abc".to_string()),
            ("cookie".to_string(), "session=xyz".to_string()),
            ("cookie".to_string(), "refresh=def".to_string()),
        ];
        assert_eq!(
            combined_cookie_header(&headers),
            Some("_m6sid=abc; session=xyz; refresh=def".to_string())
        );
    }

    #[test]
    fn test_combined_cookie_header_absent() {
        let headers = vec![("user-agent".to_string(), "test".to_string())];
        assert_eq!(combined_cookie_header(&headers), None);
    }

    #[test]
    fn test_extract_token_finds_session_when_cookies_split_across_fields() {
        let headers = vec![
            ("cookie".to_string(), "_m6sid=abc".to_string()),
            ("cookie".to_string(), "session=realtoken".to_string()),
        ];
        let combined = combined_cookie_header(&headers);
        let token = extract_token(None, combined.as_deref());
        assert_eq!(token, Some("realtoken"));
    }

    #[test]
    fn test_extract_token_from_cookie() {
        let token = extract_token(None, Some("session=cookietoken; other=val"));
        assert_eq!(token, Some("cookietoken"));
    }

    #[test]
    fn test_bearer_takes_precedence_over_cookie() {
        let token = extract_token(Some("Bearer headertoken"), Some("session=cookietoken"));
        assert_eq!(token, Some("headertoken"));
    }

    #[test]
    fn test_extract_token_none() {
        assert_eq!(extract_token(None, None), None);
        assert_eq!(extract_token(None, Some("other=val")), None);
        assert_eq!(extract_token(Some("Basic abc"), None), None);
    }

    #[test]
    fn test_check_require_group() {
        let claims = Claims {
            sub: None,
            iss: None,
            exp: None,
            groups: Some(vec!["editors".to_string(), "writers".to_string()]),
            roles: None,
            extra: Default::default(),
        };
        assert!(check_require(&claims, "group:editors"));
        assert!(!check_require(&claims, "group:admins"));
    }

    #[test]
    fn test_check_require_role() {
        let claims = Claims {
            sub: None,
            iss: None,
            exp: None,
            groups: None,
            roles: Some(vec!["admin".to_string()]),
            extra: Default::default(),
        };
        assert!(check_require(&claims, "role:admin"));
        assert!(!check_require(&claims, "role:superuser"));
    }

    #[test]
    fn test_is_browser_request() {
        assert!(is_browser_request(Some("text/html,application/xhtml+xml")));
        assert!(!is_browser_request(Some("application/json")));
        assert!(!is_browser_request(None));
    }

    #[test]
    fn test_encode_claims_header() {
        let claims = Claims {
            sub: Some("user1".to_string()),
            iss: None,
            exp: None,
            groups: None,
            roles: None,
            extra: Default::default(),
        };
        let encoded = encode_claims_header(&claims);
        // Decode and verify
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(json["sub"], "user1");
    }

    #[test]
    fn test_extract_refresh_cookie() {
        assert_eq!(
            extract_refresh_cookie(Some("session=abc; refresh=def")),
            Some("def")
        );
        assert_eq!(extract_refresh_cookie(Some("session=abc")), None);
        assert_eq!(extract_refresh_cookie(None), None);
    }

    #[test]
    fn check_without_a_token_is_no_credential() {
        let (_d, key, _) = keypair();
        let d = check(Some(&key), &[], "role:monitor");
        assert!(matches!(d, Decision::NoCredential));
        assert_eq!(d.refusal().unwrap().0, 401);
    }

    #[test]
    fn check_with_another_keys_signature_is_no_credential() {
        let (_d, key, _) = keypair();
        let (_d2, _, other_private) = keypair();
        let t = mint(&other_private, &["monitor"], 3600);
        let d = check(Some(&key), &bearer(&t), "role:monitor");
        assert!(matches!(d, Decision::NoCredential));
        assert_eq!(d.refusal().unwrap().0, 401);
    }

    #[test]
    fn check_an_expired_token_is_no_credential() {
        let (_d, key, private) = keypair();
        // An hour ago, well beyond the verifier's 60 s leeway.
        let t = mint(&private, &["monitor"], -3600);
        let d = check(Some(&key), &bearer(&t), "role:monitor");
        assert!(matches!(d, Decision::NoCredential));
    }

    #[test]
    fn check_the_wrong_role_is_denied_with_403() {
        let (_d, key, private) = keypair();
        let t = mint(&private, &["reader"], 3600);
        let d = check(Some(&key), &bearer(&t), "role:monitor");
        assert!(matches!(d, Decision::Deny));
        assert_eq!(d.refusal().unwrap().0, 403);
    }

    #[test]
    fn check_the_right_role_passes() {
        let (_d, key, private) = keypair();
        let t = mint(&private, &["monitor"], 3600);
        let Decision::Allow(claims) = check(Some(&key), &bearer(&t), "role:monitor") else {
            panic!("a valid token with the role must pass");
        };
        assert_eq!(claims.sub.as_deref(), Some("t"));
    }

    #[test]
    fn check_a_missing_key_fails_closed() {
        let d = check(None, &[], "role:monitor");
        assert!(matches!(d, Decision::NoCredential));
    }

    #[test]
    fn check_reads_the_session_cookie_like_routes() {
        let (_d, key, private) = keypair();
        let t = mint(&private, &["monitor"], 3600);
        let headers = vec![
            ("cookie".to_string(), "_m6sid=abc".to_string()),
            ("cookie".to_string(), format!("session={t}")),
        ];
        assert!(matches!(
            check(Some(&key), &headers, "role:monitor"),
            Decision::Allow(_)
        ));
    }
}

/// ES256 fixtures shared by this crate's unit tests.
#[cfg(test)]
pub(crate) mod test_keys {
    use super::PublicKey;

    /// A verify key on disk, plus the matching signing key.
    pub(crate) fn keypair() -> (tempfile::TempDir, PublicKey, String) {
        let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("ec keypair");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pub.pem");
        std::fs::write(&path, kp.public_key_pem()).unwrap();
        let key = PublicKey::from_pem_file(&path).unwrap();
        (dir, key, kp.serialize_pem())
    }

    /// Mint an ES256 token with `roles`, expiring `exp_offset_s` from now.
    pub(crate) fn mint(private_pem: &str, roles: &[&str], exp_offset_s: i64) -> String {
        #[derive(serde::Serialize)]
        struct C {
            sub: String,
            exp: i64,
            roles: Vec<String>,
        }
        let now = m6_core::platform::wall()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let claims = C {
            sub: "t".to_string(),
            exp: now + exp_offset_s,
            roles: roles.iter().map(|r| r.to_string()).collect(),
        };
        let key = jsonwebtoken::EncodingKey::from_ec_pem(private_pem.as_bytes()).unwrap();
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        jsonwebtoken::encode(&header, &claims, &key).unwrap()
    }

    pub(crate) fn bearer(token: &str) -> Vec<(String, String)> {
        vec![("authorization".to_string(), format!("Bearer {token}"))]
    }
}
