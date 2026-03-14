/// JWT authentication: local verification, no network calls.
use base64::Engine;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

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
            return Ok(PublicKey { decoding_key: key, algorithm: Algorithm::RS256 });
        }
        if let Ok(key) = DecodingKey::from_ec_pem(&pem) {
            return Ok(PublicKey { decoding_key: key, algorithm: Algorithm::ES256 });
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

        let data = decode::<Claims>(token, &self.decoding_key, &validation)
            .map_err(AuthError::Invalid)?;

        Ok(data.claims)
    }
}

/// Check if claims satisfy the `require` string.
/// Format: "group:<name>" or "role:<name>"
pub fn check_require(claims: &Claims, require: &str) -> bool {
    if let Some(group) = require.strip_prefix("group:") {
        if let Some(ref groups) = claims.groups {
            return groups.iter().any(|g| g == group);
        }
        return false;
    }
    if let Some(role) = require.strip_prefix("role:") {
        if let Some(ref roles) = claims.roles {
            return roles.iter().any(|r| r == role);
        }
        return false;
    }
    // Unknown require format — deny
    false
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

/// Encode claims as base64 JSON for X-Auth-Claims header.
pub fn encode_claims_header(claims: &Claims) -> String {
    let json = serde_json::to_string(claims).unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}

/// Check if Accept header indicates a browser (text/html).
pub fn is_browser_request(accept: Option<&str>) -> bool {
    accept.map(|a| a.contains("text/html")).unwrap_or(false)
}

/// Extract the `refresh` cookie value if present.
pub fn extract_refresh_cookie<'a>(cookie_header: Option<&'a str>) -> Option<&'a str> {
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
    use super::*;

    #[test]
    fn test_extract_token_from_bearer() {
        let token = extract_token(Some("Bearer mytoken123"), None);
        assert_eq!(token, Some("mytoken123"));
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
        let decoded = base64::engine::general_purpose::STANDARD.decode(&encoded).unwrap();
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
}
