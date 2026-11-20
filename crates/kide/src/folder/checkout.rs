//! Writing a folder from a root into a workspace, as for an empty folder.

use crate::error::Result;
use crate::format::manifest::Kind;
use crate::hash::Hash;
use crate::iface::{ObjectStore, Workspace};
use crate::load::{self, ContentReader, validate_manifest};

/// Validates the manifest of `root` (§5.1), then writes every live entry
/// with its kind and synchronised mtime; returns the number written.
pub fn checkout(store: &dyn ObjectStore, root: &Hash, ws: &dyn Workspace) -> Result<usize> {
    let mut n = 0;
    for e in validate_manifest(store, root)? {
        let r = load::record(store, &e.version)?;
        let Some(kind) = Kind::of(&r)? else {
            continue;
        };
        let mut content = ContentReader::open(store, &r.root)?;
        ws.write(&e.path, kind, &mut content, e.mtime)?;
        n += 1;
    }
    Ok(n)
}
