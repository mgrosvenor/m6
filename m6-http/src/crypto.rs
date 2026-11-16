//! The TLS crypto provider for the TCP listener and for our own clients.
//!
//! One choice, made in one place. The default is `ring`. Building with the
//! `aws-lc-rs` cargo feature switches the server's provider, its session
//! ticketer, and the probes and benches to `aws-lc-rs`, so the two can be
//! compared on the same code. QUIC (h3) is not affected: quiche carries its own
//! TLS library (BoringSSL) and never goes through rustls.

use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::server::ProducesTickets;

/// Name of the provider this build uses, for reports.
pub const NAME: &str = if cfg!(feature = "aws-lc-rs") {
    "aws-lc-rs"
} else {
    "ring"
};

/// The provider this build uses.
pub fn provider() -> CryptoProvider {
    #[cfg(feature = "aws-lc-rs")]
    {
        rustls::crypto::aws_lc_rs::default_provider()
    }
    #[cfg(not(feature = "aws-lc-rs"))]
    {
        rustls::crypto::ring::default_provider()
    }
}

/// Install this build's provider as the process default. A second call, or a
/// provider installed earlier, is left alone.
pub fn install() {
    let _ = provider().install_default();
}

/// A session ticketer from this build's provider.
pub fn ticketer() -> Result<Arc<dyn ProducesTickets>, rustls::Error> {
    #[cfg(feature = "aws-lc-rs")]
    {
        rustls::crypto::aws_lc_rs::Ticketer::new()
    }
    #[cfg(not(feature = "aws-lc-rs"))]
    {
        rustls::crypto::ring::Ticketer::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one observable difference between the two providers' defaults: only
    /// aws-lc-rs carries the X25519MLKEM768 hybrid. If the cargo feature did not
    /// switch `provider()`, this fails in whichever build the feature is on.
    #[test]
    fn the_feature_selects_the_provider() {
        let has_hybrid = provider()
            .kx_groups
            .iter()
            .any(|g| format!("{:?}", g.name()) == "X25519MLKEM768");
        assert_eq!(has_hybrid, cfg!(feature = "aws-lc-rs"));
        assert_eq!(NAME == "aws-lc-rs", cfg!(feature = "aws-lc-rs"));
    }

    /// The ticketer comes from the same provider and issues tickets.
    #[test]
    fn the_ticketer_is_usable() {
        let t = ticketer().expect("ticketer");
        assert!(t.enabled());
        let sealed = t.encrypt(b"state").expect("encrypt");
        assert_eq!(t.decrypt(&sealed).as_deref(), Some(&b"state"[..]));
    }
}
