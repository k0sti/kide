//! Content (§2.2, §2.3): bytes chunked as a stream and built into a tree, with
//! every chunk and node passed to a sink.

use super::chunk::Chunker;
use super::node::{Entry, Root, build_with, chunk_entry_with};
use super::{Params, Sink};
use crate::error::Result;

/// Writes content of any length in pieces; the root is independent of the
/// way the content was split.
#[derive(Default)]
pub struct ContentWriter {
    p: Params,
    chunker: Chunker,
    /// The bytes of the unfinished chunk.
    pending: Vec<u8>,
    chunks: Vec<Entry>,
    cuts: Vec<u64>,
}

impl ContentWriter {
    pub fn new() -> ContentWriter {
        ContentWriter::default()
    }

    /// A writer under other parameters (§9).
    pub fn with(p: Params) -> ContentWriter {
        ContentWriter {
            p,
            chunker: Chunker::with(p),
            ..ContentWriter::default()
        }
    }

    /// Chunks `data`, passing every completed chunk to `sink`.
    pub fn write(&mut self, data: &[u8], sink: &mut Sink) -> Result<()> {
        let base = self.chunker.position();
        self.cuts.clear();
        self.chunker.push(data, &mut self.cuts);
        let mut last = 0;
        for &cut in &self.cuts {
            let end = (cut - base) as usize; // a cut lies within this piece
            if self.pending.is_empty() {
                emit(&data[last..end], &self.p, &mut self.chunks, sink)?;
            } else {
                self.pending.extend_from_slice(&data[last..end]);
                emit(&self.pending, &self.p, &mut self.chunks, sink)?;
                self.pending.clear();
            }
            last = end;
        }
        self.pending.extend_from_slice(&data[last..]);
        Ok(())
    }

    /// Ends the content: passes the final chunk and every node to `sink` and
    /// returns the root.
    pub fn finish(mut self, sink: &mut Sink) -> Result<Root> {
        if !self.pending.is_empty() {
            emit(&self.pending, &self.p, &mut self.chunks, sink)?;
        }
        build_with(self.chunks, &self.p, sink)
    }
}

fn emit(chunk: &[u8], p: &Params, chunks: &mut Vec<Entry>, sink: &mut Sink) -> Result<()> {
    let e = chunk_entry_with(chunk, p);
    sink(&e.id, chunk)?;
    chunks.push(e);
    Ok(())
}

/// The root of `data`, with every chunk and node passed to `sink`.
pub fn write_content(data: &[u8], sink: &mut Sink) -> Result<Root> {
    write_content_with(data, Params::SPEC, sink)
}

/// `write_content` under other parameters (§9).
pub fn write_content_with(data: &[u8], p: Params, sink: &mut Sink) -> Result<Root> {
    let mut w = ContentWriter::with(p);
    w.write(data, sink)?;
    w.finish(sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{Rng, input_t};
    use crate::format::node::Node;
    use crate::hash::{Hash, h};
    use std::collections::HashMap;

    fn objects(data: &[u8], pieces: &[usize]) -> (Root, HashMap<Hash, Vec<u8>>) {
        let mut objs = HashMap::new();
        let mut sink = |id: &Hash, b: &[u8]| {
            assert_eq!(h(b), *id);
            objs.insert(*id, b.to_vec());
            Ok(())
        };
        let mut w = ContentWriter::new();
        let mut rest = data;
        for &n in pieces.iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let n = n.min(rest.len());
            w.write(&rest[..n], &mut sink).unwrap();
            rest = &rest[n..];
        }
        let root = w.finish(&mut sink).unwrap();
        (root, objs)
    }

    #[test]
    fn root_is_independent_of_the_split() {
        let t = input_t();
        let data = &t[..20_000];
        let (want, all) = objects(data, &[data.len()]);
        assert_eq!(want, write_content(data, &mut |_, _| Ok(())).unwrap());
        let mut rng = Rng::new(4);
        for max in [1, 100, 3000] {
            let pieces: Vec<usize> = (0..50).map(|_| rng.below(max) as usize + 1).collect();
            assert_eq!(
                objects(data, &pieces),
                (want, all.clone()),
                "pieces up to {max}"
            );
        }
    }

    #[test]
    fn other_parameters() {
        let t = input_t();
        let data = &t[..60_000];
        let spec = write_content_with(data, Params::SPEC, &mut |_, _| Ok(())).unwrap();
        assert_eq!(spec, write_content(data, &mut |_, _| Ok(())).unwrap());
        let narrow = Params {
            w: 2,
            ..Params::SPEC
        };
        let mut objs = HashMap::new();
        let root = write_content_with(data, narrow, &mut |id, b| {
            objs.insert(*id, b.to_vec());
            Ok(())
        })
        .unwrap();
        assert_ne!(root, spec);
        let (mut todo, mut widest) = (vec![(root.id, root.level)], 0);
        while let Some((id, level)) = todo.pop() {
            let n = Node::decode(&objs[&id]).unwrap();
            widest = widest.max(n.children.len());
            if level > 1 {
                todo.extend(n.children.iter().map(|c| (c.id, level - 1)));
            }
        }
        assert_eq!(widest, 2, "w bounds every node");
    }

    #[test]
    fn empty_content() {
        let (root, objs) = objects(b"", &[1]);
        assert_eq!((root.level, root.bytes), (1, 0));
        assert_eq!(objs.len(), 1, "only the empty node");
    }
}
