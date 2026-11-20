//! A document with history: one content that changes as a whole, each
//! change a version record whose parent is the version before (§3.1) and
//! whose `state` the application chooses (such as the change metadata of
//! §3.1), with the newest version's id in a file `HEAD` beside the store,
//! written only once everything it reaches is durable (§4.1). The chain is
//! served to pulls under `walk::Versions`.
//!
//! On disk, in the document's directory: `objects/` (a pack store) and
//! `HEAD` (the id in hex).

use crate::error::{Result, invalid};
use crate::file::write_atomic;
use crate::format::S;
use crate::format::content::write_content;
use crate::format::record::Record;
use crate::hash::Hash;
use crate::iface::ObjectStore;
use crate::load;
use crate::store::PackStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Doc {
    store: Arc<PackStore>,
    head_file: PathBuf,
    head: Option<Hash>,
}

impl Doc {
    /// Opens the document in `dir`, creating its store if needed.
    pub fn open(dir: &Path) -> Result<Doc> {
        let store = Arc::new(PackStore::open(dir.join("objects"))?);
        let head_file = dir.join("HEAD");
        let head = match std::fs::read_to_string(&head_file) {
            Ok(s) => Some(Hash::from_hex(s.trim())?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        Ok(Doc {
            store,
            head_file,
            head,
        })
    }

    /// The newest version; `None` before the first.
    pub fn head(&self) -> Option<Hash> {
        self.head
    }

    /// The store, to serve the chain from.
    pub fn store(&self) -> Arc<PackStore> {
        self.store.clone()
    }

    /// The content at `HEAD`; `None` before the first version.
    pub fn content(&self) -> Result<Option<Vec<u8>>> {
        match self.head {
            Some(head) => self.content_at(&head),
            None => Ok(None),
        }
    }

    /// The content at `version`; `None` for a tombstone.
    pub fn content_at(&self, version: &Hash) -> Result<Option<Vec<u8>>> {
        let record = load::record(&*self.store, version)?;
        if record.is_tombstone() {
            return Ok(None);
        }
        load::read_content(&*self.store, &record.root).map(Some)
    }

    /// Records `content` as a new version after `HEAD`, with `state` (at
    /// most S bytes, else `Invalid`), and moves `HEAD` to it.
    pub fn commit(&mut self, content: &[u8], state: &[u8]) -> Result<Hash> {
        if state.len() > S {
            return invalid(format!("a state of {} bytes; at most {S}", state.len()));
        }
        let store = &*self.store;
        let root = write_content(content, &mut |id, bytes| store.put_known(id, bytes))?;
        let record = Record {
            change: Vec::new(),
            parents: self.head.into_iter().collect(),
            root: root.id,
            state: state.to_vec(),
        };
        let id = store.put(&record.encode()?)?;
        store.sync()?;
        write_atomic(&self.head_file, id.to_hex().as_bytes(), true)?;
        self.head = Some(id);
        Ok(id)
    }

    /// Every version from `HEAD` back to the first, along first parents,
    /// with its state.
    pub fn versions(&self) -> Result<Vec<(Hash, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut next = self.head;
        while let Some(id) = next {
            let record = load::record(&*self.store, &id)?;
            next = record.parents.first().copied();
            out.push((id, record.state));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::TempDir;
    use crate::error::Error;

    #[test]
    fn versions_chain_and_survive_reopening() {
        let tmp = TempDir::new("doc");
        let mut doc = Doc::open(tmp.path()).unwrap();
        assert_eq!((doc.head(), doc.content().unwrap()), (None, None));
        let a = doc.commit(b"one\n", b"first").unwrap();
        let b = doc.commit(b"one\ntwo\n", b"second").unwrap();
        assert_ne!(a, b);
        drop(doc);
        let doc = Doc::open(tmp.path()).unwrap();
        assert_eq!(doc.head(), Some(b));
        assert_eq!(doc.content().unwrap().unwrap(), b"one\ntwo\n");
        assert_eq!(doc.content_at(&a).unwrap().unwrap(), b"one\n");
        let states: Vec<_> = doc.versions().unwrap().into_iter().map(|v| v.1).collect();
        assert_eq!(states, [b"second".to_vec(), b"first".to_vec()]);
    }

    #[test]
    fn a_state_over_s_bytes_is_refused_and_changes_nothing() {
        let tmp = TempDir::new("doc-state");
        let mut doc = Doc::open(tmp.path()).unwrap();
        let r = doc.commit(b"x", &vec![b'a'; S + 1]);
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        assert_eq!(doc.head(), None);
        doc.commit(b"x", &vec![b'a'; S]).unwrap();
    }
}
