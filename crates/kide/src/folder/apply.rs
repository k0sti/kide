//! Applying a plan (§5.6 steps 1 to 6): clash moves, then deletions, then
//! writes, each only where the disk still matches the index; then the index
//! and, last, `HEAD`. A path that changed since the scan, lies below a
//! symbolic link or fails locally is skipped, keeps its index entry, and is
//! reconciled by a later scan and pull. Held entries never touch the disk.

use super::Folder;
use super::index::{Entry, Index};
use super::plan::{Plan, Target};
use super::scan::write_manifest;
use crate::error::{Error, Result};
use crate::format::content::ContentWriter;
use crate::format::manifest::Kind;
use crate::format::path::is_dir_path;
use crate::hash::Hash;
use crate::iface::{Meta, ObjectStore, Workspace};
use crate::load::{self, ContentReader};
use crate::{log, time};
use std::io::Read;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Files, links and directories written.
    pub written: usize,
    /// Paths moved to the trash or removed (empty directories).
    pub trashed: usize,
    /// Files whose mtime alone was set.
    pub metadata: usize,
    /// Files renamed to their conflict-copy path.
    pub moved: usize,
    /// Paths left for a later scan and pull.
    pub skipped: Vec<String>,
    pub head: Hash,
}

/// Brings the folder to `plan`, then writes the index and `HEAD`. An error
/// other than a local failure of one path ends applying; what was applied
/// until then is still recorded.
pub fn apply(folder: &mut Folder, ws: &dyn Workspace, plan: &Plan) -> Result<ApplyReport> {
    folder.writable()?;
    let store = folder.store.clone();
    let mut a = Applier {
        store: &*store,
        ws,
        plan,
        index: &mut folder.index,
        report: ApplyReport::default(),
    };
    let done = a.run();
    let mut report = a.report;
    report.head = write_manifest(folder, true)?;
    done.map(|()| report)
}

struct Applier<'a> {
    store: &'a dyn ObjectStore,
    ws: &'a dyn Workspace,
    plan: &'a Plan,
    index: &'a mut Index,
    report: ApplyReport,
}

/// What became of one path.
enum Done {
    /// The disk was changed, or nothing had to be: this is the new entry.
    Entry(Option<Entry>),
    /// Left as it is.
    Skipped,
}

impl Applier<'_> {
    fn run(&mut self) -> Result<()> {
        let plan = self.plan;
        for (from, to) in &plan.moves {
            self.step(from, |a| a.clash_move(from, to))?;
        }
        let paths: Vec<String> = self.index.0.keys().cloned().collect();
        for p in &paths {
            if self.target(p).is_none_or(|t| t.kind.is_none()) {
                self.step(p, |a| a.delete(p))?;
            }
        }
        for (p, t) in &plan.target {
            match t.kind {
                Some(kind) => self.step(p, |a| a.write(p, t, kind))?,
                None => self.record_tombstone(p, t),
            }
        }
        Ok(())
    }

    /// A tombstone where the index has no live entry needs no change on disk.
    fn record_tombstone(&mut self, path: &str, t: &Target) {
        if self.index.0.get(path).is_none_or(|e| !e.is_live()) {
            let e = Entry::tombstone(t.version);
            self.index.0.insert(path.to_string(), e);
        }
    }

    fn target(&self, path: &str) -> Option<&Target> {
        self.plan.target.get(path)
    }

    /// Runs one path's change; a local failure skips the path.
    fn step(&mut self, path: &str, f: impl FnOnce(&mut Self) -> Result<Done>) -> Result<()> {
        match f(self) {
            Ok(Done::Entry(Some(e))) => drop(self.index.0.insert(path.to_string(), e)),
            Ok(Done::Entry(None)) => drop(self.index.0.remove(path)),
            Ok(Done::Skipped) => self.report.skipped.push(path.to_string()),
            Err(e @ (Error::Io(_) | Error::Invalid(_))) => {
                log!("apply {path:?}: {e}");
                self.report.skipped.push(path.to_string());
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Renames the local file `from` to its conflict-copy path `to`, which
    /// gets the copy's record; the index entry at `from` goes, and its
    /// target, the clash's tombstone, is recorded with the other tombstones.
    fn clash_move(&mut self, from: &str, to: &str) -> Result<Done> {
        let (Some(e), Some(t)) = (self.index.0.get(from).copied(), self.target(to).copied()) else {
            return Ok(Done::Skipped);
        };
        let free = self.unchanged(to)?;
        if self.below_link(from)? || self.below_link(to)? || !free || !self.unchanged(from)? {
            return Ok(Done::Skipped);
        }
        let at = time::now();
        self.ws.rename(from, to)?;
        self.index.0.remove(from);
        let m = self.observed(to, &t, e.disk.map(|d| d.mtime))?;
        self.index
            .0
            .insert(to.to_string(), Entry::observed(t.version, &m, t.mtime, at));
        self.report.moved += 1;
        log!("apply: moved {from:?} to {to:?}");
        // `to` is done; `from` has no entry now.
        Ok(Done::Entry(None))
    }

    /// A live entry whose target is a tombstone, or no entry: the file goes
    /// to the trash. An empty directory that changed since the scan stays,
    /// but takes its tombstone all the same.
    fn delete(&mut self, path: &str) -> Result<Done> {
        let Some(e) = self.index.0.get(path).copied() else {
            return Ok(Done::Skipped);
        };
        let next = self.target(path).map(|t| Entry::tombstone(t.version));
        if !e.is_live() || e.disk.is_none() {
            return Ok(Done::Entry(next)); // a tombstone, or held: no file
        }
        if self.below_link(path)? {
            return Ok(Done::Skipped);
        }
        if !self.unchanged(path)? {
            // An empty directory has no content to lose: whatever is there
            // now, the next scan records it after the tombstone.
            return Ok(match is_dir_path(path) {
                true => Done::Entry(next),
                false => Done::Skipped,
            });
        }
        self.ws.trash(path)?;
        self.report.trashed += 1;
        log!("apply: trashed {path:?}");
        Ok(Done::Entry(next))
    }

    /// A live target: writes the content, or sets only the mtime when the
    /// value is already there; a held one is recorded without a file.
    fn write(&mut self, path: &str, t: &Target, kind: Kind) -> Result<Done> {
        let e = self.index.0.get(path).copied();
        if self.plan.held.contains(path) {
            let held = Entry {
                kind: Some(kind),
                version: t.version,
                size: t.size,
                mtime: t.mtime,
                disk: None,
            };
            return Ok(Done::Entry(Some(held)));
        }
        let synced = e.filter(|e| e.disk.is_some() && e.is_live());
        if synced.is_some_and(|e| (e.version, e.mtime) == (t.version, t.mtime)) {
            return Ok(Done::Entry(e));
        }
        if self.below_link(path)? || !self.unchanged(path)? {
            return Ok(Done::Skipped);
        }
        let at = time::now();
        if let Some(e) = synced
            && self.same_value(&e.version, &t.version)?
        {
            let m = self.observed(path, t, e.disk.map(|d| d.mtime))?;
            return Ok(Done::Entry(Some(Entry::observed(
                t.version, &m, t.mtime, at,
            ))));
        }
        let r = load::record(self.store, &t.version)?;
        let mut content = ContentReader::open(self.store, &r.root)?;
        // Checked again just before the file is replaced, which goes to the
        // trash (§5.6, applying step 3).
        let expect = synced.and_then(|e| e.checked()).map(|c| c.meta);
        let Some(m) = self
            .ws
            .write_if(path, kind, &mut content, t.mtime, expect)?
        else {
            return Ok(Done::Skipped);
        };
        self.report.written += 1;
        log!("apply: wrote {path:?}");
        Ok(Done::Entry(Some(Entry::observed(
            t.version, &m, t.mtime, at,
        ))))
    }

    /// The metadata of `path` once its mtime is the target's, set only for
    /// a regular file whose disk mtime differs.
    fn observed(&mut self, path: &str, t: &Target, disk: Option<i64>) -> Result<Meta> {
        let regular = matches!(t.kind, Some(Kind::Regular | Kind::Executable));
        if regular && disk != Some(t.mtime) {
            self.report.metadata += 1;
            return self.ws.set_mtime(path, t.mtime);
        }
        let gone = || Error::Io(std::io::Error::other(format!("{path:?} vanished")));
        self.ws.stat(path)?.ok_or_else(gone)
    }

    fn same_value(&self, a: &Hash, b: &Hash) -> Result<bool> {
        if a == b {
            return Ok(true);
        }
        let (a, b) = (load::record(self.store, a)?, load::record(self.store, b)?);
        Ok(a.value() == b.value())
    }

    /// Step 1: the disk still matches the index at `path`: the same size,
    /// mtime, inode and kind, and for a racy entry the same content; or
    /// nothing where the index has no live file.
    fn unchanged(&self, path: &str) -> Result<bool> {
        let disk = self.ws.stat(path)?;
        let Some(e) = self.index.0.get(path).filter(|e| e.disk.is_some()) else {
            return Ok(disk.is_none());
        };
        // A live entry: `disk` is set.
        let (Some(c), Some(m)) = (e.checked(), disk) else {
            return Ok(false);
        };
        if c.meta != m {
            return Ok(false);
        }
        if !c.is_racy() {
            return Ok(true);
        }
        let r = load::record(self.store, &e.version)?;
        Ok(content_root(&mut *self.ws.open(path)?)? == r.root && self.ws.stat(path)? == Some(m))
    }

    /// Step 5: whether a parent component of `path` is a symbolic link.
    fn below_link(&self, path: &str) -> Result<bool> {
        let body = path.strip_suffix('/').unwrap_or(path);
        for (i, _) in body.match_indices('/') {
            if self
                .ws
                .stat(&body[..i])?
                .is_some_and(|m| m.kind == Kind::Symlink)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The content root of `r`, storing nothing.
fn content_root(r: &mut dyn Read) -> Result<Hash> {
    let mut sink = |_: &Hash, _: &[u8]| Ok(());
    let mut w = ContentWriter::new();
    let mut buf = vec![0; 64 << 10];
    loop {
        match r.read(&mut buf)? {
            0 => return Ok(w.finish(&mut sink)?.id),
            n => w.write(&buf[..n], &mut sink)?,
        }
    }
}

#[cfg(test)]
mod tests;
