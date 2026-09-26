const URL_SAFE_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub(super) fn encode_url_safe(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().enumerate().fold(0u32, |acc, (index, &byte)| {
            acc | (u32::from(byte) << (16 - 8 * index))
        });
        for index in 0..=chunk.len() {
            let sextet = (group >> (18 - 6 * index)) & 0x3f;
            out.push(char::from(URL_SAFE_ALPHABET[sextet as usize]));
        }
    }
    out
}

pub(super) fn decode_url_safe(text: &str) -> Option<Vec<u8>> {
    let digits = text.trim_end_matches('=').as_bytes();
    if digits.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(digits.len() * 3 / 4);
    for chunk in digits.chunks(4) {
        let mut group = 0u32;
        for (index, &digit) in chunk.iter().enumerate() {
            group |= u32::from(sextet(digit)?) << (18 - 6 * index);
        }
        let produced = chunk.len() - 1;
        for index in 0..produced {
            out.push((group >> (16 - 8 * index)) as u8);
        }
        let unused_bits = group & ((1 << (24 - 8 * produced)) - 1);
        if unused_bits != 0 {
            return None;
        }
    }
    Some(out)
}

fn sextet(digit: u8) -> Option<u8> {
    match digit {
        b'A'..=b'Z' => Some(digit - b'A'),
        b'a'..=b'z' => Some(digit - b'a' + 26),
        b'0'..=b'9' => Some(digit - b'0' + 52),
        b'-' | b'+' => Some(62),
        b'_' | b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors_round_trip() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode_url_safe(plain.as_bytes()), encoded);
            assert_eq!(decode_url_safe(encoded).as_deref(), Some(plain.as_bytes()));
        }
        assert_eq!(decode_url_safe("Zm9vYg==").as_deref(), Some(&b"foob"[..]));
    }

    #[test]
    fn url_safe_and_standard_digits_both_decode() {
        let bytes = [0xfb, 0xff, 0xbf];
        assert_eq!(encode_url_safe(&bytes), "-_-_");
        assert_eq!(decode_url_safe("-_-_").as_deref(), Some(&bytes[..]));
        assert_eq!(decode_url_safe("+/+/").as_deref(), Some(&bytes[..]));
    }

    #[test]
    fn malformed_text_is_rejected() {
        assert_eq!(decode_url_safe("Z"), None);
        assert_eq!(decode_url_safe("Zm9v!"), None);
        assert_eq!(decode_url_safe("Zh"), None);
    }
}
