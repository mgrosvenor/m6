//! The signing key material.
//!
//! Rotation is: write the private key file and the public key file, then touch
//! the service's config. `main::reload_state` reads both files again and swaps
//! the result in. The key files are not watched, so a reader never sees one
//! half of a pair.
//!
//! Rotation semantics (per spec): m6-auth begins signing with the new key
//! immediately. Previously issued access tokens expire naturally. Refresh
//! tokens are verified against the *database* (hash match), not re-validated by
//! JWT signature, so old refresh tokens remain usable until they expire.

use crate::jwt::JwtEngine;

/// The signing key material, replaced as a whole when the config is touched.
/// Wrapped in `Arc<RwLock<>>` so `reload_state` can swap it while request
/// threads hold read locks.
pub struct KeyMaterial {
    pub jwt: JwtEngine,
    pub public_key_pem: String,
}

impl KeyMaterial {
    pub fn load(
        private_key_path: &std::path::Path,
        public_key_path: &std::path::Path,
        issuer: String,
    ) -> anyhow::Result<Self> {
        let private_pem = std::fs::read_to_string(private_key_path).map_err(|e| {
            anyhow::anyhow!(
                "cannot read private key {}: {}",
                private_key_path.display(),
                e
            )
        })?;
        let public_pem = std::fs::read_to_string(public_key_path).map_err(|e| {
            anyhow::anyhow!(
                "cannot read public key {}: {}",
                public_key_path.display(),
                e
            )
        })?;

        let jwt = JwtEngine::new(&private_pem, &public_pem, issuer.clone())?;
        check_pair(&jwt, &issuer)?;
        Ok(KeyMaterial {
            jwt,
            public_key_pem: public_pem,
        })
    }
}

/// Sign a probe token with the private key and verify it with the public key.
///
/// Both files parse on their own, so a rotation caught between its two writes
/// loads a private key beside the previous public key without an error. Every
/// token signed afterwards would then fail to verify. An `Err` here keeps the
/// running keys, and core retries the reload with a growing delay.
fn check_pair(jwt: &JwtEngine, issuer: &str) -> anyhow::Result<()> {
    let now = crate::jwt::now_secs();
    let claims = crate::jwt::AccessClaims {
        iss: issuer.to_string(),
        sub: "key-pair-check".to_string(),
        exp: now + 60,
        iat: now,
        username: String::new(),
        groups: Vec::new(),
        roles: Vec::new(),
    };
    let token = jwt.encode_access(&claims)?;
    jwt.decode_access(&token)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("the private and public key are not a pair: {e}"))
}
