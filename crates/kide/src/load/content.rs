//! Content read back from a store as a stream (§2.3, A9): from a byte offset,
//! one node per level down to the chunk that holds it, then chunk by chunk.
//! Every node is checked against its parent and every chunk's length against
//! its entry; a disagreement is `Invalid`.

use super::node;
use crate::error::{Result, invalid};
use crate::format::node::{Entry, Node};
use crate::hash::Hash;
use crate::iface::ObjectStore;
use std::io::{self, Read};

pub struct ContentReader<'a> {
    store: &'a dyn ObjectStore,
    /// The nodes from the root down to the current chunk's, each with the
    /// index of the child being read.
    path: Vec<(Node, usize)>,
    chunk: Vec<u8>,
    /// The read position in `chunk`.
    pos: usize,
    /// Bytes of the range not yet read.
    left: u64,
}

impl<'a> ContentReader<'a> {
    /// The whole content below `root`.
    pub fn open(store: &'a dyn ObjectStore, root: &Hash) -> Result<ContentReader<'a>> {
        let n = node(store, root)?;
        let len = n.bytes();
        ContentReader::start(store, n, 0, len)
    }

    /// `len` bytes from `offset`; `Invalid` if the range passes the end.
    pub fn range(
        store: &'a dyn ObjectStore,
        root: &Hash,
        offset: u64,
        len: u64,
    ) -> Result<ContentReader<'a>> {
        let n = node(store, root)?;
        if offset.checked_add(len).is_none_or(|end| end > n.bytes()) {
            return invalid(format!("range {offset}+{len} beyond the content of {root}"));
        }
        ContentReader::start(store, n, offset, len)
    }

    fn start(store: &'a dyn ObjectStore, root: Node, offset: u64, len: u64) -> Result<Self> {
        let mut r = ContentReader {
            store,
            path: Vec::new(),
            chunk: Vec::new(),
            pos: 0,
            left: len,
        };
        if len > 0 {
            r.enter(root, offset)?;
        }
        Ok(r)
    }

    /// Descends from `node` to the chunk holding `offset` within it.
    fn enter(&mut self, mut node: Node, mut offset: u64) -> Result<()> {
        loop {
            let Some((i, within)) = node.child_at(offset) else {
                return invalid("an offset beyond its node");
            };
            let (child, level) = (node.children[i], node.level);
            self.path.push((node, i));
            if level == 1 {
                return self.load_chunk(&child, within);
            }
            node = self.child_node(&child, level - 1)?;
            offset = within;
        }
    }

    /// Moves to the first byte of the next chunk.
    fn advance(&mut self) -> Result<()> {
        while let Some((node, i)) = self.path.pop() {
            if i + 1 == node.children.len() {
                continue;
            }
            let (child, level) = (node.children[i + 1], node.level);
            self.path.push((node, i + 1));
            if level == 1 {
                return self.load_chunk(&child, 0);
            }
            let next = self.child_node(&child, level - 1)?;
            return self.enter(next, 0);
        }
        invalid("content shorter than its root says")
    }

    fn child_node(&self, child: &Entry, level: u8) -> Result<Node> {
        let n = node(self.store, &child.id)?;
        if n.level != level || n.bytes() != child.bytes {
            return invalid(format!("node {} disagrees with its parent", child.id));
        }
        Ok(n)
    }

    fn load_chunk(&mut self, child: &Entry, pos: u64) -> Result<()> {
        self.chunk = self.store.get(&child.id)?;
        if self.chunk.len() as u64 != child.bytes {
            return invalid(format!("chunk {} disagrees with its node", child.id));
        }
        self.pos = pos as usize; // less than the chunk's length
        Ok(())
    }

    /// The next piece of the range: the rest of the current chunk, at most
    /// what is left; `None` at the end.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.left == 0 {
            return Ok(None);
        }
        if self.pos == self.chunk.len() {
            self.advance()?;
        }
        let n = (self.chunk.len() - self.pos).min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let piece = self.chunk[self.pos..self.pos + n].to_vec();
        (self.pos, self.left) = (self.pos + n, self.left - n as u64);
        Ok(Some(piece))
    }
}

/// Errors pass as `io::Error` and come back unchanged through `Error::from`.
impl Read for ContentReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        if self.pos == self.chunk.len() {
            self.advance()?;
        }
        let n = (self.chunk.len() - self.pos)
            .min(buf.len())
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        (self.pos, self.left) = (self.pos + n, self.left - n as u64);
        Ok(n)
    }
}

/// The whole content below `root`.
pub fn read_content(store: &dyn ObjectStore, root: &Hash) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ContentReader::open(store, root)?.read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{Rng, input_t};
    use crate::error::Error;
    use crate::format::content::write_content;
    use crate::format::node::{chunk_entry, encode_node, locate};
    use crate::store::MemStore;

    fn stored(data: &[u8]) -> (MemStore, Hash) {
        let s = MemStore::new();
        let root = write_content(data, &mut |id, b| s.put_known(id, b)).unwrap();
        (s, root.id)
    }

    #[test]
    fn whole_content_round_trips() {
        let t = input_t();
        for data in [&t[..], &t[..5000], b"hello\n", b""] {
            let (s, root) = stored(data);
            assert_eq!(
                read_content(&s, &root).unwrap(),
                data,
                "{} bytes",
                data.len()
            );
        }
    }

    #[test]
    fn ranges_and_pieces() {
        let t = input_t();
        let (s, root) = stored(&t);
        let mut rng = Rng::new(3);
        for _ in 0..40 {
            let offset = rng.below(t.len() as u64 + 1);
            let len = rng.below(t.len() as u64 - offset + 1).min(30_000);
            let mut r = ContentReader::range(&s, &root, offset, len).unwrap();
            let mut got = Vec::new();
            while let Some(piece) = r.next_chunk().unwrap() {
                assert!(!piece.is_empty());
                got.extend(piece);
            }
            let (a, b) = (offset as usize, (offset + len) as usize);
            assert_eq!(got, &t[a..b], "range {offset}+{len}");
        }
        let len = t.len() as u64;
        assert!(ContentReader::range(&s, &root, len, 0).is_ok());
        assert!(ContentReader::range(&s, &root, len, 1).is_err());
        assert!(ContentReader::range(&s, &root, 1, u64::MAX).is_err());
    }

    #[test]
    fn a_missing_chunk_is_not_found_through_read() {
        let t = input_t();
        let (s, root) = stored(&t[..20_000]);
        let (victim, _) = locate(&root, 19_999, &mut |id| s.get(id)).unwrap();
        s.remove(&victim.id).unwrap();
        let mut r = ContentReader::open(&s, &root).unwrap();
        assert!(
            r.next_chunk().unwrap().is_some(),
            "the first chunk is still there"
        );
        match read_content(&s, &root) {
            Err(Error::NotFound(id)) => assert_eq!(id, victim.id),
            other => panic!("expected NotFound, got {other:?}"),
        }
        s.remove(&root).unwrap();
        assert!(matches!(
            ContentReader::open(&s, &root),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn a_tree_that_disagrees_is_invalid() {
        let s = MemStore::new();
        let chunk = b"twelve bytes";
        s.put(chunk).unwrap();
        let short = s.put(&encode_node(1, &[chunk_entry(&chunk[..])])).unwrap();
        let wrong = |bytes| {
            encode_node(
                2,
                &[Entry {
                    bytes,
                    ..chunk_entry(&s.get(&short).unwrap())
                }],
            )
        };
        let root = s.put(&wrong(13)).unwrap();
        assert!(matches!(read_content(&s, &root), Err(Error::Invalid(_))));
        let lying = s
            .put(&encode_node(
                1,
                &[Entry {
                    bytes: 11,
                    ..chunk_entry(&chunk[..])
                }],
            ))
            .unwrap();
        assert!(matches!(read_content(&s, &lying), Err(Error::Invalid(_))));
        assert_eq!(
            read_content(&s, &s.put(&wrong(12)).unwrap()).unwrap(),
            chunk
        );
    }
}
