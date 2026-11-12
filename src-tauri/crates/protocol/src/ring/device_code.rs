//! Frozen version-1, eight-word Computer adoption credential (96 unbiased random bits).
//! The word list is CC-BY-4.0; code is MIT. See `device_code/NOTICE.txt`.
use super::pairing::{PairError, PairSecret};
use std::fmt;
use zeroize::Zeroizing;

pub const DEVICE_CODE_NOTICE: &str = include_str!("device_code/NOTICE.txt");
pub const DEVICE_CODE_WORDS: &str = include_str!("device_code/words-v1.txt");

#[derive(Clone)]
pub struct DeviceCode(Zeroizing<[u8; 12]>);
impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceCode(..)")
    }
}
impl DeviceCode {
    pub fn generate() -> Result<Self, PairError> {
        let mut bytes = Zeroizing::new([0; 12]);
        getrandom::getrandom(&mut *bytes).map_err(|_| PairError::Crypto)?;
        Ok(Self(bytes))
    }
    pub fn from_bytes(bytes: [u8; 12]) -> Self {
        Self(Zeroizing::new(bytes))
    }
    pub fn parse(input: &str) -> Result<Self, PairError> {
        // Bound allocation and never include credentials in an error or Debug output.
        if input.len() > 256 {
            return Err(PairError::Invalid("eight device words required".into()));
        }
        let normalized = Zeroizing::new(input.to_ascii_lowercase());
        let words: Vec<_> = normalized
            .split(|c: char| c == '-' || c.is_ascii_whitespace())
            .filter(|w| !w.is_empty())
            .collect();
        if words.len() != 8 {
            return Err(PairError::Invalid("eight device words required".into()));
        }
        let mut bits = Zeroizing::new(0u128);
        for word in words {
            let index = DEVICE_CODE_WORDS
                .lines()
                .position(|w| w == word)
                .ok_or_else(|| PairError::Invalid("unknown device word".into()))?;
            *bits = (*bits << 12) | index as u128;
        }
        let bytes = Zeroizing::new(bits.to_be_bytes());
        Ok(Self::from_bytes(bytes[4..].try_into().expect("96 bits")))
    }
    /// The caller must keep this wiping display value memory-only.
    pub fn format(&self) -> Zeroizing<String> {
        let mut bytes = Zeroizing::new([0; 16]);
        bytes[4..].copy_from_slice(&*self.0);
        let bits = Zeroizing::new(u128::from_be_bytes(*bytes));
        let mut out = Zeroizing::new(String::with_capacity(79));
        for i in 0..8 {
            if i > 0 {
                out.push('-');
            }
            let index = ((*bits >> (12 * (7 - i))) & 4095) as usize;
            out.push_str(
                DEVICE_CODE_WORDS
                    .lines()
                    .nth(index)
                    .expect("frozen 4096 entries"),
            );
        }
        out
    }
    pub fn secret(&self) -> PairSecret {
        PairSecret::from_bytes(&*self.0)
    }
}
