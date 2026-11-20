//! Objects read back from a store by kind (A8): nodes, records, content and
//! manifests, with the validation that needs other objects (§2.3, §3.1,
//! §5.1). Missing objects fail with `NotFound`, which is never taken for
//! anything else (§4.4).

mod children;
mod content;
mod manifest;

pub use children::{children, split_children};
pub use content::{ContentReader, read_content};
pub use manifest::{read_manifest, validate_manifest};

use crate::error::Result;
#[cfg(test)]
use crate::error::invalid;
use crate::format::node::Node;
use crate::format::record::Record;
use crate::hash::Hash;
use crate::iface::ObjectStore;

/// The node `id`, decoded and validated on its own.
pub fn node(store: &dyn ObjectStore, id: &Hash) -> Result<Node> {
    Node::decode(&store.get(id)?)
}

/// The version record `id`, decoded and validated on its own.
pub fn record(store: &dyn ObjectStore, id: &Hash) -> Result<Record> {
    Record::decode(&store.get(id)?)
}

/// The rule of §3.1 that needs the parents: no parent of a merge record is a
/// merge record. `NotFound` if a parent is not in `store`.
#[cfg(test)]
pub(crate) fn check_parents(store: &dyn ObjectStore, r: &Record) -> Result<()> {
    if !r.is_merge() {
        return Ok(());
    }
    for p in &r.parents {
        if record(store, p)?.is_merge() {
            return invalid(format!(
                "merge record with the merge record {p} as a parent"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::store::MemStore;

    fn put(s: &MemStore, r: &Record) -> Hash {
        s.put(&r.encode().unwrap()).unwrap()
    }

    fn live(parents: Vec<Hash>, root: u8) -> Record {
        let mut parents = parents;
        parents.sort();
        Record {
            change: Vec::new(),
            parents,
            root: Hash([root; 32]),
            state: vec![0],
        }
    }

    #[test]
    fn merge_parents_are_not_merges() {
        let s = MemStore::new();
        let (x, y, z) = (
            put(&s, &live(vec![], 1)),
            put(&s, &live(vec![], 2)),
            put(&s, &live(vec![], 3)),
        );
        let xy = live(vec![x, y], 1);
        check_parents(&s, &xy).unwrap();
        check_parents(&s, &live(vec![x], 9)).unwrap();
        let m = put(&s, &xy);
        check_parents(&s, &live(vec![m], 4)).expect("an edit of a merge is no merge");
        let bad = live(vec![m, z], 3);
        assert!(matches!(check_parents(&s, &bad), Err(Error::Invalid(_))));
        let missing = Hash([7; 32]);
        let r = check_parents(&s, &live(vec![x, missing], 1));
        assert!(matches!(r, Err(Error::NotFound(id)) if id == missing));
    }
}
