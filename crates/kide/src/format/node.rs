//! Nodes (§2.3): chunks grouped into layered nodes, with boundaries set by the
//! heights of ids, so the tree depends only on the content.

use super::{Params, Sink, W, height};
use crate::error::{Result, invalid};
use crate::hash::{Hash, h};

const CHILD: usize = 40;

/// A chunk or node as seen from its parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: Hash,
    /// For a chunk, the height of its id; for a node, that of its last chunk.
    pub height: u32,
    /// The length of the content below.
    pub bytes: u64,
}

/// The root of some content: a node, with its level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Root {
    pub id: Hash,
    pub level: u8,
    pub bytes: u64,
}

/// A decoded node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub level: u8,
    pub children: Vec<Entry>,
}

/// The entry of one chunk: chunks are raw bytes, so the id is `H(chunk)`.
pub fn chunk_entry(chunk: &[u8]) -> Entry {
    chunk_entry_with(chunk, &Params::SPEC)
}

/// `chunk_entry` under other parameters (§9): they set the height.
pub fn chunk_entry_with(chunk: &[u8], p: &Params) -> Entry {
    let id = h(chunk);
    Entry {
        id,
        height: p.height(&id),
        bytes: chunk.len() as u64,
    }
}

pub fn encode_node(level: u8, children: &[Entry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + CHILD * children.len());
    out.push(level);
    for c in children {
        out.extend(c.id.0);
        out.extend(c.bytes.to_le_bytes());
    }
    out
}

impl Node {
    /// Decodes and validates one node on its own (§2.3). The checks that need
    /// a child (its length and level) belong to whoever holds the child.
    pub fn decode(b: &[u8]) -> Result<Node> {
        if b.is_empty() || !(b.len() - 1).is_multiple_of(CHILD) {
            return invalid(format!("node of {} bytes", b.len()));
        }
        let (level, n) = (b[0], (b.len() - 1) / CHILD);
        if level < 1 {
            return invalid("node level 0");
        }
        if n > W || (n == 0 && level != 1) {
            return invalid(format!("level-{level} node with {n} children"));
        }
        let children: Vec<Entry> = b[1..]
            .as_chunks::<CHILD>()
            .0
            .iter()
            .map(|c| {
                let id = Hash::at(c);
                let mut bytes = [0; 8];
                bytes.copy_from_slice(&c[32..]);
                Entry {
                    id,
                    height: height(&id),
                    bytes: u64::from_le_bytes(bytes),
                }
            })
            .collect();
        if children.iter().any(|c| c.bytes == 0) {
            return invalid("child of 0 bytes");
        }
        let node = Node { level, children };
        if node.checked_bytes().is_none() {
            return invalid("node content longer than u64");
        }
        Ok(node)
    }

    fn checked_bytes(&self) -> Option<u64> {
        self.children
            .iter()
            .try_fold(0u64, |s, c| s.checked_add(c.bytes))
    }

    /// The length of the content below this node.
    pub fn bytes(&self) -> u64 {
        // Decode and build both guarantee that the sum fits.
        self.checked_bytes().unwrap_or(u64::MAX)
    }

    /// The child holding byte `offset`, and the offset within it.
    pub fn child_at(&self, offset: u64) -> Option<(usize, u64)> {
        let mut start = 0;
        for (i, c) in self.children.iter().enumerate() {
            if offset - start < c.bytes {
                return Some((i, offset - start));
            }
            start += c.bytes;
        }
        None
    }
}

/// Follows the children's `bytes` from `root` to the chunk holding byte
/// `offset` (§2.3, A9); returns the chunk's entry and the offset within it.
/// `load` returns a node's bytes; each node is checked against its parent.
#[cfg(test)]
pub(crate) fn locate(
    root: &Hash,
    offset: u64,
    load: &mut dyn FnMut(&Hash) -> Result<Vec<u8>>,
) -> Result<(Entry, u64)> {
    let mut node = Node::decode(&load(root)?)?;
    let mut offset = offset;
    loop {
        let Some((i, within)) = node.child_at(offset) else {
            return invalid(format!("offset beyond the content of {root}"));
        };
        let child = node.children[i];
        if node.level == 1 {
            return Ok((child, within));
        }
        let next = Node::decode(&load(&child.id)?)?;
        if next.level != node.level - 1 || next.bytes() != child.bytes {
            return invalid(format!("node {} disagrees with its parent", child.id));
        }
        (node, offset) = (next, within);
    }
}

/// Builds the tree over the chunk entries of some content (§2.3), passing every
/// node to `sink`, and returns the root.
pub fn build(chunks: Vec<Entry>, sink: &mut Sink) -> Result<Root> {
    build_with(chunks, &Params::SPEC, sink)
}

/// `build` under other parameters (§9): `p.w` bounds a node; the entries'
/// heights must come from the same parameters (`chunk_entry_with`).
pub fn build_with(chunks: Vec<Entry>, p: &Params, sink: &mut Sink) -> Result<Root> {
    if chunks.is_empty() {
        return make_node(1, &[], sink).map(|e| root(e, 1));
    }
    let (mut items, mut level) = (chunks, 1u8);
    loop {
        let (mut next, mut group) = (Vec::new(), Vec::new());
        for e in items {
            let closes = e.height >= level as u32 || group.len() + 1 == p.w;
            group.push(e);
            if closes {
                next.push(make_node(level, &group, sink)?);
                group.clear();
            }
        }
        if !group.is_empty() {
            next.push(make_node(level, &group, sink)?);
        }
        if let [e] = next[..] {
            return Ok(root(e, level));
        }
        items = next;
        // Heights are at most 51, so above level 51 every level divides the
        // count by W; no u64-long content reaches level 65.
        level += 1;
    }
}

fn root(e: Entry, level: u8) -> Root {
    Root {
        id: e.id,
        level,
        bytes: e.bytes,
    }
}

fn make_node(level: u8, children: &[Entry], sink: &mut Sink) -> Result<Entry> {
    let Some(bytes) = children
        .iter()
        .try_fold(0u64, |s, c| s.checked_add(c.bytes))
    else {
        return invalid("content longer than u64");
    };
    let enc = encode_node(level, children);
    let id = h(&enc);
    sink(&id, &enc)?;
    Ok(Entry {
        id,
        height: children.last().map_or(0, |c| c.height),
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::input_b;
    use crate::format::chunk::cut_points;
    use std::collections::HashMap;

    fn tree(data: &[u8]) -> (Root, HashMap<Hash, Vec<u8>>) {
        let mut nodes = HashMap::new();
        let mut start = 0;
        let chunks = cut_points(data)
            .into_iter()
            .map(|end| {
                let e = chunk_entry(&data[start..end]);
                start = end;
                e
            })
            .collect();
        let root = build(chunks, &mut |id, b| {
            nodes.insert(*id, b.to_vec());
            Ok(())
        })
        .unwrap();
        (root, nodes)
    }

    fn entry(i: u64, bytes: u64) -> Entry {
        let id = h(&i.to_le_bytes());
        Entry {
            id,
            height: height(&id),
            bytes,
        }
    }

    #[test]
    fn encode_decode_round_trip() {
        let children: Vec<Entry> = (0..5).map(|i| entry(i, 1000 + i)).collect();
        let enc = encode_node(3, &children);
        assert_eq!(enc.len(), 1 + 40 * 5);
        assert_eq!(enc[0], 3);
        let n = Node::decode(&enc).unwrap();
        assert_eq!(n, Node { level: 3, children });
        assert_eq!(n.bytes(), 5010);
        assert_eq!(Node::decode(&[1]).unwrap().children, vec![]);
    }

    #[test]
    fn decode_rejects_invalid_nodes() {
        let ok = encode_node(2, &[entry(1, 1)]);
        assert!(Node::decode(&ok).is_ok());
        let bad: [(&str, Vec<u8>); 8] = [
            ("empty", vec![]),
            ("level 0", [&[0][..], &ok[1..]].concat()),
            ("level 0, no children", vec![0]),
            ("level 2, no children", vec![2]),
            ("short", ok[..ok.len() - 1].to_vec()),
            ("long", [&ok[..], &[0]].concat()),
            ("child of 0 bytes", encode_node(1, &[entry(1, 0)])),
            (
                "more than W children",
                encode_node(1, &vec![entry(1, 1); W + 1]),
            ),
        ];
        for (name, b) in bad {
            assert!(Node::decode(&b).is_err(), "{name}");
        }
        assert!(Node::decode(&encode_node(1, &vec![entry(1, 1); W])).is_ok());
        let sum = encode_node(1, &[entry(1, u64::MAX), entry(2, 1)]);
        assert!(Node::decode(&sum).is_err(), "sum above u64");
        assert!(Node::decode(&encode_node(1, &[entry(1, u64::MAX)])).is_ok());
    }

    #[test]
    fn single_chunk_root_is_a_node() {
        let (root, nodes) = tree(b"hello\n");
        assert_eq!((root.level, root.bytes), (1, 6));
        assert_eq!(Node::decode(&nodes[&root.id]).unwrap().children.len(), 1);
    }

    #[test]
    fn w_bounds_every_node() {
        // Height-0 entries never close a group, so only W does.
        let flat: Vec<Entry> = (0..)
            .map(|i| entry(i, 1))
            .filter(|e| e.height == 0)
            .take(W * W + 3)
            .collect();
        let mut widest = 0;
        let root = build(flat, &mut |_, b| {
            widest = widest.max(Node::decode(b).unwrap().children.len());
            Ok(())
        })
        .unwrap();
        assert_eq!((widest, root.level), (W, 3));
    }

    #[test]
    fn locate_finds_every_chunk() {
        let data = input_b();
        let (root, nodes) = tree(&data);
        assert_eq!(root.level, 2);
        let mut load = |id: &Hash| Ok(nodes[id].clone());
        for offset in [0, 1, 255, 4096, 30_001, 262_143] {
            let (chunk, within) = locate(&root.id, offset, &mut load).unwrap();
            let start = (offset - within) as usize;
            let bytes = &data[start..start + chunk.bytes as usize];
            assert_eq!(chunk.id, h(bytes), "offset {offset}");
        }
        assert!(locate(&root.id, 262_144, &mut load).is_err());
    }

    #[test]
    fn locate_rejects_a_child_that_disagrees() {
        let child = encode_node(1, &[entry(1, 10)]);
        let parent = encode_node(
            2,
            &[Entry {
                bytes: 11,
                ..chunk_entry(&child)
            }],
        );
        let nodes = HashMap::from([(h(&child), child), (h(&parent), parent.clone())]);
        let mut load = |id: &Hash| Ok(nodes[id].clone());
        assert!(locate(&h(&parent), 0, &mut load).is_err());
    }
}
