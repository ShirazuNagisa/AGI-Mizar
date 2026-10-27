//! Standard base64, because more than one wire needs it.
//!
//! The tunnel and the phone protocol both carry bytes this way, and a second
//! hand-rolled copy is how two encoders end up disagreeing about padding at the
//! one place it matters.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as standard base64, padding included.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        let value = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);
        out.push(ALPHABET[(value >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(value >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(value >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[value as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Decode standard base64. `None` for anything that is not that.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut index = 0;
    while index < bytes.len() {
        let chunk = &bytes[index..index + 4];
        let mut values = [0u32; 4];
        let mut padding = 0;
        for (slot, byte) in chunk.iter().enumerate() {
            values[slot] = match *byte {
                b'A'..=b'Z' => u32::from(byte - b'A'),
                b'a'..=b'z' => u32::from(byte - b'a') + 26,
                b'0'..=b'9' => u32::from(byte - b'0') + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' if slot >= 2 => {
                    padding += 1;
                    0
                }
                _ => return None,
            };
        }
        let value = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
        out.push((value >> 16) as u8);
        if padding < 2 {
            out.push((value >> 8) as u8);
        }
        if padding < 1 {
            out.push(value as u8);
        }
        index += 4;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_length() {
        for text in ["", "a", "ab", "abc", "hello world", "\u{1F600}"] {
            let encoded = encode(text.as_bytes());
            assert_eq!(decode(&encoded).unwrap(), text.as_bytes());
        }
        assert_eq!(encode(b"abc"), "YWJj");
        assert_eq!(encode(b"ab"), "YWI=");
        assert_eq!(decode("YQ==").unwrap(), b"a");
    }

    #[test]
    fn anything_else_is_not_base64() {
        assert!(decode("a").is_none());
        assert!(decode("****").is_none());
    }
}
