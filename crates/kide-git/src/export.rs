//! From the folder to Git: what changed since the base commit, a commit of
//! exactly those changes on top of it, and pushing it.

use crate::{Link, is_conflict_copy};
use kide::folder::Folder;
use kide::folder::compare::compare;
use kide::format::manifest::{Kind, PathEntry};
use kide::hash::Hash;
use kide::iface::ObjectStore;
use kide::load::{self, ContentReader, read_manifest};
use kide::time::now_secs;
use kide::{Error, Result};
use std::io::Write;
use std::process::ChildStdin;

/// What `commit` would do now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub url: String,
    pub branch: String,
    /// The local branch: the last commit both sides agree on.
    pub base: Option<String>,
    /// `A`dded, `M`odified or `D`eleted since the base, in path order.
    pub changes: Vec<Change>,
    /// Live conflict copies (SPEC §5.6): a commit waits until they are gone.
    pub conflicts: Vec<String>,
    /// Empty directories, which Git cannot hold.
    pub empty_dirs: Vec<String>,
}

/// `A`dded, `M`odified or `D`eleted, and the path.
pub type Change = (char, String);

/// A path's new content.
struct Put {
    path: String,
    kind: Kind,
    root: Hash,
    size: u64,
}

pub fn status(folder: &Folder) -> Result<Status> {
    let link = Link::open(folder)?;
    let base = link.base()?;
    let (changes, _) = diff(folder, &link, base.as_deref())?;
    let (conflicts, empty_dirs) = unexportable(folder)?;
    Ok(Status {
        url: link.settings.url,
        branch: link.settings.branch,
        base,
        changes,
        conflicts,
        empty_dirs,
    })
}

/// The live conflict copies and empty directories of `HEAD`.
fn unexportable(folder: &Folder) -> Result<(Vec<String>, Vec<String>)> {
    let store = &*folder.store;
    let (mut conflicts, mut empty_dirs) = (Vec::new(), Vec::new());
    for e in read_manifest(store, &folder.head)? {
        let dir = e.path.ends_with('/');
        if (dir || is_conflict_copy(&e.path)) && live(store, &e)?.is_some() {
            match dir {
                true => empty_dirs.push(e.path),
                false => conflicts.push(e.path),
            }
        }
    }
    Ok((conflicts, empty_dirs))
}

/// The changes between the base commit's root and `HEAD`, and the new
/// contents among them.
fn diff(folder: &Folder, link: &Link, base: Option<&str>) -> Result<(Vec<Change>, Vec<Put>)> {
    let store = &*folder.store;
    let from = link.base_root(folder, base)?;
    let (mut changes, mut puts) = (Vec::new(), Vec::new());
    for d in compare(store, &from, &folder.head)? {
        if d.path.ends_with('/') {
            continue; // an empty directory: no Git equivalent
        }
        let old = match &d.local {
            Some(e) => live(store, e)?,
            None => None,
        };
        let new = match &d.remote {
            Some(e) if d.local.as_ref().map(|l| l.version) == Some(e.version) => continue,
            Some(e) => live(store, e)?.map(|(kind, root)| (kind, root, e.size)),
            None => None,
        };
        match (old, new) {
            (old, Some((kind, root, size))) => {
                changes.push((if old.is_some() { 'M' } else { 'A' }, d.path.clone()));
                puts.push(Put {
                    path: d.path,
                    kind,
                    root,
                    size,
                });
            }
            (Some(_), None) => changes.push(('D', d.path)),
            (None, None) => {}
        }
    }
    Ok((changes, puts))
}

/// The kind and content root of a live entry; `None` for a tombstone.
fn live(store: &dyn ObjectStore, e: &PathEntry) -> Result<Option<(Kind, Hash)>> {
    let r = load::record(store, &e.version)?;
    Ok(Kind::of(&r)?.map(|k| (k, r.root)))
}

/// Commits the changes since the base commit on top of it, with `message`;
/// `None` when there are none. Refused while conflict copies exist.
pub fn commit(folder: &Folder, message: &str) -> Result<Option<String>> {
    let link = Link::open(folder)?;
    let (conflicts, _) = unexportable(folder)?;
    if !conflicts.is_empty() {
        let list = conflicts.join(", ");
        return Err(Error::Refused(format!(
            "resolve the conflict copies first: {list}"
        )));
    }
    let base = link.base()?;
    let (changes, puts) = diff(folder, &link, base.as_deref())?;
    if changes.is_empty() {
        return Ok(None);
    }
    let scratch = "refs/kide/new";
    link.git.run(&["update-ref", "-d", scratch])?;
    let now = now_secs();
    let who = &link.settings.author;
    let mut stream = |w: &mut ChildStdin| -> Result<()> {
        write!(
            w,
            "commit {scratch}\nauthor {who} {now} +0000\ncommitter {who} {now} +0000\n"
        )?;
        write!(w, "data {}\n{message}\n", message.len())?;
        if let Some(b) = &base {
            writeln!(w, "from {b}")?;
        }
        let mut puts = puts.iter().peekable();
        for (what, path) in &changes {
            match what {
                'D' => writeln!(w, "D {}", quote(path))?,
                _ => {
                    // `diff` pushes one put per added or modified path, in order.
                    let p = puts.next().expect("a put per A or M");
                    put(&*folder.store, p, w)?;
                }
            }
        }
        writeln!(w, "done")?;
        Ok(())
    };
    link.git.run_with(
        &["fast-import", "--quiet", "--done", "--date-format=raw"],
        &mut stream,
    )?;
    let new = link.git.run(&["rev-parse", scratch])?;
    let tree = |c: &str| link.git.run(&["rev-parse", &format!("{c}^{{tree}}")]);
    if let Some(b) = &base
        && tree(&new)? == tree(b)?
    {
        link.git.run(&["update-ref", "-d", scratch])?;
        return Ok(None); // new versions with the bytes Git already has
    }
    link.map(&new, &folder.head)?;
    let old = base.clone().unwrap_or_else(|| "0".repeat(new.len()));
    link.git
        .run(&["update-ref", &link.branch_ref(), &new, &old])?;
    link.git.run(&["update-ref", "-d", scratch])?;
    Ok(Some(new))
}

fn put(store: &dyn ObjectStore, p: &Put, w: &mut ChildStdin) -> Result<()> {
    let mode = match p.kind {
        Kind::Regular => "100644",
        Kind::Executable => "100755",
        Kind::Symlink => "120000",
        Kind::Directory => return Ok(()), // never passed: `diff` skips them
    };
    write!(w, "M {mode} inline {}\ndata {}\n", quote(&p.path), p.size)?;
    let mut reader = ContentReader::open(store, &p.root)?;
    let mut n = 0;
    while let Some(chunk) = reader.next_chunk()? {
        n += chunk.len() as u64;
        w.write_all(&chunk)?;
    }
    if n != p.size {
        return Err(Error::Invalid(format!(
            "{}: {n} bytes, not {}",
            p.path, p.size
        )));
    }
    writeln!(w)?;
    Ok(())
}

/// A path as a C-style quoted string, as fast-import reads it.
fn quote(path: &str) -> String {
    let mut q = String::from("\"");
    for c in path.chars() {
        match c {
            '"' => q.push_str("\\\""),
            '\\' => q.push_str("\\\\"),
            '\n' => q.push_str("\\n"),
            c if (c as u32) < 0x20 || c == '\x7f' => q.push_str(&format!("\\{:03o}", c as u32)),
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

/// Pushes the local branch to the remote branch, without force; the commit
/// pushed. `Refused` if the remote branch has moved: pull, commit, push.
pub fn push(folder: &Folder) -> Result<String> {
    let link = Link::open(folder)?;
    let Some(base) = link.base()? else {
        return Err(Error::Refused("nothing is committed yet".into()));
    };
    let r = link.branch_ref();
    let url = &link.settings.url;
    match link
        .git
        .remote(&["push", "--quiet", url, &format!("{r}:{r}")])
    {
        Err(Error::Refused(m)) => Err(Error::Refused(format!(
            "the remote branch has moved: run kide git pull, then commit and push ({m})"
        ))),
        r => r.map(|_| base),
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn paths_are_quoted_for_fast_import() {
        assert_eq!(quote("a b/c.md"), "\"a b/c.md\"");
        assert_eq!(quote("q\"\\\n\t"), "\"q\\\"\\\\\\n\\011\"");
        assert_eq!(quote("Zoë"), "\"Zoë\"");
    }
}
