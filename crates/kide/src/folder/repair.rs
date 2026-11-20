//! Repair (§4.1): after the store dropped a corrupt object, objects that
//! link to it are stored without their whole closure, which a pull's walk
//! trusts. A repair walks the whole closure of the given roots, stored
//! objects included, and fetches from a peer whatever is absent.

use super::Folder;
use super::net::Manifests;
use crate::error::Result;
use crate::fetch::{Request, Walk};
use crate::hash::Hash;
use crate::iface::Session;
use crate::log;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepairReport {
    /// Objects fetched from the peer.
    pub fetched: usize,
    /// Objects absent here that the peer lacks too.
    pub absent: usize,
}

/// Walks the closures of `roots` and fetches what is absent from `session`,
/// then clears the damage mark: what the peer lacks too is reported, and a
/// pull that needs it fails with `NotFound` as before.
pub fn repair(folder: &Folder, session: &mut dyn Session, roots: &[Hash]) -> Result<RepairReport> {
    let store = folder.store.clone();
    let staging = folder.staging()?;
    let all = |_: &str| true;
    let mut r = RepairReport::default();
    for root in roots {
        let mut w = Walk::partial(&*store, &staging, &Manifests, root, &all);
        while let Some(req) = w.request()? {
            match req {
                Request::Get(ids) => w.answer(session.get(&ids)?)?,
                Request::GetChildren(nodes) => w.answer(session.get_children(&nodes)?)?,
                Request::Fetch { .. } => w.pushed(Vec::new())?, // not asked for
            }
        }
        let f = w.commit()?;
        r.fetched += f.committed;
        r.absent += f.absent;
    }
    log!("repair: fetched {}, absent {}", r.fetched, r.absent);
    folder.repaired()?;
    Ok(r)
}

/// The roots a folder keeps: `HEAD` and the retained roots (§5.7).
pub fn kept(folder: &Folder) -> Result<Vec<Hash>> {
    let mut roots = vec![folder.head];
    for (_, root) in folder.roots()? {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    Ok(roots)
}
