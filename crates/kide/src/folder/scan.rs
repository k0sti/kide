//! The scan engine (§5.5): records local changes as version records, then
//! builds the manifest from the index, stores it and writes `HEAD`.

use super::Folder;
use super::apply::apply;
use super::guard::{self, Deletions};
use super::index::{Entry, Index};
use super::plan::{Plan, holds};
use crate::error::{Error, Result};
use crate::format::content::ContentWriter;
use crate::format::manifest::{Kind, LiveKind, build_manifest, validate};
use crate::format::node::Root;
use crate::format::record::Record;
use crate::hash::Hash;
use crate::iface::{ChangeSource, ObjectStore, Workspace};
use crate::names::{Platform, held};
use crate::{load, time};
use std::collections::HashMap;
use std::io::Read;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Version records for new, edited, retyped or re-created paths.
    pub versions: usize,
    pub tombstones: usize,
    /// Entries whose synchronised mtime changed without a new version.
    pub metadata: usize,
    /// Paths that changed while they were read; retried next scan.
    pub raced: usize,
    /// Paths whose content was read.
    pub opened: usize,
    /// Live entries this device cannot represent on disk (§5.3).
    pub held: Vec<String>,
    /// Entries that stopped being held, written to disk.
    pub written: usize,
    pub head: Hash,
}

/// The synchronised mtime of content observed with `mtime`: only regular
/// files carry one; links and directories have 0 (§5.1).
pub fn synced_mtime(kind: Kind, mtime: i64) -> i64 {
    match kind {
        Kind::Regular | Kind::Executable => mtime,
        Kind::Symlink | Kind::Directory => 0,
    }
}

/// The live entries of the index at ignored paths (§5.8): held, so that
/// they are never written, though not reported.
pub fn ignored(folder: &Folder) -> impl Iterator<Item = String> + '_ {
    let live = folder.index.0.iter().filter(|(_, e)| e.is_live());
    live.filter(|(p, _)| folder.config.ignores(p))
        .map(|(p, _)| p.clone())
}

/// Scans the folder (§5.5 steps 1 to 7). Held entries are not checked
/// against the disk; entries that stop being held are written to disk
/// (§5.3) before the manifest is built.
pub fn scan(
    folder: &mut Folder,
    ws: &dyn Workspace,
    source: &mut dyn ChangeSource,
    platform: &Platform,
) -> Result<ScanReport> {
    folder.writable()?;
    let store = folder.store.clone();
    let deletions = match folder.deletions {
        Deletions::Guarded if guard::confirmed(&folder.kide(), &folder.head) => Deletions::Allowed,
        d => d,
    };
    if deletions == Deletions::Restore {
        std::fs::create_dir_all(&folder.root)?;
    }
    if deletions == Deletions::Guarded {
        guard::check_root(&folder.root)?;
    }
    let (before, _) = held(live(&folder.index), platform);
    let checked = |(p, e): (&String, &Entry)| {
        e.is_live() && e.disk.is_some() && !before.contains(p) && !folder.config.ignores(p)
    };
    let on_disk = folder.index.0.iter().filter(|x| checked(*x)).count();
    let mut r = ScanReport::default();
    let mut next = Vec::new();
    for path in source.changed(ws, &folder.index.checked())? {
        if before.contains(&path) || folder.config.ignores(&path) {
            continue; // never checked against the disk
        }
        let old = folder.index.0.get(&path).copied();
        if let Some(e) = look(&*store, ws, &path, old, deletions, &mut r)? {
            next.push((path, e));
        }
    }
    if deletions == Deletions::Guarded {
        let had = |p: &String| folder.index.0.get(p).is_some_and(|e| checked((p, e)));
        let new = next
            .iter()
            .filter(|(p, e)| e.is_live() && e.disk.is_some() && !had(p))
            .count();
        guard::check_deletions(&folder.root, r.tombstones, new, on_disk)?;
    }
    let mut dirty = false;
    for (path, e) in next {
        dirty |= folder.index.0.get(&path) != Some(&e);
        folder.index.0.insert(path, e);
    }
    let mut held = holds(live(&folder.index), platform);
    r.held = held.iter().cloned().collect();
    held.extend(ignored(folder));
    let unheld = |(p, e): (&String, &Entry)| e.is_live() && e.disk.is_none() && !held.contains(p);
    if folder.index.0.iter().any(unheld) {
        let a = apply(folder, ws, &Plan::keep(&folder.index, held))?;
        (r.written, r.head) = (a.written, a.head);
    } else {
        r.head = write_manifest(folder, dirty)?;
    }
    if r.tombstones > 0 {
        guard::clear(&folder.kide())?; // a confirmation is used once
    }
    Ok(r)
}

/// The live paths of `index`, each with whether a file is on record.
fn live(index: &Index) -> impl Iterator<Item = (&str, bool)> {
    let live = index.0.iter().filter(|(_, e)| e.is_live());
    live.map(|(p, e)| (p.as_str(), e.disk.is_some()))
}

/// The new entry of one reported path, or `None` to keep the old one.
fn look(
    store: &dyn ObjectStore,
    ws: &dyn Workspace,
    path: &str,
    old: Option<Entry>,
    deletions: Deletions,
    r: &mut ScanReport,
) -> Result<Option<Entry>> {
    let at = time::now();
    let Some(m) = ws.stat(path)? else {
        // Step 6: a live entry whose file is gone becomes a tombstone; one
        // with no file on record has none to lose, and is written later.
        let Some(e) = old.filter(|e| e.is_live() && e.disk.is_some()) else {
            return Ok(None);
        };
        if deletions == Deletions::Restore {
            return Ok(Some(Entry { disk: None, ..e })); // written back below
        }
        r.tombstones += 1;
        let id = store.put(&Record::tombstone(vec![e.version]).encode()?)?;
        return Ok(Some(Entry::tombstone(id)));
    };
    // Step 2: a matching entry that is not racy stays.
    let cached = old.and_then(|e| e.checked());
    if cached.is_some_and(|c| c.meta == m && !c.is_racy()) {
        return Ok(None);
    }
    // Step 3: stream the content into the store; a change meanwhile waits.
    r.opened += 1;
    let root = store_content(store, &mut *ws.open(path)?)?;
    if ws.stat(path)? != Some(m) {
        r.raced += 1;
        return Ok(None);
    }
    let state = m.kind.state();
    let observed = synced_mtime(m.kind, m.mtime);
    if let Some(e) = old.filter(Entry::is_live) {
        let rec = load::record(store, &e.version)?;
        if (rec.root, rec.state.as_slice()) == (root.id, state.as_slice()) {
            // Step 4: the same value; a touch moves the synchronised mtime.
            let touched = e.disk.is_none_or(|d| d.mtime != m.mtime);
            let mtime = if touched { observed } else { e.mtime };
            r.metadata += usize::from(mtime != e.mtime);
            return Ok(Some(Entry::observed(e.version, &m, mtime, at)));
        }
    }
    // Step 5: a new version after the current record or tombstone.
    r.versions += 1;
    let record = Record {
        change: Vec::new(),
        parents: old.map(|e| vec![e.version]).unwrap_or_default(),
        root: root.id,
        state,
    };
    let id = store.put(&record.encode()?)?;
    Ok(Some(Entry::observed(id, &m, observed, at)))
}

/// Chunks content as a stream into `store` (§2.2, §2.3).
pub(crate) fn store_content(store: &dyn ObjectStore, content: &mut dyn Read) -> Result<Root> {
    let mut sink = |id: &Hash, b: &[u8]| store.put_known(id, b);
    let mut w = ContentWriter::new();
    let mut buf = vec![0; 64 << 10];
    loop {
        match content.read(&mut buf)? {
            0 => return w.finish(&mut sink),
            n => w.write(&buf[..n], &mut sink)?,
        }
    }
}

/// Step 7: builds and stores the manifest of the index, then writes the
/// index (if `dirty`) and `HEAD` (if it moved), after syncing the store.
pub fn write_manifest(folder: &mut Folder, dirty: bool) -> Result<Hash> {
    let store = folder.store.clone();
    let entries = folder.index.manifest();
    let kinds: HashMap<Hash, LiveKind> = folder
        .index
        .0
        .values()
        .map(|e| (e.version, e.kind.map(|k| (k, e.size))))
        .collect();
    // The records' kinds and sizes are the index's, so no store reads.
    validate(&entries, &mut |id| {
        kinds.get(id).copied().ok_or(Error::NotFound(*id))
    })?;
    let root = build_manifest(&entries, &mut |id, b| store.put_known(id, b))?;
    if dirty {
        store.sync()?;
        folder.save_index()?;
    }
    if root.id != folder.head {
        folder.set_head(root.id)?;
    }
    Ok(root.id)
}

#[cfg(test)]
mod tests;
