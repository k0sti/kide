//! Restoring an earlier tree as new forward records (§5.2).
use super::{
    Folder,
    apply::{ApplyReport, apply},
    plan::{Plan, Target},
};
use crate::error::{Result, invalid};
use crate::format::{change::Change, manifest::Kind, record::Record};
use crate::hash::Hash;
use crate::iface::Workspace;
use crate::load::{self, ContentReader};
use std::collections::BTreeMap;

/// Explicitly replaces the current snapshot with `earlier`. The caller must
/// have reviewed and accepted replacement of changes through `expected`.
/// Refuses stale or unscanned state. Concurrent disk edits during application
/// survive and appear in `skipped`; inspect that before reporting completion.
pub fn restore_tree(
    folder: &mut Folder,
    ws: &dyn Workspace,
    earlier: &Hash,
    expected: &Hash,
) -> Result<ApplyReport> {
    folder.writable()?;
    if folder.config.mode == super::config::Mode::Send {
        return invalid("a send folder does not write into the workspace");
    }
    if folder.head != *expected {
        return invalid("restore expectation is stale");
    }
    super::moves::scanned(folder, ws)?;
    let entries = load::validate_manifest(&*folder.store, earlier)?;
    let old: BTreeMap<_, _> = entries.into_iter().map(|e| (e.path.clone(), e)).collect();
    let mut target = Plan::keep(&folder.index, Default::default());
    let mut paths: Vec<_> = old.keys().chain(folder.index.0.keys()).cloned().collect();
    paths.sort();
    paths.dedup();
    let metadata = Change::Restore(*earlier).encode()?;
    for path in paths {
        let current = folder.index.0.get(&path).copied();
        let wanted = old.get(&path);
        let next = wanted
            .map(|e| load::record(&*folder.store, &e.version))
            .transpose()?;
        let live = next.as_ref().is_some_and(|r| !r.is_tombstone());
        if !live && current.is_none_or(|e| !e.is_live()) {
            continue;
        }
        if let (Some(c), Some(w)) = (current, wanted) {
            let now = load::record(&*folder.store, &c.version)?;
            if next.as_ref().is_some_and(|r| r.value() == now.value()) && c.mtime == w.mtime {
                continue;
            }
        }
        if folder.config.ignores(&path) || current.is_some_and(|e| e.is_live() && e.disk.is_none())
        {
            return invalid("restore would replace an ignored or held entry");
        }
        let parent = current
            .map(|e| e.version)
            .or_else(|| wanted.map(|e| e.version));
        let mut r = if live {
            // `live` guarantees a record. Avoid discarding older lineage for
            // paths absent from the current snapshot.
            next.ok_or_else(|| crate::error::Error::Invalid("missing restore record".into()))?
        } else {
            Record::tombstone(Vec::new())
        };
        r.parents = parent.into_iter().collect();
        r.change = metadata.clone();
        let kind = Kind::of(&r)?;
        if kind.is_some() {
            // Read every wanted object before the first disk write.
            std::io::copy(
                &mut ContentReader::open(&*folder.store, &r.root)?,
                &mut std::io::sink(),
            )?;
        }
        let id = folder.store.put(&r.encode()?)?;
        let (size, mtime) = if live {
            wanted.map(|e| (e.size, e.mtime)).unwrap_or_default()
        } else {
            (0, 0)
        };
        target.target.insert(
            path,
            Target {
                version: id,
                kind,
                size,
                mtime,
            },
        );
    }
    // A snapshot may introduce names this device cannot represent. Refuse
    // rather than claiming a whole-tree restore that only held those names.
    let (held, _) = crate::names::held(
        target
            .target
            .iter()
            .filter(|(_, t)| t.kind.is_some())
            .map(|(p, _)| (p.as_str(), false)),
        &crate::names::Platform::current(),
    );
    if !held.is_empty() {
        return invalid("restored tree cannot be represented on this device");
    }
    folder.store.sync()?;
    apply(folder, ws, &target)
}
