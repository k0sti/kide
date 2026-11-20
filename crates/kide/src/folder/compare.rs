//! Comparing two manifests (§5.6): a merge join of their entries, skipping
//! subtrees with equal ids on both sides, which hold equal entries.

use crate::error::Result;
use crate::format::manifest::{PathEntry, decode_chunk};
use crate::hash::Hash;
use crate::iface::ObjectStore;
use crate::load;
use std::cmp::Ordering;
use std::collections::VecDeque;

/// One path whose entries differ in version, size or mtime; at least one
/// side is present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Differ {
    pub path: String,
    pub local: Option<PathEntry>,
    pub remote: Option<PathEntry>,
}

/// Every differing path of the manifests below `local` and `remote`, in
/// path order. Both manifests must be stored.
pub fn compare(store: &dyn ObjectStore, local: &Hash, remote: &Hash) -> Result<Vec<Differ>> {
    let (mut a, mut b) = (Cursor::new(store, local)?, Cursor::new(store, remote)?);
    let mut out = Vec::new();
    loop {
        if a.buf.is_empty() && b.buf.is_empty() {
            match (a.top(), b.top()) {
                (None, None) => return Ok(out),
                (Some(x), Some(y)) if x.0 == y.0 => {
                    a.items.pop(); // equal subtrees: equal entries
                    b.items.pop();
                }
                (Some(x), Some(y)) if x.1 != y.1 => {
                    let higher = if x.1 > y.1 { &mut a } else { &mut b };
                    higher.expand()?;
                }
                (Some(x), Some(_)) if x.1 > 0 => {
                    a.expand()?;
                    b.expand()?;
                }
                (Some(_), Some(_)) => {
                    a.load()?; // two different chunks
                    b.load()?;
                }
                (Some(_), None) => a.load()?,
                (None, Some(_)) => b.load()?,
            }
            continue;
        }
        for c in [&mut a, &mut b] {
            if c.buf.is_empty() && c.top().is_some() {
                c.load()?;
            }
        }
        let order = match (a.buf.front(), b.buf.front()) {
            (Some(x), Some(y)) => x.path.cmp(&y.path),
            (Some(_), None) => Ordering::Less,
            (None, _) => Ordering::Greater,
        };
        let (local, remote) = match order {
            Ordering::Less => (a.buf.pop_front(), None),
            Ordering::Greater => (None, b.buf.pop_front()),
            Ordering::Equal => (a.buf.pop_front(), b.buf.pop_front()),
        };
        if local != remote {
            let path = local.as_ref().or(remote.as_ref()).map(|e| e.path.clone());
            // One side is present: a buffer was non-empty.
            let path = path.unwrap_or_default();
            out.push(Differ {
                path,
                local,
                remote,
            });
        }
    }
}

/// A position in one manifest: the subtrees still to read, the next on top,
/// with their levels (0 for a chunk), and the entries of a chunk being read.
struct Cursor<'a> {
    store: &'a dyn ObjectStore,
    items: Vec<(Hash, u8)>,
    buf: VecDeque<PathEntry>,
}

impl<'a> Cursor<'a> {
    fn new(store: &'a dyn ObjectStore, root: &Hash) -> Result<Cursor<'a>> {
        let level = load::node(store, root)?.level;
        Ok(Cursor {
            store,
            items: vec![(*root, level)],
            buf: VecDeque::new(),
        })
    }

    fn top(&self) -> Option<(Hash, u8)> {
        self.items.last().copied()
    }

    /// Replaces the node on top with its children; a chunk stays.
    fn expand(&mut self) -> Result<()> {
        let Some((id, level)) = self.top().filter(|t| t.1 > 0) else {
            return Ok(());
        };
        self.items.pop();
        let node = load::node(self.store, &id)?;
        let below = level - 1; // a level-1 node holds chunks, level 0
        self.items
            .extend(node.children.iter().rev().map(|c| (c.id, below)));
        Ok(())
    }

    /// Reads the next chunk into the buffer.
    fn load(&mut self) -> Result<()> {
        while self.top().is_some_and(|t| t.1 > 0) {
            self.expand()?;
        }
        if let Some((id, _)) = self.items.pop() {
            self.buf.extend(decode_chunk(&self.store.get(&id)?)?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::manifest::build_manifest;
    use crate::store::MemStore;

    fn entry(path: &str, v: u8) -> PathEntry {
        PathEntry {
            path: path.into(),
            version: Hash([v; 32]),
            size: 0,
            mtime: 0,
        }
    }

    fn manifest(s: &MemStore, entries: &[PathEntry]) -> Hash {
        build_manifest(entries, &mut |id, b| s.put_known(id, b))
            .unwrap()
            .id
    }

    /// The merge join over every entry, without skipping.
    fn naive(a: &[PathEntry], b: &[PathEntry]) -> Vec<Differ> {
        let mut paths: Vec<&str> = a.iter().chain(b).map(|e| e.path.as_str()).collect();
        paths.sort();
        paths.dedup();
        let find = |v: &[PathEntry], p: &str| v.iter().find(|e| e.path == p).cloned();
        paths
            .into_iter()
            .map(|p| Differ {
                path: p.into(),
                local: find(a, p),
                remote: find(b, p),
            })
            .filter(|d| d.local != d.remote)
            .collect()
    }

    /// A store that counts reads.
    #[derive(Default)]
    struct Counting(MemStore, std::sync::atomic::AtomicUsize);

    impl ObjectStore for Counting {
        fn has(&self, id: &Hash) -> Result<bool> {
            self.0.has(id)
        }
        fn get(&self, id: &Hash) -> Result<Vec<u8>> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.0.get(id)
        }
        fn put(&self, bytes: &[u8]) -> Result<Hash> {
            self.0.put(bytes)
        }
        fn put_known(&self, id: &Hash, bytes: &[u8]) -> Result<()> {
            self.0.put_known(id, bytes)
        }
        fn remove(&self, id: &Hash) -> Result<()> {
            self.0.remove(id)
        }
        fn ids(&self) -> Result<Vec<Hash>> {
            self.0.ids()
        }
    }

    #[test]
    fn equal_subtrees_are_skipped() {
        let s = Counting::default();
        let big: Vec<PathEntry> = (0..3000)
            .map(|i| entry(&format!("dir/file-{i:05}"), (i % 250) as u8))
            .collect();
        let root = manifest(&s.0, &big);
        let mut edited = big.clone();
        edited[1500].version = Hash([0xee; 32]);
        let other = manifest(&s.0, &edited);
        assert!(compare(&s, &root, &root).unwrap().is_empty());
        let reads = s.1.swap(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(reads, 2, "the two roots only");
        let got = compare(&s, &root, &other).unwrap();
        assert_eq!(got.len(), 1);
        let reads = s.1.load(std::sync::atomic::Ordering::Relaxed);
        let entries = crate::load::read_manifest(&s.0, &root).unwrap().len();
        assert!(
            reads < 20,
            "{reads} reads for one changed entry of {entries}"
        );
    }

    #[test]
    fn matches_a_join_over_all_entries() {
        let s = MemStore::new();
        let mut rng = crate::conformance::Rng::new(3);
        for round in 0..10 {
            let base: Vec<PathEntry> = (0..800).map(|i| entry(&format!("p/{i:05}"), 1)).collect();
            let mut other = base.clone();
            for _ in 0..rng.below(6) {
                let i = rng.below(other.len() as u64) as usize;
                match rng.below(3) {
                    0 => other[i].version = Hash([2; 32]),
                    1 => drop(other.remove(i)),
                    _ => other.insert(i + 1, entry(&format!("{}x", other[i].path), 3)),
                }
            }
            let (ra, rb) = (manifest(&s, &base), manifest(&s, &other));
            let got = compare(&s, &ra, &rb).unwrap();
            assert_eq!(got, naive(&base, &other), "round {round}");
            let back = compare(&s, &rb, &ra).unwrap();
            assert_eq!(back, naive(&other, &base), "round {round}, swapped");
        }
        let empty = manifest(&s, &[]);
        let one = manifest(&s, &[entry("a", 1)]);
        assert_eq!(compare(&s, &empty, &one).unwrap().len(), 1);
        assert!(compare(&s, &empty, &empty).unwrap().is_empty());
    }
}
