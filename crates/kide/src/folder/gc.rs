//! `kide gc` (§5.7): drops retained roots older than `keep_days`, keeps
//! what `HEAD` and the remaining roots need, removes every other object,
//! clears `staging/`, deletes trash older than `trash_days` and syncs the
//! store. Needs the lock, which a running daemon holds.

use super::Folder;
use super::net::Manifests;
use super::roots;
use crate::error::Result;
use crate::gc::collect;
use crate::iface::ObjectStore;
use std::fs;
use std::path::Path;

const DAY: u64 = 86_400;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub kept: usize,
    pub removed: usize,
    /// Retained roots dropped for their age.
    pub roots_dropped: usize,
    /// Objects of an unfinished pull, cleared from staging.
    pub unstaged: usize,
    /// Trash slots (`trash/<unix-time>[-<n>]`) deleted for their age.
    pub trash_removed: usize,
}

/// Collects garbage in `folder` at the unix time `now`.
pub fn gc(folder: &Folder, now: u64) -> Result<GcReport> {
    folder.writable()?;
    let kide = folder.kide();
    let horizon = now.saturating_sub(folder.config.keep_days * DAY);
    let (mut keep, roots_dropped) = roots::prune(&kide.join("roots"), horizon, folder.fsync)?;
    keep.push(folder.head);
    let c = collect(&*folder.store, &Manifests, &keep, false)?;
    let staging = folder.staging()?;
    let unstaged = staging.ids()?.len();
    staging.clear()?;
    let horizon = now.saturating_sub(folder.config.trash_days * DAY);
    let trash_removed = prune_trash(&kide.join("trash"), horizon)?;
    folder.store.sync()?;
    Ok(GcReport {
        kept: c.kept,
        removed: c.removed,
        roots_dropped,
        unstaged,
        trash_removed,
    })
}

/// Deletes the trash slots made before `horizon`; leaves names it did not
/// make alone.
fn prune_trash(trash: &Path, horizon: u64) -> Result<usize> {
    let mut n = 0;
    for e in fs::read_dir(trash)? {
        let e = e?;
        let name = e.file_name();
        let secs = name.to_str().and_then(slot_time);
        if secs.is_some_and(|s| s < horizon) {
            fs::remove_dir_all(e.path())?;
            n += 1;
        }
    }
    Ok(n)
}

/// The time of a slot named `<unix-time>` or `<unix-time>-<n>`.
fn slot_time(name: &str) -> Option<u64> {
    let (secs, n) = name.split_once('-').unwrap_or((name, "1"));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match digits(secs) && digits(n) {
        true => secs.parse().ok(),
        false => None,
    }
}

#[cfg(test)]
mod tests;
