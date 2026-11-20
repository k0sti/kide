//! The format (§2, §3.1, §5.1 to §5.3): parameters, gear table, heights and
//! the objects built from them. Concrete; everything else builds on it.

pub mod change;
pub mod chunk;
pub mod content;
pub mod manifest;
pub mod node;
pub mod path;
pub mod record;

use crate::error::Result;
use crate::hash::{Hash, h};
use std::sync::OnceLock;

/// The folder format (§5.4): the objects and the folder profile's rules.
/// Every folder records it, and Hello says it (§4.2).
pub const FORMAT: u16 = 8;

/// The single tunable (§2.1, A6).
pub const K: u32 = 10;
/// Target size of chunks and nodes: 1,024.
pub const S: usize = 1 << K;
/// Smallest chunk, except a final one: 256.
pub const MIN: usize = S / 4;
/// Largest chunk (manifest chunks: §5.1): 8,192.
pub const MAX: usize = 8 * S;
/// Bits of height per level: 5.
pub const B: u32 = K - 5;
/// Largest number of children of a node, and of parents of a record: 32.
pub const W: usize = 1 << B;

/// The parameters of §2.1 as a value, for measuring others (§9). The format
/// uses `Params::SPEC`; every other value builds objects that a device
/// rejects or never produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub k: u32,
    pub min: usize,
    pub max: usize,
    pub b: u32,
    pub w: usize,
}

impl Params {
    /// The spec's parameters: the constants above.
    pub const SPEC: Params = Params::of_k(K);

    /// The parameters derived from `k` as in §2.1 (k ≥ 6).
    pub const fn of_k(k: u32) -> Params {
        let s = 1 << k;
        Params {
            k,
            min: s / 4,
            max: 8 * s,
            b: k - 5,
            w: 1 << (k - 5),
        }
    }

    /// Whether the window hash `h` allows a natural cut: its top k bits are zero.
    pub fn natural(&self, h: u64) -> bool {
        h >> (64 - self.k) == 0
    }

    /// Leading zero bits of `id`, big-endian, divided by `b` and rounded down.
    pub fn height(&self, id: &Hash) -> u32 {
        let zeros =
            id.0.iter()
                .position(|&b| b != 0)
                .map_or(256, |i| i as u32 * 8 + id.0[i].leading_zeros());
        zeros / self.b
    }
}

impl Default for Params {
    fn default() -> Params {
        Params::SPEC
    }
}

/// Receives every object a builder produces, as (id, bytes).
pub type Sink<'a> = dyn FnMut(&Hash, &[u8]) -> Result<()> + 'a;

/// `GEAR[i]`: the first 8 bytes of `H([i])`, little-endian (§2.2).
pub fn gear() -> &'static [u64; 256] {
    static GEAR: OnceLock<[u64; 256]> = OnceLock::new();
    GEAR.get_or_init(|| {
        std::array::from_fn(|i| {
            let id = h(&[i as u8]);
            let mut first = [0; 8];
            first.copy_from_slice(&id.0[..8]);
            u64::from_le_bytes(first)
        })
    })
}

/// Leading zero bits of `id`, big-endian, divided by `B` and rounded down (§2.3).
pub fn height(id: &Hash) -> u32 {
    Params::SPEC.height(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters() {
        assert_eq!((S, MIN, MAX, B, W), (1024, 256, 8192, 5, 32));
        let p = Params::SPEC;
        assert_eq!((p.k, p.min, p.max, p.b, p.w), (K, MIN, MAX, B, W));
        let p = Params::of_k(12);
        assert_eq!((p.min, p.max, p.b, p.w), (1024, 32768, 7, 128));
    }

    #[test]
    fn gear_matches_vectors() {
        let g = gear();
        assert_eq!(g[0], 0x987ab3ff9c0b346e);
        assert_eq!(g[1], 0xc55445342f12f54b);
        assert_eq!(g[255], 0xd04019aae60a10a8);
    }

    fn id(prefix: &[u8]) -> Hash {
        let mut b = [0xff; 32];
        b[..prefix.len()].copy_from_slice(prefix);
        Hash(b)
    }

    #[test]
    fn heights() {
        assert_eq!(height(&id(&[0x08])), 0, "4 zero bits");
        assert_eq!(height(&id(&[0x04])), 1, "5 zero bits");
        assert_eq!(height(&id(&[0x00, 0x20])), 2, "10 zero bits");
        assert_eq!(height(&id(&[0x00, 0x10])), 2, "11 zero bits");
        assert_eq!(height(&Hash::ZERO), 51);
    }
}
