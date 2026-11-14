//! Random token generation.
//!
//! One implementation, because the risk here is the third copy. There were two,
//! `generate_session_id` in `m6-http` and `generate_csrf_token` in `m6-render`,
//! byte-for-byte the same logic at two lengths. Both were correct, so nothing
//! was broken; the hazard is that the next person needing a token has two
//! examples to copy and no shared function, and a copy reaching for
//! `rand::random()` or a non-cryptographic generator would look identical at a
//! glance.

use rand::RngCore;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The longest token these helpers make: 32 random bytes, a 64-character hex
/// string.
const MAX_TOKEN_BYTES: usize = 32;

/// A cryptographically random token, hex-encoded into `out` with no heap
/// allocation.
///
/// The zero-allocation form of [`random_hex_token`], for a caller on a request
/// path that keeps `[u8; 32]` on the stack and reads the id back as a `&str`
/// over it. `out` is exactly two hex characters per random byte, so it must be
/// even and at most twice [`MAX_TOKEN_BYTES`]. The returned `&str` borrows
/// `out`.
pub fn random_hex_token_into(out: &mut [u8]) -> &str {
    assert!(
        out.len().is_multiple_of(2),
        "a hex token takes two bytes per random byte"
    );
    assert!(
        out.len() / 2 <= MAX_TOKEN_BYTES,
        "a token is at most {MAX_TOKEN_BYTES} random bytes"
    );
    let n = out.len() / 2;
    let mut raw = [0u8; MAX_TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut raw[..n]);
    for (i, b) in raw[..n].iter().enumerate() {
        out[2 * i] = HEX[(b >> 4) as usize];
        out[2 * i + 1] = HEX[(b & 0x0f) as usize];
    }
    std::str::from_utf8(&out[..2 * n]).expect("hex characters are ASCII")
}

/// A cryptographically random token of `N` bytes, hex encoded.
///
/// The returned string is `2 * N` characters. `N` is at most
/// [`MAX_TOKEN_BYTES`], the longest token any caller here makes.
///
/// **One allocation.** Both versions this replaces built the string with
/// `bytes.iter().map(|b| format!("{:02x}", b)).collect()`, which allocates a
/// `String` per byte and then collects them: 16 allocations for a session id
/// and 32 for a CSRF token, discarded immediately. Here the random bytes live
/// on the stack and the output `String` is allocated once, at its exact final
/// capacity.
///
/// `rand::thread_rng()` is a CSPRNG, seeded from the OS and periodically
/// reseeded. That property is the reason this function exists in one place: a
/// token generator is a security primitive and should not be re-derived from
/// memory.
pub fn random_hex_token<const N: usize>() -> String {
    let mut out = [0u8; 2 * MAX_TOKEN_BYTES];
    random_hex_token_into(&mut out[..2 * N]).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_is_two_hex_characters_per_byte() {
        assert_eq!(random_hex_token::<16>().len(), 32);
        assert_eq!(random_hex_token::<32>().len(), 64);
    }

    #[test]
    fn output_is_lowercase_hex_only() {
        let t = random_hex_token::<32>();
        assert!(
            t.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "non-hex character in {t}"
        );
    }

    /// Not a randomness test, which a unit test cannot do. This catches the
    /// failure that would matter: a generator wired up to return a constant, or
    /// seeded identically per call.
    #[test]
    fn successive_tokens_differ() {
        let a = random_hex_token::<16>();
        let b = random_hex_token::<16>();
        assert_ne!(a, b);
    }

    #[test]
    fn the_output_string_is_allocated_exactly_once() {
        let t = random_hex_token::<32>();
        assert_eq!(t.capacity(), 64, "capacity should be exact, not grown");
    }

    #[test]
    fn the_stack_form_has_the_same_shape() {
        let mut id = [0u8; 32];
        let t = random_hex_token_into(&mut id);
        assert_eq!(t.len(), 32);
        assert!(
            t.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "non-hex character in {t}"
        );
        // Two calls fill different ids: it is the same CSPRNG as the String form.
        let first = t.to_owned();
        let second = random_hex_token_into(&mut id).to_owned();
        assert_ne!(first, second);
    }
}
