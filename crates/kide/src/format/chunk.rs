//! Content-defined chunking (§2.2): one rule for all content.

use super::{MAX, Params, gear};
use crate::error::{Result, invalid};

/// The end offset of every chunk of `data` (§2.2, the spec's function).
pub fn cut_points(data: &[u8]) -> Vec<usize> {
    cut_points_with(data, &Params::SPEC)
}

/// `cut_points` under other parameters (§9).
pub fn cut_points_with(data: &[u8], p: &Params) -> Vec<usize> {
    let g = gear();
    let (mut cuts, mut start, mut h) = (Vec::new(), 0usize, 0u64);
    for (i, &b) in data.iter().enumerate() {
        h = (h << 1).wrapping_add(g[b as usize]);
        let size = i + 1 - start;
        if size >= p.min && (p.natural(h) || size == p.max) {
            cuts.push(i + 1);
            start = i + 1;
        }
    }
    if start < data.len() {
        cuts.push(data.len());
    }
    cuts
}

/// A streaming chunker: its cut points equal those of `cut_points` for every
/// way of splitting the input into `push` calls.
#[derive(Default)]
pub struct Chunker {
    p: Params,
    h: u64,
    /// Bytes since the last cut.
    size: usize,
    /// Bytes pushed so far.
    pos: u64,
}

impl Chunker {
    pub fn new() -> Chunker {
        Chunker::default()
    }

    /// A chunker under other parameters (§9).
    pub fn with(p: Params) -> Chunker {
        Chunker {
            p,
            ..Chunker::default()
        }
    }

    /// Feeds more data, appending the absolute end offset of every chunk it
    /// completes.
    pub fn push(&mut self, data: &[u8], cuts: &mut Vec<u64>) {
        let g = gear();
        for &b in data {
            self.h = (self.h << 1).wrapping_add(g[b as usize]);
            self.size += 1;
            self.pos += 1;
            if self.size >= self.p.min && (self.p.natural(self.h) || self.size == self.p.max) {
                cuts.push(self.pos);
                self.size = 0;
            }
        }
    }

    /// Bytes pushed since the last cut: the final chunk, if not zero.
    pub fn pending(&self) -> usize {
        self.size
    }

    /// Bytes pushed so far.
    pub fn position(&self) -> u64 {
        self.pos
    }
}

/// A chunk is at most `max` bytes (§2.1); manifest chunks: §5.1.
pub fn check_chunk(chunk: &[u8]) -> Result<()> {
    if chunk.len() > MAX {
        return invalid(format!("chunk of {} bytes", chunk.len()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{Rng, input_b, input_l, input_t};
    use crate::hash::{Hash, h};
    use std::collections::HashSet;

    #[test]
    fn empty_input_has_no_chunks() {
        assert!(cut_points(b"").is_empty());
        assert_eq!(cut_points(b"x"), vec![1], "a short final chunk");
    }

    #[test]
    fn forced_cuts_at_max() {
        let z = vec![0; 3 * MAX + 5];
        assert_eq!(cut_points(&z), vec![MAX, 2 * MAX, 3 * MAX, 3 * MAX + 5]);
    }

    /// Pushes `data` in pieces of 0 to `max_piece` bytes.
    fn streamed(data: &[u8], rng: &mut Rng, max_piece: u64) -> Vec<usize> {
        streamed_with(data, rng, max_piece, Params::SPEC)
    }

    fn streamed_with(data: &[u8], rng: &mut Rng, max_piece: u64, p: Params) -> Vec<usize> {
        let mut c = Chunker::with(p);
        let mut cuts = Vec::new();
        let mut rest = data;
        while !rest.is_empty() {
            let n = (rng.below(max_piece + 1) as usize).min(rest.len());
            c.push(&rest[..n], &mut cuts);
            rest = &rest[n..];
        }
        if c.pending() > 0 {
            cuts.push(c.position());
        }
        cuts.into_iter().map(|c| c as usize).collect()
    }

    #[test]
    fn streaming_equals_one_shot() {
        let mut rng = Rng::new(1);
        let random = rng.bytes(20_000);
        let t = input_t();
        for data in [&random[..], &t[..8000], &vec![0; 3 * MAX + 7]] {
            let want = cut_points(data);
            assert_eq!(streamed(data, &mut rng, 1), want, "pieces of 0 or 1 bytes");
            for i in 0..12 {
                let max_piece = [2, 7, 300, 5000][i % 4];
                assert_eq!(streamed(data, &mut rng, max_piece), want, "split {i}");
            }
        }
    }

    #[test]
    fn other_parameters() {
        let t = input_t();
        let data = &t[..40_000];
        assert_eq!(cut_points_with(data, &Params::SPEC), cut_points(data));
        let mut rng = Rng::new(3);
        for p in [
            Params::of_k(11),
            Params {
                min: 64,
                ..Params::SPEC
            },
        ] {
            let want = cut_points_with(data, &p);
            assert_ne!(want, cut_points(data));
            assert_eq!(streamed_with(data, &mut rng, 300, p), want);
            let mut start = 0;
            for &end in &want[..want.len() - 1] {
                assert!((p.min..=p.max).contains(&(end - start)));
                start = end;
            }
        }
        assert_eq!(Chunker::new().p, Params::SPEC);
    }

    fn chunk_ids(data: &[u8]) -> HashSet<Hash> {
        let mut start = 0;
        cut_points(data)
            .into_iter()
            .map(|end| {
                let id = h(&data[start..end]);
                start = end;
                id
            })
            .collect()
    }

    #[test]
    fn one_insert_changes_at_most_three_chunks() {
        let mut rng = Rng::new(2);
        for data in [input_t(), input_b(), input_l()] {
            let old = chunk_ids(&data);
            for _ in 0..3 {
                let at = rng.below(data.len() as u64 + 1) as usize;
                let byte = [rng.below(256) as u8];
                let edited = [&data[..at], &byte, &data[at..]].concat();
                let changed = chunk_ids(&edited).difference(&old).count();
                assert!(changed <= 3, "{changed} new chunks after an insert at {at}");
            }
        }
    }

    #[test]
    fn check_chunk_bounds() {
        assert!(check_chunk(&[0; MAX]).is_ok());
        assert!(check_chunk(&[0; MAX + 1]).is_err());
    }
}
