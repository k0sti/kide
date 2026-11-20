//! The walk over a plain folder (§5.5 step 1), with the NFC rule of §5.3:
//! a name not in NFC is renamed on disk to its NFC form, unless that name is
//! taken by another file or the folder is read-only; then it is skipped.

use crate::error::Result;
use crate::format::manifest::Kind;
use crate::format::path::{check_path, valid_component};
use crate::iface::Meta;
use crate::log;
use crate::names::nfc;
use crate::report::{Why, report};
use crate::time::to_ns;
use std::fs::{self, Metadata};
use std::io::{self, ErrorKind};
use std::path::Path;

/// Every listable path below `root`, with its metadata; with `rename`,
/// names are brought to NFC on disk.
pub(super) fn walk(root: &Path, rename: bool) -> Result<Vec<(String, Meta)>> {
    let mut out = Vec::new();
    walk_dir(root, "", rename, &mut out)?;
    Ok(out)
}

fn walk_dir(dir: &Path, prefix: &str, rename: bool, out: &mut Vec<(String, Meta)>) -> Result<()> {
    // Collected first: the NFC rule renames entries of this directory.
    let names = fs::read_dir(dir)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    for raw in names {
        let Some(orig) = raw.to_str() else {
            let lossy = raw.to_string_lossy();
            let path = if prefix.is_empty() {
                lossy.to_string()
            } else {
                format!("{prefix}/{lossy}")
            };
            report(Why::Skipped, &path, "the name is not UTF-8");
            continue;
        };
        let top = prefix.is_empty();
        if top && orig.eq_ignore_ascii_case(".kide") {
            continue;
        }
        let name = nfc(orig);
        let path = if top {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if !valid_component(&name, top) || check_path(&path).is_err() {
            report(Why::Skipped, &path, "an invalid or reserved name");
            continue;
        }
        if name != orig && !rename {
            let raw = match prefix {
                "" => orig.to_string(),
                _ => format!("{prefix}/{orig}"),
            };
            report(Why::Skipped, &raw, "not in NFC, in a read-only folder");
            continue;
        }
        if name != orig && !to_nfc(dir, orig, &name, prefix)? {
            continue;
        }
        let full = dir.join(&*name);
        let m = match fs::symlink_metadata(&full) {
            Err(e) if e.kind() == ErrorKind::NotFound => continue, // just removed
            r => r?,
        };
        if !m.is_dir() {
            out.extend(meta(&full, &m)?.map(|meta| (path, meta)));
        } else if is_empty(&full)? {
            out.push((format!("{path}/"), dir_meta(&m)));
        } else {
            walk_dir(&full, &path, rename, out)?;
        }
    }
    Ok(())
}

/// Gives the file `raw` its NFC name `want` on disk where the file system
/// keeps both forms apart; false if another file has the NFC name, and
/// then the raw name, below `prefix`, is skipped and reported.
fn to_nfc(dir: &Path, raw: &str, want: &str, prefix: &str) -> Result<bool> {
    let (from, to) = (dir.join(raw), dir.join(want));
    match fs::symlink_metadata(&to) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            fs::rename(&from, &to)?;
            log!("renamed {raw:?} to its NFC form {want:?}");
            Ok(true)
        }
        Ok(t) => {
            let f = fs::symlink_metadata(&from)?;
            // The same file: the file system conflates normalisation forms.
            let same = crate::os::same_file(&f, &t);
            if !same {
                let why = format!("its NFC form {want:?} is taken");
                let path = match prefix {
                    "" => raw.to_string(),
                    _ => format!("{prefix}/{raw}"),
                };
                report(Why::Skipped, &path, &why);
            }
            Ok(same)
        }
        Err(e) => Err(e.into()),
    }
}

pub(super) fn is_empty(dir: &Path) -> Result<bool> {
    Ok(fs::read_dir(dir)?.next().is_none())
}

/// The metadata of the regular file or symbolic link at `full`; `None` for
/// anything else. A link's size is the length of its target.
pub(super) fn meta(full: &Path, m: &Metadata) -> io::Result<Option<Meta>> {
    let t = m.file_type();
    let kind = match () {
        _ if t.is_symlink() => Kind::Symlink,
        _ if t.is_file() && crate::os::executable(m) => Kind::Executable,
        _ if t.is_file() => Kind::Regular,
        _ => return Ok(None),
    };
    let size = match kind {
        Kind::Symlink => crate::os::link_len(full, m)?,
        _ => m.len(),
    };
    Ok(Some(Meta {
        kind,
        size,
        mtime: m.modified().map_or(0, to_ns),
        inode: crate::os::inode(m),
    }))
}

/// The metadata of an empty directory: no content.
pub(super) fn dir_meta(m: &Metadata) -> Meta {
    Meta {
        kind: Kind::Directory,
        size: 0,
        mtime: m.modified().map_or(0, to_ns),
        inode: crate::os::inode(m),
    }
}
