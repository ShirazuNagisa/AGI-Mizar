//! Pairing codes.
//!
//! A code is eight characters of Crockford base32, shown as `K7QP-3M2X`. That
//! alphabet has no `I`, `L`, `O` or `U`: the first three are the ones a person
//! mistypes when reading a code off a screen, and `U` is left out so a random
//! code cannot spell something unfortunate. It is also case-insensitive, and the
//! normaliser maps the confusable letters onto the digits they look like.
//!
//! ### What this module does not do
//!
//! It does not store codes, count attempts or expire anything — those are
//! properties of a *session*, not of a string, and they live in
//! [`crate::server`] where the clock and the lock are.

/// Crockford's base32 alphabet, in order.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// How many characters a code carries.
const LENGTH: usize = 8;

/// How long a code is valid.
pub const TTL_SECONDS: i64 = 300;

/// A freshly generated code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
    /// What the operator reads and types, with the dash.
    pub display: String,
    /// What comparison uses: eight characters, no dash.
    pub canonical: String,
    /// Epoch milliseconds after which the code is dead.
    pub expires_at: i64,
}

/// Generate a code from 40 bits of randomness.
pub fn generate(now_ms: i64) -> PairingCode {
    let bytes = uuid::Uuid::new_v4();
    let raw = bytes.as_bytes();
    // 8 characters × 5 bits = 40 bits, taken from the first five bytes.
    let mut bits: u64 = 0;
    for byte in &raw[..5] {
        bits = (bits << 8) | u64::from(*byte);
    }

    let mut canonical = String::with_capacity(LENGTH);
    for index in 0..LENGTH {
        let shift = 5 * (LENGTH - 1 - index);
        let value = ((bits >> shift) & 0b1_1111) as usize;
        canonical.push(ALPHABET[value] as char);
    }

    let display = format!("{}-{}", &canonical[..4], &canonical[4..]);
    PairingCode {
        display,
        canonical,
        expires_at: now_ms + TTL_SECONDS * 1_000,
    }
}

/// Normalise what a person typed into the canonical form.
///
/// Accepts dashes and spaces anywhere, any case, and the three confusables
/// Crockford defines. Anything outside the alphabet is `None` — a code with a
/// stray character is a typo, not a code that might work.
pub fn normalise(input: &str) -> Option<String> {
    let mut out = String::with_capacity(LENGTH);
    for character in input.chars() {
        let upper = character.to_ascii_uppercase();
        match upper {
            '-' | ' ' | '\t' => continue,
            'O' => out.push('0'),
            'I' | 'L' => out.push('1'),
            _ => {
                if !ALPHABET.contains(&(upper as u8)) {
                    return None;
                }
                out.push(upper);
            }
        }
    }
    (out.len() == LENGTH).then_some(out)
}

/// Compare two codes without an early exit.
pub fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Epoch milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_eight_crockford_characters() {
        let code = generate(0);
        assert_eq!(code.canonical.len(), 8);
        assert_eq!(code.display.len(), 9);
        assert_eq!(code.display.chars().nth(4), Some('-'));
        for character in code.canonical.chars() {
            assert!(
                ALPHABET.contains(&(character as u8)),
                "{character} is not in the alphabet"
            );
        }
    }

    #[test]
    fn the_alphabet_has_no_confusables() {
        for banned in ['I', 'L', 'O', 'U'] {
            assert!(!ALPHABET.contains(&(banned as u8)), "{banned} must not appear");
        }
    }

    #[test]
    fn codes_differ() {
        let a = generate(0);
        let b = generate(0);
        assert_ne!(a.canonical, b.canonical);
    }

    #[test]
    fn normalises_case_separators_and_confusables() {
        assert_eq!(normalise("k7qp-3m2x").as_deref(), Some("K7QP3M2X"));
        assert_eq!(normalise("K7QP 3M2X").as_deref(), Some("K7QP3M2X"));
        // O looks like zero, I and L look like one.
        assert_eq!(normalise("OIL0-1234").as_deref(), Some("01101234"));
    }

    #[test]
    fn refuses_anything_outside_the_alphabet_or_the_wrong_length() {
        assert!(normalise("K7QP-3M2").is_none(), "too short");
        assert!(normalise("K7QP-3M2XX").is_none(), "too long");
        assert!(normalise("K7QP-3M2!").is_none(), "not in the alphabet");
        assert!(normalise("").is_none());
    }

    #[test]
    fn comparison_is_by_value() {
        assert!(constant_time_eq("K7QP3M2X", "K7QP3M2X"));
        assert!(!constant_time_eq("K7QP3M2X", "K7QP3M2Y"));
        assert!(!constant_time_eq("K7QP3M2X", "K7QP3M2"));
    }

    #[test]
    fn a_code_expires_five_minutes_after_it_was_made() {
        let code = generate(1_000_000);
        assert_eq!(code.expires_at, 1_000_000 + 300_000);
    }
}
