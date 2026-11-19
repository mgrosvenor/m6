use anyhow::{anyhow, Result};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, TokenData, Validation};
use serde::{Deserialize, Serialize};

/// The kind of token, which the header's `typ` names.
///
/// Every decoder requires the value, and the three kinds have three values, so a
/// refresh token presented at the check route is refused before its claims are
/// read. RFC 9068 gives the access token `at+jwt`. The other two are m6's own
/// names, and a value outside these three, including the library's default
/// `JWT`, is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// A session access token: `at+jwt`.
    Access,
    /// An API token: `m6-api+jwt`.
    Api,
    /// A refresh token: `m6-refresh+jwt`.
    Refresh,
}

impl TokenKind {
    /// The `typ` header this kind carries.
    pub fn typ(self) -> &'static str {
        match self {
            TokenKind::Access => "at+jwt",
            TokenKind::Api => "m6-api+jwt",
            TokenKind::Refresh => "m6-refresh+jwt",
        }
    }

    fn from_typ(typ: Option<&str>) -> Option<Self> {
        match typ {
            Some("at+jwt") => Some(TokenKind::Access),
            Some("m6-api+jwt") => Some(TokenKind::Api),
            Some("m6-refresh+jwt") => Some(TokenKind::Refresh),
            _ => None,
        }
    }
}

/// Claims for an access token, and for an API token.
///
/// The two kinds share the shape and are told apart by the header `typ`.
///
/// `jti` is a fresh identifier for each minted token. Nothing reads it back at
/// this tip, so it names no row: the row that can end a token is part of the
/// shelved token-rules work. `sid` is absent for the same reason, because it
/// names a session row this schema does not have.
#[derive(Debug, Serialize, Deserialize)]
pub struct AccessClaims {
    pub iss: String,
    /// The deployment this token was minted for, which the verifier requires.
    pub aud: String,
    pub sub: String, // user_id
    pub exp: i64,
    pub iat: i64,
    /// This token's unique id, fresh at every mint.
    pub jti: String,
    pub username: String,
    pub groups: Vec<String>,
    pub roles: Vec<String>,
}

/// Claims for a refresh token.
///
/// The refresh route identifies the token by its stored SHA-256 hash and reads
/// the row, so it never decodes these claims. They carry the issuer, the
/// audience and a unique id for every other reader, and the token is refused at
/// the check route by its `typ`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RefreshClaims {
    pub iss: String,
    pub aud: String,
    pub sub: String, // user_id
    pub exp: i64,
    pub iat: i64,
    /// This token's unique id, fresh at every mint.
    pub jti: String,
    pub username: String,
    #[serde(rename = "type")]
    pub token_type: String, // "refresh"
}

/// The algorithm in use, determined from the key type at construction.
#[derive(Clone, Copy, Debug)]
pub enum KeyAlgo {
    Rs256,
    Es256,
}

/// Holds encoding/decoding keys, the algorithm, the issuer and the audience.
pub struct JwtEngine {
    /// `None` on a verify-only engine: the process holds the public key and
    /// never signs.
    encoding: Option<EncodingKey>,
    decoding: DecodingKey,
    algorithm: KeyAlgo,
    pub issuer: String,
    pub audience: String,
}

impl JwtEngine {
    /// Create a new JwtEngine, detecting algorithm from the PEM content.
    ///
    /// Supports PKCS8 EC keys ("PRIVATE KEY" tag, as produced by
    /// `openssl genpkey -algorithm EC`) and RSA keys.
    pub fn new(
        private_pem: &str,
        public_pem: &str,
        issuer: String,
        audience: String,
    ) -> Result<Self> {
        let (encoding, algorithm) = if let Ok(k) = EncodingKey::from_ec_pem(private_pem.as_bytes())
        {
            (k, KeyAlgo::Es256)
        } else {
            let k = EncodingKey::from_rsa_pem(private_pem.as_bytes())
                .map_err(|e| anyhow!("invalid private key (tried EC and RSA): {}", e))?;
            (k, KeyAlgo::Rs256)
        };

        let decoding = match algorithm {
            KeyAlgo::Es256 => DecodingKey::from_ec_pem(public_pem.as_bytes())
                .map_err(|e| anyhow!("invalid EC public key: {}", e))?,
            KeyAlgo::Rs256 => DecodingKey::from_rsa_pem(public_pem.as_bytes())
                .map_err(|e| anyhow!("invalid RSA public key: {}", e))?,
        };

        Ok(JwtEngine {
            encoding: Some(encoding),
            decoding,
            algorithm,
            issuer,
            audience,
        })
    }

    /// Create a verify-only engine from the public key, with no signing key.
    ///
    /// `/check` decodes a token and never signs one, so a process that serves
    /// only that route needs the public key and not the private key. The
    /// algorithm comes from the public key alone: EC is tried first, as the
    /// signing constructor tries EC for the private key, so one key pair gives
    /// one algorithm either way.
    pub fn verify_only(public_pem: &str, issuer: String, audience: String) -> Result<Self> {
        let (decoding, algorithm) = if let Ok(k) = DecodingKey::from_ec_pem(public_pem.as_bytes()) {
            (k, KeyAlgo::Es256)
        } else {
            let k = DecodingKey::from_rsa_pem(public_pem.as_bytes())
                .map_err(|e| anyhow!("invalid public key (tried EC and RSA): {}", e))?;
            (k, KeyAlgo::Rs256)
        };
        Ok(JwtEngine {
            encoding: None,
            decoding,
            algorithm,
            issuer,
            audience,
        })
    }

    fn algo(&self) -> Algorithm {
        match self.algorithm {
            KeyAlgo::Es256 => Algorithm::ES256,
            KeyAlgo::Rs256 => Algorithm::RS256,
        }
    }

    fn header(&self, kind: TokenKind) -> Header {
        let mut header = Header::new(self.algo());
        // The library's `Header::new` writes `typ: JWT`, which is the value this
        // engine refuses. Every token names its own kind.
        header.typ = Some(kind.typ().to_string());
        header
    }

    fn encoding(&self) -> Result<&EncodingKey> {
        self.encoding
            .as_ref()
            .ok_or_else(|| anyhow!("this process has no private signing key"))
    }

    /// The one validation rule, set explicitly.
    ///
    /// `leeway` is 0, so a token is refused at the second its `exp` names rather
    /// than sixty seconds later, which is the library's default. The issuer and
    /// the audience are the deployment's, so a token minted for one deployment is
    /// refused at another even when the two share a key. `iat` and `jti` are
    /// enforced by [`AccessClaims`] and [`RefreshClaims`]: both are non-optional,
    /// so a body without one fails to deserialize before any other check runs.
    fn validation(&self) -> Validation {
        let mut validation = Validation::new(self.algo());
        validation.leeway = 0;
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["iss", "aud", "sub", "exp", "iat", "jti"]);
        validation
    }

    /// Encode a session access token, whose `typ` is `at+jwt`.
    pub fn encode_access(&self, claims: &AccessClaims) -> Result<String> {
        jsonwebtoken::encode(&self.header(TokenKind::Access), claims, self.encoding()?)
            .map_err(|e| anyhow!("JWT encode error: {}", e))
    }

    /// Encode an API token, whose `typ` is `m6-api+jwt`.
    pub fn encode_api(&self, claims: &AccessClaims) -> Result<String> {
        jsonwebtoken::encode(&self.header(TokenKind::Api), claims, self.encoding()?)
            .map_err(|e| anyhow!("JWT encode error: {}", e))
    }

    /// Encode a refresh token, whose `typ` is `m6-refresh+jwt`.
    pub fn encode_refresh(&self, claims: &RefreshClaims) -> Result<String> {
        jsonwebtoken::encode(&self.header(TokenKind::Refresh), claims, self.encoding()?)
            .map_err(|e| anyhow!("JWT encode error: {}", e))
    }

    /// Verify an access or API token and say which of the two it is.
    ///
    /// The signature, the algorithm from the key, the issuer, the audience and
    /// the expiry are checked by the engine, and the `typ` is checked here
    /// because the library does not. A refresh token, a token with no `typ` and
    /// a token with `typ: JWT` are all `Err`, so a caller cannot read the claims
    /// of a token of the wrong kind.
    pub fn decode_access(&self, token: &str) -> Result<(TokenKind, TokenData<AccessClaims>)> {
        let data = jsonwebtoken::decode::<AccessClaims>(token, &self.decoding, &self.validation())
            .map_err(|e| anyhow!("JWT decode error: {}", e))?;
        let kind = match TokenKind::from_typ(data.header.typ.as_deref()) {
            Some(kind @ (TokenKind::Access | TokenKind::Api)) => kind,
            _ => {
                return Err(anyhow!(
                    "JWT typ {:?} is not at+jwt or m6-api+jwt",
                    data.header.typ
                ))
            }
        };
        Ok((kind, data))
    }
}

/// Hash a JWT string for storage.
pub fn hash_token(token: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Return the current Unix timestamp as i64.
pub fn now_secs() -> i64 {
    m6_core::platform::wall()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUER: &str = "issuer.example.com";
    const AUDIENCE: &str = "audience.example.com";

    /// An engine over a fresh RSA key pair, made with `openssl` the way a
    /// deployment makes one.
    fn engine() -> JwtEngine {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let private = dir.path().join("test.key");
        let public = dir.path().join("test.pub");
        let generated = std::process::Command::new("openssl")
            .args(["genrsa", "-out"])
            .arg(&private)
            .arg("2048")
            .output()
            .expect("run openssl genrsa");
        assert!(generated.status.success(), "openssl genrsa failed");
        let exported = std::process::Command::new("openssl")
            .args(["rsa", "-in"])
            .arg(&private)
            .args(["-pubout", "-out"])
            .arg(&public)
            .output()
            .expect("run openssl rsa -pubout");
        assert!(exported.status.success(), "openssl rsa -pubout failed");
        JwtEngine::new(
            &std::fs::read_to_string(&private).expect("private key"),
            &std::fs::read_to_string(&public).expect("public key"),
            ISSUER.to_string(),
            AUDIENCE.to_string(),
        )
        .expect("build the engine")
    }

    fn access_claims() -> AccessClaims {
        let now = now_secs();
        AccessClaims {
            iss: ISSUER.to_string(),
            aud: AUDIENCE.to_string(),
            sub: "user-1".to_string(),
            exp: now + 900,
            iat: now,
            jti: crate::new_token_id(),
            username: "alice".to_string(),
            groups: Vec::new(),
            roles: vec!["user".to_string()],
        }
    }

    fn refresh_claims() -> RefreshClaims {
        let now = now_secs();
        RefreshClaims {
            iss: ISSUER.to_string(),
            aud: AUDIENCE.to_string(),
            sub: "user-1".to_string(),
            exp: now + 900,
            iat: now,
            jti: crate::new_token_id(),
            username: "alice".to_string(),
            token_type: "refresh".to_string(),
        }
    }

    /// A token whose header the caller chooses, signed with the engine's key.
    fn signed_with_typ(engine: &JwtEngine, typ: Option<&str>, claims: &AccessClaims) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.typ = typ.map(str::to_string);
        jsonwebtoken::encode(&header, claims, engine.encoding().expect("a signing key"))
            .expect("encode")
    }

    #[test]
    fn each_kind_carries_its_typ_in_the_header() {
        let engine = engine();
        let access = engine.encode_access(&access_claims()).expect("access");
        let api = engine.encode_api(&access_claims()).expect("api");
        let refresh = engine.encode_refresh(&refresh_claims()).expect("refresh");

        for (token, wanted) in [
            (&access, "at+jwt"),
            (&api, "m6-api+jwt"),
            (&refresh, "m6-refresh+jwt"),
        ] {
            let header = jsonwebtoken::decode_header(token).expect("a header");
            assert_eq!(header.typ.as_deref(), Some(wanted), "wrong typ header");
        }
    }

    /// An access token and an API token are the shapes the check route accepts,
    /// and it says which one arrived.
    #[test]
    fn the_check_route_tells_the_two_accepted_kinds_apart() {
        let engine = engine();
        let access = engine.encode_access(&access_claims()).expect("access");
        let api = engine.encode_api(&access_claims()).expect("api");
        assert_eq!(
            engine.decode_access(&access).expect("access decodes").0,
            TokenKind::Access
        );
        assert_eq!(
            engine.decode_access(&api).expect("api decodes").0,
            TokenKind::Api
        );
    }

    /// The library's `Header::new` writes `typ: JWT`, and that value is refused.
    #[test]
    fn a_token_with_typ_jwt_is_refused() {
        let engine = engine();
        let token = signed_with_typ(&engine, Some("JWT"), &access_claims());
        assert!(
            engine.decode_access(&token).is_err(),
            "a token with typ JWT was accepted"
        );
    }

    /// A token with no `typ` at all is refused, so a token from a signer that
    /// omits the header cannot be presented as an access token.
    #[test]
    fn a_token_without_typ_is_refused() {
        let engine = engine();
        let token = signed_with_typ(&engine, None, &access_claims());
        assert!(
            engine.decode_access(&token).is_err(),
            "a token with no typ was accepted"
        );
    }

    /// A refresh token is refused at the access validator, before its claims are
    /// read.
    #[test]
    fn a_refresh_token_is_not_an_access_token() {
        let engine = engine();
        let refresh = engine.encode_refresh(&refresh_claims()).expect("refresh");
        assert!(
            engine.decode_access(&refresh).is_err(),
            "a refresh token was accepted as an access token"
        );
    }

    #[test]
    fn a_token_for_another_audience_is_refused() {
        let engine = engine();
        let mut claims = access_claims();
        claims.aud = "other.example.com".to_string();
        let token = engine.encode_access(&claims).expect("encode");
        assert!(
            engine.decode_access(&token).is_err(),
            "a token for another audience was accepted"
        );
    }

    #[test]
    fn a_token_from_another_issuer_is_refused() {
        let engine = engine();
        let mut claims = access_claims();
        claims.iss = "other.example.com".to_string();
        let token = engine.encode_access(&claims).expect("encode");
        assert!(
            engine.decode_access(&token).is_err(),
            "a token from another issuer was accepted"
        );
    }

    /// The library's default leeway is sixty seconds. A token thirty seconds
    /// past its expiry is inside that default and outside this engine's zero.
    #[test]
    fn an_expired_token_is_refused_within_the_libraries_leeway() {
        let engine = engine();
        let mut claims = access_claims();
        claims.exp = now_secs() - 30;
        let token = engine.encode_access(&claims).expect("encode");
        assert!(
            engine.decode_access(&token).is_err(),
            "a token thirty seconds past its expiry was accepted, so leeway is not zero"
        );
    }

    /// Two mints of the same claims get two ids, so a `jti` names one mint.
    #[test]
    fn every_mint_carries_a_unique_jti() {
        let engine = engine();
        let first = engine.encode_access(&access_claims()).expect("first");
        let second = engine.encode_access(&access_claims()).expect("second");
        let first = engine.decode_access(&first).expect("first decodes").1;
        let second = engine.decode_access(&second).expect("second decodes").1;
        assert!(!first.claims.jti.is_empty(), "the first token has no jti");
        assert!(!second.claims.jti.is_empty(), "the second token has no jti");
        assert_ne!(
            first.claims.jti, second.claims.jti,
            "two mints carried the same jti"
        );
    }
}
