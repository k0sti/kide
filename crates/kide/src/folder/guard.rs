//! The scan's guard (§5.5): a folder that is missing, emptied or has lost
//! most of its files at once is not recorded as deleted until the user
//! confirms it, restores it, resets it to a device's state or joins.

use crate::error::{Error, Result};
use crate::file::write_atomic;
use crate::hash::Hash;
use std::fs;
use std::path::Path;

/// What a scan does with files gone from the disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Deletions {
    /// Records them as tombstones unless the guard holds: the default.
    #[default]
    Guarded,
    /// Records them as tombstones: `guard = off`, or confirmed.
    Allowed,
    /// Writes them back from the store: nothing is deleted.
    Restore,
}

/// Fewest tombstones that count as a mass deletion when they are more than
/// half of the files on record but not all of them.
pub(crate) const MASS: usize = 10;

/// Refuses when the synchronised directory is missing or not a directory.
pub fn check_root(root: &Path) -> Result<()> {
    match fs::metadata(root) {
        Ok(m) if m.is_dir() => Ok(()),
        _ => Err(Error::Refused(format!(
            "{} is missing or not a directory; restore it, or reset, join or confirm",
            root.display()
        ))),
    }
}

/// Refuses `gone` tombstones out of `on_disk` live entries with a file on
/// record, while `new` files appeared at paths with none on record: when no
/// file remains, or the net loss is at least `MASS` and more than half. A
/// rename is a deletion and a creation, so it loses nothing net.
pub fn check_deletions(root: &Path, gone: usize, new: usize, on_disk: usize) -> Result<()> {
    let empty = gone > 0 && on_disk + new <= gone;
    let lost = gone.saturating_sub(new);
    if !empty && !(lost >= MASS && 2 * lost > on_disk) {
        return Ok(());
    }
    Err(Error::Refused(format!(
        "{gone} of {on_disk} files are gone from {}; kide confirm records the deletions, \
         kide restore writes them back",
        root.display()
    )))
}

/// `.kide/confirm`: the deletions of the next scan at `head` are wanted.
pub fn confirm(kide: &Path, head: &Hash) -> Result<()> {
    write_atomic(&kide.join("confirm"), format!("{head}\n").as_bytes(), true)
}

/// Whether `.kide/confirm` confirms the deletions of a scan at `head`.
pub fn confirmed(kide: &Path, head: &Hash) -> bool {
    fs::read_to_string(kide.join("confirm")).is_ok_and(|s| s.trim() == head.to_hex())
}

/// Drops a confirmation once a scan has used it, or once `HEAD` moved on.
pub fn clear(kide: &Path) -> Result<()> {
    match fs::remove_file(kide.join("confirm")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}
