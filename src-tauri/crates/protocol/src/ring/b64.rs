//! Strict base64url without padding (RFC 4648 §5), the only binary-to-text encoding on the
//! Ring's wire. Decoding refuses padding, the standard alphabet, whitespace and non-canonical
//! trailing bits, so every byte string has exactly one accepted spelling.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum B64Error {
    /// Not canonical unpadded base64url.
    Invalid,
    /// Decoded to the wrong number of bytes.
    Length { expected: usize, got: usize },
}

impl fmt::Display for B64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            B64Error::Invalid => f.write_str("not canonical unpadded base64url"),
            B64Error::Length { expected, got } => {
                write!(f, "expected {expected} bytes, got {got}")
            }
        }
    }
}

impl std::error::Error for B64Error {}

pub fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The decoded length of a canonical encoding of `encoded_len` characters, or `None` when no
/// canonical encoding has that length. Lets a receiver bound a payload before decoding it.
pub fn decoded_len(encoded_len: usize) -> Option<usize> {
    match encoded_len % 4 {
        1 => None,
        r => Some(encoded_len / 4 * 3 + r.saturating_sub(1)),
    }
}

pub fn decode(s: &str) -> Result<Vec<u8>, B64Error> {
    // The engine already refuses padding and non-canonical trailing bits; the alphabet check
    // is explicit so the rule does not hang on an engine default.
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(B64Error::Invalid);
    }
    URL_SAFE_NO_PAD.decode(s).map_err(|_| B64Error::Invalid)
}

pub fn decode_array<const N: usize>(s: &str) -> Result<[u8; N], B64Error> {
    // Refuse from the length before decoding anything.
    if decoded_len(s.len()) != Some(N) {
        return Err(B64Error::Length {
            expected: N,
            got: decoded_len(s.len()).unwrap_or(0),
        });
    }
    let v = decode(s)?;
    v.try_into().map_err(|v: Vec<u8>| B64Error::Length {
        expected: N,
        got: v.len(),
    })
}

/// Decodes `s` straight into `out`, which must be exactly its decoded length; no other copy
/// of the bytes is made (for secrets kept in zeroizing storage). On error `out` is zeroed.
pub fn decode_into<const N: usize>(s: &str, out: &mut [u8; N]) -> Result<(), B64Error> {
    if decoded_len(s.len()) != Some(N) {
        return Err(B64Error::Length {
            expected: N,
            got: decoded_len(s.len()).unwrap_or(0),
        });
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(B64Error::Invalid);
    }
    match URL_SAFE_NO_PAD.decode_slice(s, &mut out[..]) {
        Ok(n) if n == N => Ok(()),
        _ => {
            out.fill(0);
            Err(B64Error::Invalid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_into_matches_decode_array() {
        let bytes: [u8; 32] = core::array::from_fn(|i| (i as u8).wrapping_mul(29));
        let s = encode(&bytes);
        let mut out = [0u8; 32];
        decode_into(&s, &mut out).unwrap();
        assert_eq!(out, bytes);
        assert!(decode_into(&s[..42], &mut out).is_err());
        let mut bad = s.clone();
        bad.replace_range(0..1, "+");
        assert!(decode_into(&bad, &mut out).is_err());
        // Non-canonical trailing bits are refused, as by decode.
        let last = s.chars().last().unwrap();
        let mut tweaked = s[..42].to_string();
        tweaked.push(if last == 'B' { 'C' } else { 'B' });
        assert_eq!(
            decode_into(&tweaked, &mut out).is_ok(),
            decode(&tweaked).is_ok()
        );
    }

    #[test]
    fn round_trips() {
        for n in 0..40 {
            let bytes: Vec<u8> = (0..n as u8).map(|b| b.wrapping_mul(37)).collect();
            let s = encode(&bytes);
            assert_eq!(decode(&s).unwrap(), bytes);
            assert_eq!(decoded_len(s.len()), Some(n));
        }
    }

    #[test]
    fn refuses_non_canonical_spellings() {
        for bad in [
            "AA==",  // padding
            "AA=",   // padding
            "A",     // impossible length
            "AB",    // trailing bits set (canonical is "AA")
            "AAB",   // trailing bits set
            "+/AA",  // standard alphabet
            "AA AA", // whitespace
            "AA\n",  // newline
            "ÄA",    // non-ASCII
        ] {
            assert!(decode(bad).is_err(), "{bad:?} decoded");
        }
        assert_eq!(decode("AA").unwrap(), vec![0]);
        assert_eq!(decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn decode_array_checks_length() {
        let s = encode(&[7u8; 32]);
        assert_eq!(decode_array::<32>(&s).unwrap(), [7u8; 32]);
        assert!(matches!(
            decode_array::<64>(&s),
            Err(B64Error::Length { expected: 64, .. })
        ));
        assert!(decode_array::<32>(&encode(&[7u8; 31])).is_err());
    }
}
