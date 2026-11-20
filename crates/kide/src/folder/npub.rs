//! Nostr public keys as `npub1…` (NIP-19): 32 bytes in bech32 (BIP-173)
//! with the prefix `npub`. Written by hand: only this one use.

use crate::error::{Result, invalid};
use std::fmt;

const CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const HRP: &str = "npub";
/// `npub1`, 52 data characters (256 bits and 4 zero bits), 6 of checksum.
const LEN: usize = HRP.len() + 1 + 52 + 6;

/// A device's x-only public key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub [u8; 32]);

fn polymod(values: impl Iterator<Item = u8>) -> u32 {
    const GEN: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
    values.fold(1, |chk, v| {
        let top = chk >> 25;
        let chk = ((chk & 0x1ff_ffff) << 5) ^ v as u32;
        (0..5)
            .filter(|i| (top >> i) & 1 == 1)
            .fold(chk, |c, i| c ^ GEN[i])
    })
}

fn hrp_expanded() -> impl Iterator<Item = u8> {
    let b = HRP.bytes();
    b.clone()
        .map(|c| c >> 5)
        .chain([0])
        .chain(b.map(|c| c & 31))
}

impl Key {
    pub fn from_npub(s: &str) -> Result<Key> {
        let bad = || invalid(format!("not an npub key: {s:?}"));
        let lower = s.to_ascii_lowercase();
        if s.len() != LEN || (s != lower && s != s.to_ascii_uppercase()) {
            return bad();
        }
        let Some(data) = lower.strip_prefix("npub1") else {
            return bad();
        };
        let Some(fives) = data
            .bytes()
            .map(|c| CHARSET.iter().position(|&x| x == c).map(|p| p as u8))
            .collect::<Option<Vec<u8>>>()
        else {
            return bad();
        };
        if polymod(hrp_expanded().chain(fives.iter().copied())) != 1 {
            return bad();
        }
        let (mut key, mut acc, mut bits, mut n) = ([0u8; 32], 0u32, 0, 0);
        for &v in &fives[..52] {
            (acc, bits) = (((acc << 5) | v as u32) & 0xfff, bits + 5);
            if bits >= 8 {
                bits -= 8;
                key[n] = (acc >> bits) as u8;
                n += 1;
            }
        }
        if acc & ((1 << bits) - 1) != 0 {
            return bad(); // non-zero padding
        }
        Ok(Key(key))
    }

    pub fn to_npub(&self) -> String {
        let mut fives = Vec::with_capacity(58);
        let (mut acc, mut bits) = (0u32, 0);
        for &b in &self.0 {
            (acc, bits) = (((acc << 8) | b as u32) & 0xfff, bits + 8);
            while bits >= 5 {
                bits -= 5;
                fives.push(((acc >> bits) & 31) as u8);
            }
        }
        fives.push(((acc << (5 - bits)) & 31) as u8);
        let pm = polymod(hrp_expanded().chain(fives.iter().copied()).chain([0; 6])) ^ 1;
        fives.extend((0..6).map(|i| ((pm >> (5 * (5 - i))) & 31) as u8));
        let data: String = fives.iter().map(|&v| CHARSET[v as usize] as char).collect();
        format!("{HRP}1{data}")
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_npub())
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({})", self.to_npub())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;

    /// The example of NIP-19.
    const NPUB: &str = "npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg";
    const HEX: &str = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e";

    #[test]
    fn nip19_example() {
        let k = Key::from_npub(NPUB).unwrap();
        assert_eq!(Hash(k.0).to_hex(), HEX);
        assert_eq!(k.to_npub(), NPUB);
        assert_eq!(Key::from_npub(&NPUB.to_uppercase()).unwrap(), k);
    }

    #[test]
    fn round_trip() {
        for i in 0..20u8 {
            let k = Key(crate::hash::h(&[i]).0);
            assert_eq!(Key::from_npub(&k.to_npub()).unwrap(), k);
        }
    }

    #[test]
    fn rejections() {
        let flipped = NPUB.replace("dma8", "dma9");
        let mixed = format!("N{}", &NPUB[1..]);
        let nsec = format!("nsec{}", &NPUB[4..]);
        for bad in [
            &NPUB[..62],
            &flipped,
            &mixed,
            &nsec,
            "npub1",
            "",
            &format!("{NPUB}q"),
        ] {
            assert!(Key::from_npub(bad).is_err(), "{bad:?}");
        }
    }
}
