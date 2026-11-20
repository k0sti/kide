//! Deciding a pull (§5.6): every differing path through the resolver, then
//! the conflict copies, each decided at its path, the name clashes, the
//! empty directories that gained content, and what this device holds
//! (§5.3). The result is the folder's next manifest; `apply` brings the
//! disk to it. Merge, copy and tombstone records are stored as they are made.

use super::compare::compare;
use super::index::{Entry, Index};
use crate::error::{Error, Result};
use crate::format::content::write_content;
use crate::format::manifest::{Kind, PathEntry};
use crate::format::path::{check_path, conflict_path, is_dir_path};
use crate::format::record::Record;
use crate::hash::Hash;
use crate::iface::{ConflictCopy, ObjectStore, Outcome, Resolver, Side};
use crate::load::{self, read_content};
use crate::log;
use crate::names::{Platform, held};
use crate::report::{Why, report};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

/// A path's entry in the next manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub version: Hash,
    /// `None` for a tombstone.
    pub kind: Option<Kind>,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Every path of the next manifest.
    pub target: BTreeMap<String, Target>,
    /// Clash moves: a local file that gives way to a directory is renamed
    /// to its conflict-copy path, whose target adopted the copy's record.
    pub moves: Vec<(String, String)>,
    /// The live target paths this device holds (§5.3).
    pub held: BTreeSet<String>,
    /// Paths the resolver decided.
    pub decided: usize,
    /// Paths whose result is a new text merge (§5.6), to report.
    pub merged: Vec<String>,
}

impl Plan {
    /// The paths whose target differs from `index`'s entry: what applying
    /// would change, each with its target (`None`: the path goes away).
    pub fn changes<'a>(&'a self, index: &'a Index) -> Vec<(&'a str, Option<&'a Target>)> {
        let mut out: Vec<(&str, Option<&Target>)> = Vec::new();
        for (path, t) in &self.target {
            let same = index.0.get(path).is_some_and(|e| {
                (e.version, e.kind, e.size, e.mtime) == (t.version, t.kind, t.size, t.mtime)
            });
            if !same {
                out.push((path, Some(t)));
            }
        }
        for path in index.0.keys().filter(|p| !self.target.contains_key(*p)) {
            out.push((path, None));
        }
        out
    }

    /// Refuses the change at `path`: it keeps `index`'s entry, and any clash
    /// move from or to it is dropped.
    pub fn refuse(&mut self, path: &str, index: &Index) {
        match index.0.get(path) {
            Some(e) => {
                let t = Target {
                    version: e.version,
                    kind: e.kind,
                    size: e.size,
                    mtime: e.mtime,
                };
                self.target.insert(path.to_string(), t);
            }
            None => {
                self.target.remove(path);
            }
        }
        self.moves.retain(|(a, b)| a != path && b != path);
    }

    /// The plan that keeps every entry of `index`: applied, it writes only
    /// the live entries with no file on record that are not in `held`.
    pub fn keep(index: &Index, held: BTreeSet<String>) -> Plan {
        Plan {
            target: targets(index),
            held,
            ..Plan::default()
        }
    }
}

/// The entries of `index` as targets.
fn targets(index: &Index) -> BTreeMap<String, Target> {
    let target = |e: &Entry| Target {
        version: e.version,
        kind: e.kind,
        size: e.size,
        mtime: e.mtime,
    };
    index
        .0
        .iter()
        .map(|(p, e)| (p.clone(), target(e)))
        .collect()
}

/// What a device holds among `live` paths, each with whether it is already
/// on disk (§5.3); reports them and any on-disk collisions.
pub fn holds<'a>(live: impl Iterator<Item = (&'a str, bool)>, p: &Platform) -> BTreeSet<String> {
    let (held, mismatch) = held(live, p);
    for path in &held {
        report(Why::Held, path, "this device cannot represent it on disk");
    }
    for path in &mismatch {
        report(
            Why::Mismatch,
            path,
            "collides with on-disk entry; platform setting is wrong",
        );
    }
    held
}

/// Decides every path whose entries differ between the manifests `local`
/// (equal to `index`) and `remote`, both stored and validated.
pub fn plan(
    store: &dyn ObjectStore,
    resolver: &dyn Resolver,
    index: &Index,
    local: &Hash,
    remote: &Hash,
    platform: &Platform,
) -> Result<Plan> {
    let mut p = Planner {
        store,
        resolver,
        target: targets(index),
        moves: Vec::new(),
        merged: Vec::new(),
    };
    let mut copies = Vec::new();
    let differ = compare(store, local, remote)?;
    let side = |e: &PathEntry| (e.version, e.mtime);
    for d in &differ {
        let (l, r) = (d.local.as_ref().map(side), d.remote.as_ref().map(side));
        copies.extend(p.decide(&d.path, l, r)?);
    }
    for c in copies {
        p.place(c)?;
    }
    p.clashes(index)?;
    p.filled_dirs()?;
    let on_disk = |path: &str| index.0.get(path).is_some_and(|e| e.disk.is_some());
    let live = p.target.iter().filter(|(_, t)| t.kind.is_some());
    Ok(Plan {
        held: holds(
            live.map(|(path, _)| (path.as_str(), on_disk(path))),
            platform,
        ),
        target: p.target,
        moves: p.moves,
        decided: differ.len(),
        merged: p.merged,
    })
}

struct Planner<'a> {
    store: &'a dyn ObjectStore,
    resolver: &'a dyn Resolver,
    target: BTreeMap<String, Target>,
    moves: Vec<(String, String)>,
    merged: Vec<String>,
}

/// One side of a path: its version and mtime.
type At = (Hash, i64);

impl Planner<'_> {
    fn side(&self, (id, mtime): At) -> Result<Side> {
        Ok(Side {
            id,
            record: load::record(self.store, &id)?,
            mtime,
        })
    }

    /// Decides one path; returns its conflict copies.
    fn decide(
        &mut self,
        path: &str,
        local: Option<At>,
        remote: Option<At>,
    ) -> Result<Vec<ConflictCopy>> {
        let l = local.map(|e| self.side(e)).transpose()?;
        let r = remote.map(|e| self.side(e)).transpose()?;
        let (l, r) = (l.as_ref(), r.as_ref());
        let store = self.store;
        let mut history = |ids: &[Hash]| ids.iter().map(|x| load::record(store, x)).collect();
        let mut content = |root: &Hash, max: u64| match load::node(store, root)?.bytes() > max {
            true => Ok(None),
            false => read_content(store, root).map(Some),
        };
        let d = self
            .resolver
            .decide(path, l, r, &mut history, &mut content)?;
        if let Some(text) = &d.content {
            // A text merge's value: stored before its record (§4.1).
            write_content(text, &mut |id, b| store.put_known(id, b))?;
            self.merged.push(path.to_string());
        }
        log!("decide {path:?}: {:?} at {}", d.outcome, d.mtime);
        let (id, record) = match d.outcome {
            Outcome::Keep => match l {
                Some(l) => (l.id, l.record.clone()),
                None => return Err(Error::Invalid(format!("{path:?}: kept, but no local side"))),
            },
            Outcome::Adopt(id) => match r.filter(|r| r.id == id) {
                Some(r) => (id, r.record.clone()),
                None => (id, load::record(store, &id)?),
            },
            Outcome::Merge(m) => (store.put(&m.encode()?)?, m),
        };
        self.set(path, id, &record, d.mtime)?;
        Ok(d.copies)
    }

    fn set(&mut self, path: &str, version: Hash, r: &Record, mtime: i64) -> Result<()> {
        let kind = Kind::of(r)?;
        let size = match kind {
            Some(_) => load::node(self.store, &r.root)?.bytes(),
            None => 0,
        };
        let t = Target {
            version,
            kind,
            size,
            mtime,
        };
        self.target.insert(path.to_string(), t);
        Ok(())
    }

    /// Decides a conflict copy at its path like a remote record, against
    /// whatever that path holds, and places that decision's own copies
    /// likewise (§5.6); returns whether the path adopted the copy. A copy
    /// whose path would be invalid is not made: the loser stays in history.
    fn place(&mut self, c: ConflictCopy) -> Result<bool> {
        if check_path(&c.path).is_err() {
            let of = c.record.parents.first().map_or(String::new(), Hash::to_hex);
            let why = format!("no conflict copy of {of}: its path would be invalid");
            report(Why::NotCopied, &c.path, &why);
            return Ok(false);
        }
        let id = self.store.put(&c.record.encode()?)?;
        let there = self.target.get(&c.path).map(|t| (t.version, t.mtime));
        for further in self.decide(&c.path, there, Some((id, c.mtime)))? {
            self.place(further)?;
        }
        Ok(self.target.get(&c.path).is_some_and(|t| t.version == id))
    }

    /// Whether a live entry lies below `path` (a file's, or an empty
    /// directory's other than its own).
    fn live_below(&self, path: &str) -> bool {
        let prefix = match is_dir_path(path) {
            true => path.to_string(),
            false => format!("{path}/"),
        };
        self.target
            .range(prefix.clone()..)
            .take_while(|(p, _)| p.starts_with(&prefix))
            .any(|(p, t)| p != path && t.kind.is_some())
    }

    /// A live file or link with live entries below it gives way: the
    /// directory keeps the path, which takes the tombstone (tombstone, [its
    /// id]), and the record (its value, [its id]) is decided at its
    /// conflict-copy path. A local file whose value is on disk is renamed
    /// there if that path adopts the copy; otherwise it goes to the trash.
    /// Tombstones take no part.
    fn clashes(&mut self, index: &Index) -> Result<()> {
        let files: Vec<(String, Target)> = self
            .target
            .iter()
            .filter(|(_, t)| t.kind.is_some_and(|k| k != Kind::Directory))
            .map(|(p, t)| (p.clone(), *t))
            .collect();
        for (path, t) in files {
            if !self.live_below(&path) {
                continue;
            }
            let tomb = Record::tombstone(vec![t.version]);
            let id = self.store.put(&tomb.encode()?)?;
            self.set(&path, id, &tomb, 0)?;
            let r = load::record(self.store, &t.version)?;
            let copy = ConflictCopy {
                path: conflict_path(&path, &t.version),
                record: Record {
                    change: Vec::new(),
                    parents: vec![t.version],
                    root: r.root,
                    state: r.state.clone(),
                },
                mtime: t.mtime,
            };
            let to = copy.path.clone();
            log!("clash at {path:?}: the file's record goes to {to:?}");
            if self.place(copy)? && self.on_disk(index, &path, &r)? {
                self.moves.push((path, to));
            }
        }
        Ok(())
    }

    /// Whether the index records a file at `path` with the value of `r`.
    fn on_disk(&self, index: &Index, path: &str, r: &Record) -> Result<bool> {
        let Some(e) = index.0.get(path).filter(|e| e.disk.is_some()) else {
            return Ok(false);
        };
        let mine = load::record(self.store, &e.version)?;
        Ok(mine.value() == r.value())
    }

    /// An empty directory with live entries below it is no longer empty:
    /// its entry becomes a tombstone, as a scan would make it (§5.1).
    fn filled_dirs(&mut self) -> Result<()> {
        let dirs: Vec<(String, Hash)> = self
            .target
            .iter()
            .filter(|(p, t)| t.kind == Some(Kind::Directory) && self.live_below(p))
            .map(|(p, t)| (p.clone(), t.version))
            .collect();
        for (path, v) in dirs {
            let t = Record::tombstone(vec![v]);
            let id = self.store.put(&t.encode()?)?;
            self.set(&path, id, &t, 0)?;
        }
        Ok(())
    }
}

/// Whether `x` is an ancestor of `of`: breadth-first over parents in the
/// store (§3.2).
pub fn is_ancestor(store: &dyn ObjectStore, x: &Hash, of: &Hash) -> Result<bool> {
    let (mut todo, mut seen) = (VecDeque::from([*of]), HashSet::new());
    while let Some(id) = todo.pop_front() {
        for p in load::record(store, &id)?.parents {
            if p == *x {
                return Ok(true);
            }
            if seen.insert(p) {
                todo.push_back(p);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests;
