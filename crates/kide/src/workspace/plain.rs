//! `PlainFolder`: the user's folder as ordinary files (§5.2), beside its
//! local state in `.kide/` (§5.4), or apart from it. A read-only one (a
//! send folder, §5.8) refuses every change and renames nothing. It never follows a symbolic link, neither
//! as the last component nor as a parent: every parent is checked to be a
//! real directory first. (Without `openat` a link swapped in between the
//! check and the call can still be followed.)

use super::walk::{dir_meta, is_empty, meta, walk};
use crate::error::{Error, Result, invalid};
use crate::format::manifest::Kind;
use crate::format::path::{check_path, is_dir_path};
use crate::iface::{Meta, Workspace};
use crate::time::{from_ns, now_secs};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};

pub struct PlainFolder {
    root: PathBuf,
    trash: PathBuf,
    read_only: bool,
}

/// Where the parents of a path lead.
enum Parents {
    /// Real directories all the way.
    Dirs,
    /// A parent is missing or is a file.
    Absent,
    /// A parent is a symbolic link.
    Link,
}

impl PlainFolder {
    /// A folder whose trash is its own `.kide/trash`.
    pub fn new(root: impl Into<PathBuf>) -> PlainFolder {
        let root = root.into();
        PlainFolder::with_trash(&root, root.join(".kide/trash"))
    }

    /// A folder whose state lives elsewhere, with its trash.
    pub fn with_trash(root: impl Into<PathBuf>, trash: impl Into<PathBuf>) -> PlainFolder {
        PlainFolder {
            root: root.into(),
            trash: trash.into(),
            read_only: false,
        }
    }

    /// A folder that is only read: every change is refused with `Invalid`,
    /// and a name not in NFC is skipped and reported instead of renamed.
    pub fn read_only(root: impl Into<PathBuf>) -> PlainFolder {
        PlainFolder {
            read_only: true,
            ..PlainFolder::new(root)
        }
    }

    fn writable(&self) -> Result<()> {
        match self.read_only {
            false => Ok(()),
            true => invalid(format!("{} is read-only", self.root.display())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file-system path of a valid manifest path.
    fn full(&self, path: &str) -> Result<PathBuf> {
        check_path(path)?;
        Ok(self.root.join(path.strip_suffix('/').unwrap_or(path)))
    }

    /// Checks, and with `create` makes, the parent directories of `full`.
    fn parents(&self, full: &Path, create: bool) -> Result<Parents> {
        let Ok(rel) = full.strip_prefix(&self.root) else {
            return invalid(format!("{} is outside the folder", full.display()));
        };
        let mut dir = self.root.clone();
        let mut parts = rel.components().peekable();
        while let Some(c) = parts.next() {
            if parts.peek().is_none() {
                break; // the last component is the path itself
            }
            dir.push(c);
            let m = match fs::symlink_metadata(&dir) {
                Err(e) if create && e.kind() == ErrorKind::NotFound => match fs::create_dir(&dir) {
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => fs::symlink_metadata(&dir)?,
                    r => {
                        r?;
                        continue;
                    }
                },
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                    return Ok(Parents::Absent);
                }
                r => r?,
            };
            match () {
                _ if m.is_symlink() => return Ok(Parents::Link),
                _ if !m.is_dir() => return Ok(Parents::Absent),
                _ => {}
            }
        }
        Ok(Parents::Dirs)
    }

    /// The path of a valid manifest path whose parents are real directories;
    /// `Invalid` through a link, `Io` if a parent is missing or a file.
    fn reach(&self, path: &str, create: bool) -> Result<PathBuf> {
        let full = self.full(path)?;
        match self.parents(&full, create)? {
            Parents::Dirs => Ok(full),
            Parents::Link => invalid(format!("{path:?} lies below a symbolic link")),
            Parents::Absent if create => {
                Err(io_err(ErrorKind::NotADirectory, path, "a parent is a file"))
            }
            Parents::Absent => Err(absent(path)),
        }
    }

    /// What is at `full`, without following a link: `None` if nothing.
    fn lstat(full: &Path) -> Result<Option<fs::Metadata>> {
        match fs::symlink_metadata(full) {
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                Ok(None)
            }
            r => Ok(Some(r?)),
        }
    }

    fn observed(&self, path: &str) -> Result<Meta> {
        self.stat(path)?
            .ok_or_else(|| io_err(ErrorKind::NotFound, path, "vanished after a change"))
    }

    /// Removes the parents of `full` that are empty, up to the folder.
    fn remove_empty_parents(&self, full: &Path) {
        let mut dir = full.parent();
        while let Some(d) = dir {
            if d == self.root || !d.starts_with(&self.root) || fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }

    /// Writes a file or link beside `full`, then renames it over `full`;
    /// with `expect`, only if `path` is still so just before, keeping the
    /// file it replaces in the trash. Whether it renamed.
    fn replace(
        &self,
        (full, path): (&Path, &str),
        kind: Kind,
        content: &mut dyn Read,
        mtime: i64,
        expect: Option<Option<Meta>>,
    ) -> Result<bool> {
        let tmp = tmp_path(full);
        match fs::remove_file(&tmp) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
            _ => {} // left by an interrupted write, or none
        }
        let done = make_tmp(&tmp, kind, content, mtime).and_then(|()| {
            if let Some(expect) = expect {
                if self.stat(path)? != expect {
                    return Ok(false); // changed meanwhile: the next scan sees it
                }
                if expect.is_some_and(|m| m.kind != Kind::Directory) {
                    self.keep(full, path)?;
                }
            }
            if Self::lstat(full)?.is_some_and(|m| m.is_dir()) {
                fs::remove_dir(full)?; // only an empty directory gives way
            }
            fs::rename(&tmp, full)?;
            Ok(true)
        });
        if !matches!(done, Ok(true)) {
            let _ = fs::remove_file(&tmp);
        }
        done
    }

    /// Keeps the file or link at `full` in the trash, as a second link to
    /// it, or a copy where the trash is on another file system.
    fn keep(&self, full: &Path, path: &str) -> Result<()> {
        let slot = free_slot(&self.trash, path)?;
        if fs::hard_link(full, &slot).is_err()
            && !Self::lstat(full)?.is_some_and(|m| m.is_symlink())
        {
            fs::copy(full, &slot)?;
        }
        Ok(())
    }

    /// `write` and `write_if`: with `expect`, only if `path` is still so.
    fn put(
        &self,
        path: &str,
        kind: Kind,
        content: &mut dyn Read,
        mtime: i64,
        expect: Option<Option<Meta>>,
    ) -> Result<Option<Meta>> {
        self.writable()?;
        let full = self.full(path)?;
        if is_dir_path(path) != (kind == Kind::Directory) {
            return invalid(format!("{path:?} does not suit kind {kind:?}"));
        }
        if kind == Kind::Directory && content.read(&mut [0])? > 0 {
            return invalid(format!("directory {path:?} with content"));
        }
        self.reach(path, true)?;
        if kind == Kind::Directory && expect.is_some_and(|e| self.stat(path).ok() != Some(e)) {
            return Ok(None);
        }
        match (kind, Self::lstat(&full)?) {
            (Kind::Directory, None) => fs::create_dir(&full)?,
            (Kind::Directory, Some(m)) if m.is_dir() && is_empty(&full)? => {}
            (Kind::Directory, Some(_)) => {
                return Err(io_err(
                    ErrorKind::AlreadyExists,
                    path,
                    "something else is there",
                ));
            }
            (_, Some(m)) if m.is_dir() && !is_empty(&full)? => {
                return Err(io_err(
                    ErrorKind::DirectoryNotEmpty,
                    path,
                    "a directory holds something",
                ));
            }
            _ => {
                if !self.replace((&full, path), kind, content, mtime, expect)? {
                    return Ok(None);
                }
            }
        }
        self.observed(path).map(Some)
    }
}

impl Workspace for PlainFolder {
    fn list(&self) -> Result<Vec<(String, Meta)>> {
        walk(&self.root, !self.read_only)
    }

    fn stat(&self, path: &str) -> Result<Option<Meta>> {
        let full = self.full(path)?;
        if !matches!(self.parents(&full, false)?, Parents::Dirs) {
            return Ok(None);
        }
        let Some(m) = Self::lstat(&full)? else {
            return Ok(None);
        };
        match (is_dir_path(path), m.is_dir()) {
            (true, true) if is_empty(&full)? => Ok(Some(dir_meta(&m))),
            (false, false) => Ok(meta(&full, &m)?),
            _ => Ok(None),
        }
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        let full = self.reach(path, false)?;
        let m = self.stat(path)?.ok_or_else(|| absent(path))?;
        match m.kind {
            Kind::Directory => Ok(Box::new(io::empty())),
            Kind::Symlink => Ok(Box::new(io::Cursor::new(crate::os::link_target(&full)?))),
            Kind::Regular | Kind::Executable => {
                let f = File::open(&full)?;
                if crate::os::inode(&f.metadata()?) != m.inode {
                    return Err(io_err(ErrorKind::Other, path, "replaced while opening"));
                }
                Ok(Box::new(f))
            }
        }
    }

    fn write(&self, path: &str, kind: Kind, content: &mut dyn Read, mtime: i64) -> Result<Meta> {
        let m = self.put(path, kind, content, mtime, None)?;
        // Unconditional: `put` returns `None` only for an unmet `expect`.
        m.ok_or_else(|| io_err(ErrorKind::Other, path, "not written"))
    }

    fn write_if(
        &self,
        path: &str,
        kind: Kind,
        content: &mut dyn Read,
        mtime: i64,
        expect: Option<Meta>,
    ) -> Result<Option<Meta>> {
        self.put(path, kind, content, mtime, Some(expect))
    }

    fn set_mtime(&self, path: &str, mtime: i64) -> Result<Meta> {
        self.writable()?;
        let full = self.reach(path, false)?;
        let m = self.stat(path)?.ok_or_else(|| absent(path))?;
        // Links and directories carry no synchronised mtime (§5.1).
        if matches!(m.kind, Kind::Regular | Kind::Executable) {
            let f = crate::os::open_for_times(&full)?;
            if crate::os::inode(&f.metadata()?) != m.inode {
                return Err(io_err(ErrorKind::Other, path, "replaced while opening"));
            }
            f.set_modified(from_ns(mtime))?;
        }
        self.observed(path)
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.writable()?;
        let dir = is_dir_path(from);
        if dir != is_dir_path(to)
            || from == to
            || (dir && (to.starts_with(from) || from.starts_with(to)))
        {
            return invalid("rename operands overlap or disagree on directory kind");
        }
        self.full(to)?;
        let src = self.reach(from, false)?;
        let m = Self::lstat(&src)?.ok_or_else(|| absent(from))?;
        if m.is_dir() != dir {
            return invalid("rename source disagrees with directory kind");
        }
        let dst = self.reach(to, true)?;
        if Self::lstat(&dst)?.is_some() {
            return Err(io_err(ErrorKind::AlreadyExists, to, "the target exists"));
        }
        fs::rename(&src, &dst)?;
        // Make the tree rename durable before a move intent publishes HEAD.
        if let Some(parent) = src.parent() {
            crate::file::sync_dir(parent)?;
        }
        if let Some(parent) = dst.parent() {
            crate::file::sync_dir(parent)?;
        }
        self.remove_empty_parents(&src);
        Ok(())
    }

    fn trash(&self, path: &str) -> Result<()> {
        self.writable()?;
        let full = self.reach(path, false)?;
        let m = self.stat(path)?.ok_or_else(|| absent(path))?;
        if m.kind == Kind::Directory {
            fs::remove_dir(&full)?;
        } else {
            let slot = free_slot(&self.trash, path)?;
            fs::rename(&full, &slot)?;
        }
        self.remove_empty_parents(&full);
        Ok(())
    }
}

/// `trash/<unix-time>/<path>`, or `<unix-time>-<n>` when that slot holds the
/// path or a file where one of its directories belongs (§5.6 step 4).
fn free_slot(trash: &Path, path: &str) -> Result<PathBuf> {
    let secs = now_secs();
    for n in 0..1000 {
        let slot = match n {
            0 => secs.to_string(),
            n => format!("{secs}-{n}"),
        };
        let dst = trash.join(slot).join(path);
        let dir_ok = dst.parent().is_none_or(|d| fs::create_dir_all(d).is_ok());
        if dir_ok && PlainFolder::lstat(&dst)?.is_none() {
            return Ok(dst);
        }
    }
    Err(io_err(ErrorKind::Other, path, "no free trash slot"))
}

/// Writes the new file or link at `tmp`: content, mode, mtime, fsync. A
/// link's own mtime is not synchronised (§5.1), so it keeps the disk's.
fn make_tmp(tmp: &Path, kind: Kind, content: &mut dyn Read, mtime: i64) -> Result<()> {
    if kind == Kind::Symlink {
        let mut target = Vec::new();
        content.read_to_end(&mut target)?;
        return Ok(crate::os::symlink(&target, tmp)?);
    }
    let exec = kind == Kind::Executable;
    // `create_new` never follows a link planted at the temporary name.
    let mut f = crate::os::create_new(tmp, exec)?;
    io::copy(content, &mut f)?;
    crate::os::set_executable(&f, exec)?; // past the umask
    f.set_modified(from_ns(mtime))?;
    Ok(f.sync_all()?)
}

/// `.<name>.kide-tmp` beside `full`, the name cut to fit 255 bytes.
fn tmp_path(full: &Path) -> PathBuf {
    let name = full.file_name().and_then(OsStr::to_str).unwrap_or("file");
    let mut end = name.len().min(255 - ".kide-tmp".len() - 1);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    full.with_file_name(format!(".{}.kide-tmp", &name[..end]))
}

fn io_err(kind: ErrorKind, path: &str, what: &str) -> Error {
    Error::Io(io::Error::new(kind, format!("{path:?}: {what}")))
}

fn absent(path: &str) -> Error {
    io_err(ErrorKind::NotFound, path, "no such file")
}

// The suite needs unix symbolic links, permission bits and byte names.
#[cfg(all(test, unix))]
mod tests;
