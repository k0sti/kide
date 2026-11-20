//! An in-memory object store: for tests, and the staging of a pull in the
//! browser.

use crate::error::{Error, Result};
use crate::hash::{Hash, h};
use crate::iface::ObjectStore;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

#[derive(Default)]
pub struct MemStore(Mutex<HashMap<Hash, Vec<u8>>>);

impl MemStore {
    pub fn new() -> MemStore {
        MemStore::default()
    }

    fn map(&self) -> MutexGuard<'_, HashMap<Hash, Vec<u8>>> {
        // A poisoned lock only means another thread panicked; the map is intact.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ObjectStore for MemStore {
    fn has(&self, id: &Hash) -> Result<bool> {
        Ok(self.map().contains_key(id))
    }

    fn get(&self, id: &Hash) -> Result<Vec<u8>> {
        self.map().get(id).cloned().ok_or(Error::NotFound(*id))
    }

    fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let id = h(bytes);
        self.map().entry(id).or_insert_with(|| bytes.to_vec());
        Ok(id)
    }

    fn put_known(&self, id: &Hash, bytes: &[u8]) -> Result<()> {
        if h(bytes) != *id {
            return Err(Error::Corrupt(*id));
        }
        self.map().entry(*id).or_insert_with(|| bytes.to_vec());
        Ok(())
    }

    fn remove(&self, id: &Hash) -> Result<()> {
        self.map().remove(id);
        Ok(())
    }

    fn ids(&self) -> Result<Vec<Hash>> {
        Ok(self.map().keys().copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conformance() {
        crate::conformance::object_store(&|| Box::new(MemStore::new()));
    }
}
