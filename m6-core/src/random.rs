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

/// A cryptographically random token of `N` bytes, hex encoded.
///
/// The returned string is `2 * N` characters.
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
    let mut raw = [0u8; N];
    rand::thread_rng().fill_bytes(&mut raw);

    let mut out = String::with_capacity(N * 2);
    for b in raw {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
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
            t.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
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
}
