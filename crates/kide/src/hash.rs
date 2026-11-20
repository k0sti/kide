//! Object ids (§2.1): `H` is SHA-256; ids are 32 bytes, written as lowercase hex.

use crate::error::{Result, invalid};
use sha2::Digest;
use std::fmt;

/// `H`: SHA-256 of `data` (A1).
pub fn h(data: &[u8]) -> Hash {
    Hash(sha2::Sha256::digest(data).into())
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// Thirty-two zero bytes: the root of a tombstone (§3.1).
    pub const ZERO: Hash = Hash([0; 32]);

    /// The id at the start of `b`, which must hold at least 32 bytes.
    pub(crate) fn at(b: &[u8]) -> Hash {
        let mut id = [0; 32];
        id.copy_from_slice(&b[..32]);
        Hash(id)
    }

    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }

    pub fn from_hex(s: &str) -> Result<Hash> {
        match <[u8; 32]>::try_from(from_hex(s)?) {
            Ok(b) => Ok(Hash(b)),
            Err(_) => invalid(format!("not a 32-byte hex id: {s:?}")),
        }
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

/// Lowercase hex of any byte string.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

/// Decodes lowercase or uppercase hex; `Invalid` on odd length or a bad digit.
pub fn from_hex(s: &str) -> Result<Vec<u8>> {
    fn digit(c: u8) -> Option<u8> {
        (c as char).to_digit(16).map(|d| d as u8)
    }
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return invalid(format!("odd-length hex: {s:?}"));
    }
    b.chunks(2)
        .map(|p| match (digit(p[0]), digit(p[1])) {
            (Some(hi), Some(lo)) => Ok(hi << 4 | lo),
            _ => invalid(format!("bad hex digit in {s:?}")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let h = Hash(std::array::from_fn(|i| (i * 9) as u8));
        let s = h.to_hex();
        assert_eq!(s.len(), 64);
        assert!(s.starts_with("0009121b"));
        assert_eq!(Hash::from_hex(&s).unwrap(), h);
        assert_eq!(Hash::from_hex(&s.to_uppercase()).unwrap(), h);
        assert_eq!(format!("{h}"), s);
    }

    #[test]
    fn h_is_sha256() {
        assert_eq!(
            h(b"").to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn from_hex_rejects_bad_input() {
        assert!(Hash::from_hex("00").is_err());
        assert!(Hash::from_hex(&"0".repeat(63)).is_err());
        assert!(Hash::from_hex(&format!("{}g", "0".repeat(63))).is_err());
        assert!(from_hex("abc").is_err());
        assert_eq!(from_hex("").unwrap(), Vec::<u8>::new());
        assert_eq!(to_hex(&[0xde, 0xad, 0x01]), "dead01");
    }
}
