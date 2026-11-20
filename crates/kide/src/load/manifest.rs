//! A folder's manifest read back from a store (§5.1), and the validation of
//! a whole manifest, which needs the records it names and their roots.

use super::content::ContentReader;
use super::{node, record};
use crate::error::Result;
use crate::format::manifest::{Kind, LiveKind, PathEntry, decode_chunk, validate};
use crate::hash::Hash;
use crate::iface::ObjectStore;

/// The entries of the manifest below `root`, each chunk decoded and
/// validated on its own.
pub fn read_manifest(store: &dyn ObjectStore, root: &Hash) -> Result<Vec<PathEntry>> {
    let mut r = ContentReader::open(store, root)?;
    let mut entries = Vec::new();
    while let Some(chunk) = r.next_chunk()? {
        entries.extend(decode_chunk(&chunk)?);
    }
    Ok(entries)
}

/// Reads and validates a whole manifest (§5.1). `NotFound` if a record it
/// names, or the root of a live one, is not in `store`.
pub fn validate_manifest(store: &dyn ObjectStore, root: &Hash) -> Result<Vec<PathEntry>> {
    let entries = read_manifest(store, root)?;
    validate(&entries, &mut |id| live(store, id))?;
    Ok(entries)
}

fn live(store: &dyn ObjectStore, id: &Hash) -> Result<LiveKind> {
    let r = record(store, id)?;
    match Kind::of(&r)? {
        None => Ok(None),
        Some(kind) => Ok(Some((kind, node(store, &r.root)?.bytes()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::format::content::write_content;
    use crate::format::manifest::build_manifest;
    use crate::format::record::Record;
    use crate::store::MemStore;

    struct Folder {
        store: MemStore,
        entries: Vec<PathEntry>,
    }

    impl Folder {
        fn new() -> Folder {
            Folder {
                store: MemStore::new(),
                entries: Vec::new(),
            }
        }

        fn content(&self, data: &[u8]) -> Hash {
            write_content(data, &mut |id, b| self.store.put_known(id, b))
                .unwrap()
                .id
        }

        fn record(&self, r: Record) -> Hash {
            self.store.put(&r.encode().unwrap()).unwrap()
        }

        fn add(&mut self, path: &str, kind: Kind, data: &[u8], mtime: i64) -> Hash {
            let root = self.content(data);
            let v = self.record(Record {
                change: Vec::new(),
                parents: vec![],
                root,
                state: kind.state(),
            });
            let size = data.len() as u64;
            self.entries.push(PathEntry {
                path: path.into(),
                version: v,
                size,
                mtime,
            });
            v
        }

        fn root(&mut self) -> Hash {
            self.entries.sort_by(|a, b| a.path.cmp(&b.path));
            build_manifest(&self.entries, &mut |id, b| self.store.put_known(id, b))
                .unwrap()
                .id
        }
    }

    fn sample() -> Folder {
        let mut f = Folder::new();
        f.add("a.txt", Kind::Regular, b"hello\n", 5);
        f.add("b/run.sh", Kind::Executable, b"#!/bin/sh\n", 6);
        f.add("b/link", Kind::Symlink, b"run.sh", 0);
        f.add("d/", Kind::Directory, b"", 0);
        let gone = f.add("gone", Kind::Regular, b"x", 1);
        let t = f.record(Record::tombstone(vec![gone]));
        f.entries.retain(|e| e.path != "gone");
        f.entries.push(PathEntry {
            path: "gone".into(),
            version: t,
            size: 0,
            mtime: 0,
        });
        for i in 0..300 {
            f.add(&format!("many/f{i:04}"), Kind::Regular, &[i as u8; 3], i);
        }
        f
    }

    #[test]
    fn a_valid_manifest_reads_back() {
        let mut f = sample();
        let root = f.root();
        let got = validate_manifest(&f.store, &root).unwrap();
        assert_eq!(got, f.entries);
        assert_eq!(read_manifest(&f.store, &f.content(b"")).unwrap(), vec![]);
    }

    #[test]
    fn missing_records_and_roots_are_not_found() {
        let mut f = sample();
        let root = f.root();
        let v = f.entries[0].version;
        let content = record(&f.store, &v).unwrap().root;
        f.store.remove(&content).unwrap();
        let r = validate_manifest(&f.store, &root);
        assert!(
            matches!(r, Err(Error::NotFound(x)) if x == content),
            "{r:?}"
        );
        f.store.remove(&v).unwrap();
        let r = validate_manifest(&f.store, &root);
        assert!(matches!(r, Err(Error::NotFound(x)) if x == v), "{r:?}");
        f.store.remove(&root).unwrap();
        assert!(matches!(
            read_manifest(&f.store, &root),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn records_that_disagree_with_entries_are_invalid() {
        type Mutation = fn(&mut Folder);
        let mutations: [(&str, Mutation); 5] = [
            ("wrong size", |f| f.entries[0].size += 1),
            ("a file below a file", |f| {
                f.add("a.txt/x", Kind::Regular, b"x", 1);
            }),
            ("a file below an empty directory", |f| {
                f.add("d/x", Kind::Regular, b"x", 1);
            }),
            ("a link with an mtime", |f| {
                f.add("l", Kind::Symlink, b"t", 1);
            }),
            ("a directory without its slash", |f| {
                f.add("e", Kind::Directory, b"", 0);
            }),
        ];
        for (name, m) in mutations {
            let mut f = sample();
            m(&mut f);
            let root = f.root();
            let r = validate_manifest(&f.store, &root);
            assert!(matches!(r, Err(Error::Invalid(_))), "{name}: {r:?}");
        }
    }
}
