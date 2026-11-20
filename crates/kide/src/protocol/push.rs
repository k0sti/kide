//! The answer to a Fetch (§4.3): what `want` reaches and the `have`s do
//! not. Both are walked side by side, layer by layer, along the profile's
//! links, within the scope and the session's grants; neither side descends
//! into an id both reach. Objects come one per call, up to `MAX_PUSH` bytes,
//! so a push never sits in memory.

use super::ScopeRule;
use super::grants::Grants;
use crate::hash::Hash;
use crate::iface::Profile;
use crate::iface::{MAX_FRAME, MAX_PUSH, ObjectStore};
use crate::walk::{Child, Kind, children, join};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

/// An object to walk: its id, its kind and its path.
type Item = (Hash, Kind, String);

pub(super) struct Push {
    profile: Arc<dyn Profile>,
    rule: Option<Arc<ScopeRule>>,
    scope: Vec<u8>,
    want: Hash,
    /// This layer of the want side, and the next.
    todo: VecDeque<Item>,
    next: Vec<Item>,
    /// The have side's layer not yet marked.
    have: Vec<Item>,
    /// Every id the have side reached.
    had: HashSet<Hash>,
    /// Every id the want side reached.
    wanted: HashSet<Hash>,
    seen: HashSet<Item>,
    sent: HashSet<Hash>,
    bytes: u64,
}

impl Push {
    pub(super) fn new(
        profile: Arc<dyn Profile>,
        rule: Option<Arc<ScopeRule>>,
        want: Hash,
        have: Vec<Hash>,
        scope: Vec<u8>,
    ) -> Push {
        let root = Kind::from(profile.root());
        let item = |id| (id, root, String::new());
        Push {
            profile,
            rule,
            scope,
            want,
            todo: VecDeque::new(),
            next: vec![item(want)],
            have: have.into_iter().map(item).collect(),
            had: HashSet::new(),
            wanted: HashSet::from([want]),
            seen: HashSet::from([item(want)]),
            sent: HashSet::new(),
            bytes: 0,
        }
    }

    /// The next object to push, or `None` when the push is over. Objects
    /// the server lacks, cannot read or that do not validate are skipped.
    pub(super) fn next(
        &mut self,
        store: &dyn ObjectStore,
        mut grants: Option<&mut Grants>,
    ) -> Option<(Hash, Vec<u8>)> {
        loop {
            let Some((id, kind, path)) = self.todo.pop_front() else {
                if self.next.is_empty() {
                    return None;
                }
                self.advance(store);
                continue;
            };
            if self.had.contains(&id) {
                continue;
            }
            if let Some(g) = grants.as_deref_mut() {
                // Below `want`, every readable object was granted when its
                // parent was walked.
                let readable = match id == self.want {
                    true => g.allows(store, &id),
                    false => g.has(&id),
                };
                if !readable {
                    continue;
                }
            }
            let Ok(bytes) = store.get(&id) else {
                continue;
            };
            let Ok(below) = children(&*self.profile, &id, kind, &bytes) else {
                continue;
            };
            for item in self.within(&path, below) {
                if self.seen.insert(item.clone()) {
                    self.wanted.insert(item.0);
                    self.next.push(item);
                }
            }
            if let Some(g) = grants.as_deref_mut() {
                g.served(&id, &bytes);
            }
            if 1 + 32 + bytes.len() > MAX_FRAME || !self.sent.insert(id) {
                continue;
            }
            self.bytes += bytes.len() as u64;
            if self.bytes > MAX_PUSH && self.bytes > bytes.len() as u64 {
                self.todo.clear(); // over: at least one object, at most the limit
                self.next.clear();
                return None;
            }
            return Some((id, bytes));
        }
    }

    /// Marks the have side's layer and walks it one layer down, except into
    /// ids the want side reached; then starts the want side's next layer.
    fn advance(&mut self, store: &dyn ObjectStore) {
        for (id, kind, path) in std::mem::take(&mut self.have) {
            if !self.had.insert(id) || self.wanted.contains(&id) {
                continue;
            }
            let Ok(bytes) = store.get(&id) else {
                continue;
            };
            if let Ok(below) = children(&*self.profile, &id, kind, &bytes) {
                let below = self.within(&path, below);
                self.have
                    .extend(below.into_iter().filter(|c| !self.had.contains(&c.0)));
            }
        }
        self.todo = std::mem::take(&mut self.next).into();
    }

    /// The children within the scope, with their paths.
    fn within(&self, path: &str, below: Vec<Child>) -> Vec<Item> {
        let in_scope = |p: &str| match &self.rule {
            Some(rule) if !self.scope.is_empty() => rule(&self.scope, p),
            _ => true,
        };
        below
            .into_iter()
            .filter_map(|c| {
                let path = match &c.label {
                    Some(label) => Some(join(path, label)).filter(|p| in_scope(p))?,
                    None => path.to_string(),
                };
                Some((c.id, c.kind, path))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::Plain;
    use crate::iface::Want;
    use crate::store::MemStore;

    #[test]
    fn a_push_stops_at_its_limit() {
        if !crate::conformance::full_tests() {
            return; // hashes 17 MiB
        }
        let store = MemStore::new();
        let link = |i: u8| (Want::Object, Hash([i; 32]));
        // 17 objects of 1 MiB less a byte, each linking to absent objects.
        let big: Vec<Hash> = (0..17)
            .map(|i| store.put(&Plain::object(&vec![link(i); 31_775])).unwrap())
            .collect();
        let links: Vec<_> = big.iter().map(|b| (Want::Object, *b)).collect();
        let root = store.put(&Plain::object(&links)).unwrap();
        let mut p = Push::new(Arc::new(Plain), None, root, vec![], vec![]);
        let pushed: Vec<_> = std::iter::from_fn(|| p.next(&store, None)).collect();
        let bytes: u64 = pushed.iter().map(|o| o.1.len() as u64).sum();
        assert!(bytes <= MAX_PUSH, "{bytes}");
        assert_eq!(pushed.len(), 1 + 15, "the root and 15 of the 17");
        assert_eq!(pushed[0].0, root, "breadth first");
        assert!(p.next(&store, None).is_none(), "over");
    }
}
