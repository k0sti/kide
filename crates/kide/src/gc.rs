//! Garbage collection (§4.1): mark what a profile's roots need, remove every
//! other object, then compact and sync the store. The profile says what its
//! own objects link to (`walk::Profile`); the core does not know folders.

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::iface::ObjectStore;
use crate::iface::Profile;
use crate::walk::{Kind, children};
use std::collections::HashSet;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Collected {
    pub kept: usize,
    pub removed: usize,
}

/// Every object the roots need: what each root links to through the
/// profile, every version record it names with that record's content
/// closure, the leaves of those that are merges with theirs, and the history
/// of every one of them (§4.1, §5.7). Errors pass through unchanged: a
/// missing object stops the mark, unless the trees are `partial`, where it
/// is absent and nothing below it is marked through it.
pub(crate) fn mark(
    store: &dyn ObjectStore,
    profile: &dyn Profile,
    roots: &[Hash],
    partial: bool,
) -> Result<HashSet<Hash>> {
    let mut marked = HashSet::new();
    let mut seen = HashSet::new();
    let root = Kind::from(profile.root());
    let mut todo: Vec<(Hash, Kind)> = roots.iter().map(|r| (*r, root)).collect();
    while let Some((id, kind)) = todo.pop() {
        if !seen.insert((id, kind)) {
            continue;
        }
        marked.insert(id);
        if kind.is_leaf() {
            continue; // kept unread
        }
        let bytes = match store.get(&id) {
            Err(Error::NotFound(_)) if partial => continue,
            r => r?,
        };
        let below = children(profile, &id, kind, &bytes)?;
        todo.extend(below.into_iter().map(|c| (c.id, c.kind)));
    }
    Ok(marked)
}

/// Marks from `roots`, removes every unmarked object, syncs, compacts and
/// syncs the store. Nothing is removed if the mark fails. With `partial`
/// trees, an object below an absent one is removed.
pub fn collect(
    store: &dyn ObjectStore,
    profile: &dyn Profile,
    roots: &[Hash],
    partial: bool,
) -> Result<Collected> {
    let keep = mark(store, profile, roots, partial)?;
    let mut c = Collected::default();
    for id in store.ids()? {
        if keep.contains(&id) {
            c.kept += 1;
        } else {
            store.remove(&id)?;
            c.removed += 1;
        }
    }
    store.sync()?;
    store.compact()?;
    store.sync()?;
    Ok(c)
}

#[cfg(test)]
mod tests;
