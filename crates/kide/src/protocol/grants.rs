//! Read rights by session grants (§4.2): a session may be served only ids
//! it reached through readable paths from the root it was sent. Grants grow
//! as objects are served; a request for an id not yet granted widens them
//! layer by layer from what was granted but not read, until the id is
//! found or nothing readable is left.

use super::ReadRule;
use crate::auth::PeerKey;
use crate::hash::Hash;
use crate::iface::ObjectStore;
use crate::iface::Profile;
use crate::log;
use crate::walk::{Kind, children, join};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// One way an object was granted: its kind and its path.
type Grant = (Kind, String);

pub(super) struct Grants {
    profile: Option<Arc<dyn Profile>>,
    may: Arc<ReadRule>,
    peer: Option<PeerKey>,
    /// Every grant, and whether its children are granted yet.
    granted: HashMap<Hash, Vec<(Grant, bool)>>,
    /// Grants whose children are not granted yet, oldest first.
    frontier: VecDeque<(Hash, Grant)>,
}

impl Grants {
    /// Grants for a session sent `root`, following the links of `profile`;
    /// without one, only the root is granted.
    pub(super) fn new(
        profile: Option<Arc<dyn Profile>>,
        may: Arc<ReadRule>,
        peer: Option<PeerKey>,
        root: Hash,
    ) -> Grants {
        let kind = profile.as_ref().map_or(Kind::Object, |p| p.root().into());
        let mut g = Grants {
            profile,
            may,
            peer,
            granted: HashMap::new(),
            frontier: VecDeque::new(),
        };
        g.grant(root, (kind, String::new()));
        g
    }

    /// Whether `id` may be served: granted, or found below what was granted.
    pub(super) fn allows(&mut self, store: &dyn ObjectStore, id: &Hash) -> bool {
        while !self.granted.contains_key(id) {
            let Some((at, grant)) = self.frontier.pop_front() else {
                return false;
            };
            let gs = self.granted.get(&at).into_iter().flatten();
            if gs.into_iter().any(|g| g.0 == grant && g.1) {
                continue; // widened when it was served
            }
            if grant.0.is_leaf() {
                // A content chunk links to nothing: not worth reading.
                self.mark(&at, &grant);
                continue;
            }
            match store.get(&at) {
                Ok(bytes) => self.widen(&at, &grant, &bytes),
                Err(e) => log!("read rights below {at}: {e}"),
            }
        }
        true
    }

    /// Whether `id` is granted already.
    pub(super) fn has(&self, id: &Hash) -> bool {
        self.granted.contains_key(id)
    }

    /// Marks the children of `id` under `grant` as granted; false if they
    /// were already, or `id` has no such grant.
    fn mark(&mut self, id: &Hash, grant: &Grant) -> bool {
        let g = self.granted.get_mut(id);
        match g.and_then(|gs| gs.iter_mut().find(|g| g.0 == *grant)) {
            Some(g) if !g.1 => {
                g.1 = true;
                true
            }
            _ => false,
        }
    }

    /// Grants the readable children of `id`, served as `bytes`.
    pub(super) fn served(&mut self, id: &Hash, bytes: &[u8]) {
        let grants = self.granted.get(id).into_iter().flatten();
        let open: Vec<Grant> = grants.filter(|g| !g.1).map(|g| g.0.clone()).collect();
        for grant in open {
            self.widen(id, &grant, bytes);
        }
    }

    fn grant(&mut self, id: Hash, grant: Grant) {
        let grants = self.granted.entry(id).or_default();
        if grants.iter().all(|g| g.0 != grant) {
            grants.push((grant.clone(), false));
            self.frontier.push_back((id, grant));
        }
    }

    /// Grants the children of `id` under `grant` whose path is readable; a
    /// labelled link asks the rule, an unlabelled one keeps its parent's path.
    fn widen(&mut self, id: &Hash, grant: &Grant, bytes: &[u8]) {
        if !self.mark(id, grant) {
            return;
        }
        let (kind, path) = grant;
        let Some(profile) = &self.profile else {
            return;
        };
        let below = match children(&**profile, id, *kind, bytes) {
            Ok(c) => c,
            Err(e) => return log!("read rights below {id}: {e}"),
        };
        for c in below {
            let (path, ask) = match &c.label {
                Some(label) => (join(path, label), true),
                None => (path.clone(), false),
            };
            if ask && !(self.may)(self.peer.as_ref(), &path) {
                continue;
            }
            self.grant(c.id, (c.kind, path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::Rng;
    use crate::error::Result;
    use crate::format::content::write_content;
    use crate::format::record::Record;
    use crate::store::MemStore;
    use crate::walk::Versions;
    use std::sync::Mutex;

    /// A store that remembers which ids were read.
    #[derive(Default)]
    struct Counting(MemStore, Mutex<Vec<Hash>>);

    impl ObjectStore for Counting {
        fn has(&self, id: &Hash) -> Result<bool> {
            self.0.has(id)
        }
        fn get(&self, id: &Hash) -> Result<Vec<u8>> {
            self.1.lock().unwrap().push(*id);
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
    fn widening_never_reads_content_chunks() {
        let store = Counting::default();
        let data = Rng::new(3).bytes(64 << 10);
        let mut written = Vec::new();
        let root = write_content(&data, &mut |id, b| {
            written.push(*id);
            store.put_known(id, b)
        })
        .unwrap();
        let is_node = |id: &Hash| crate::load::node(&store.0, id).is_ok();
        let chunks: Vec<Hash> = written.into_iter().filter(|id| !is_node(id)).collect();
        let record = Record {
            change: Vec::new(),
            parents: vec![],
            root: root.id,
            state: vec![],
        };
        let top = store.put(&record.encode().unwrap()).unwrap();
        store.1.lock().unwrap().clear();
        let all = Arc::new(|_: Option<&PeerKey>, _: &str| true);
        let mut g = Grants::new(Some(Arc::new(Versions)), all, None, top);
        assert!(!g.allows(&store, &Hash([9; 32])), "an id it lacks");
        let read = store.1.lock().unwrap().clone();
        assert!(read.contains(&root.id), "the content's node is read");
        assert!(chunks.len() > 1, "the content has chunks");
        assert!(
            chunks.iter().all(|c| !read.contains(c)),
            "no chunk is read: {} of {}",
            chunks.iter().filter(|c| read.contains(c)).count(),
            chunks.len()
        );
    }
}
