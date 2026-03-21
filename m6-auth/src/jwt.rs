use anyhow::{anyhow, Result};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, TokenData, Validation};
use serde::{Deserialize, Serialize};

/// Claims for an access token (and API tokens).
#[derive(Debug, Serialize, Deserialize)]
pub struct AccessClaims {
    pub iss:      String,
    pub sub:      String,       // user_id
    pub exp:      i64,
    pub iat:      i64,
    pub username: String,
    pub groups:   Vec<String>,
    pub roles:    Vec<String>,
}

/// Claims for a refresh token.
#[derive(Debug, Serialize, Deserialize)]
pub struct RefreshClaims {
    pub iss:      String,
    pub sub:      String,       // user_id
    pub exp:      i64,
    pub iat:      i64,
    pub username: String,
    #[serde(rename = "type")]
    pub token_type: String,    // "refresh"
}

/// The algorithm in use — determined from the key type at construction.
#[derive(Clone, Copy, Debug)]
pub enum KeyAlgo {
    Rs256,
    Es256,
}

/// Holds encoding/decoding keys and the algorithm.
pub struct JwtEngine {
    encoding:  EncodingKey,
    decoding:  DecodingKey,
    algorithm: KeyAlgo,
    pub issuer: String,
}

impl JwtEngine {
    /// Create a new JwtEngine, detecting algorithm from the PEM content.
    ///
    /// Supports PKCS8 EC keys ("PRIVATE KEY" tag, as produced by
    /// `openssl genpkey -algorithm EC`) and RSA keys.
    pub fn new(private_pem: &str, public_pem: &str, issuer: String) -> Result<Self> {
        let (encoding, algorithm) =
            if let Ok(k) = EncodingKey::from_ec_pem(private_pem.as_bytes()) {
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

        Ok(JwtEngine { encoding, decoding, algorithm, issuer })
    }

    fn algo(&self) -> Algorithm {
        match self.algorithm {
            KeyAlgo::Es256 => Algorithm::ES256,
            KeyAlgo::Rs256 => Algorithm::RS256,
        }
    }

    fn header(&self) -> Header {
        Header::new(self.algo())
    }

    pub fn encode_access(&self, claims: &AccessClaims) -> Result<String> {
        jsonwebtoken::encode(&self.header(), claims, &self.encoding)
            .map_err(|e| anyhow!("JWT encode error: {}", e))
    }

    pub fn encode_refresh(&self, claims: &RefreshClaims) -> Result<String> {
        jsonwebtoken::encode(&self.header(), claims, &self.encoding)
            .map_err(|e| anyhow!("JWT encode error: {}", e))
    }

    pub fn decode_access(&self, token: &str) -> Result<TokenData<AccessClaims>> {
        let mut validation = Validation::new(self.algo());
        validation.set_issuer(&[&self.issuer]);
        jsonwebtoken::decode::<AccessClaims>(token, &self.decoding, &validation)
            .map_err(|e| anyhow!("JWT decode error: {}", e))
    }

    pub fn decode_refresh(&self, token: &str) -> Result<TokenData<RefreshClaims>> {
        let mut validation = Validation::new(self.algo());
        validation.set_issuer(&[&self.issuer]);
        jsonwebtoken::decode::<RefreshClaims>(token, &self.decoding, &validation)
            .map_err(|e| anyhow!("JWT decode error: {}", e))
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
