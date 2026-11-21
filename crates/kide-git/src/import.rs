//! From Git to the folder: the fetched branch head becomes a Kide root whose
//! changed paths are new versions after the base commit's versions, and the
//! folder pulls that root like a peer's. A path changed only upstream moves
//! forward; one changed on both sides is decided by Kide's own rule (SPEC
//! §5.6), so no local edit is lost.

use crate::Link;
use kide::change::Rescan;
use kide::folder::Folder;
use kide::folder::net::served;
use kide::folder::pull::{Pulling, pull as pull_root};
use kide::folder::roots;
use kide::folder::scan::synced_mtime;
use kide::format::content::ContentWriter;
use kide::format::manifest::{Kind, PathEntry, build_manifest};
use kide::format::record::Record;
use kide::hash::Hash;
use kide::load::{self, read_manifest, validate_manifest};
use kide::names::Platform;
use kide::resolve::DecisionTable;
use kide::time::now_secs;
use kide::transport::InProcess;
use kide::{Error, Result};
use std::collections::BTreeMap;

/// The result of `pull`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pulled {
    /// The upstream commit taken in; `None` when there was nothing new.
    pub commit: Option<String>,
    /// Paths whose entries differed between the folder and upstream.
    pub decided: usize,
    /// Paths merged as text, with both sides' changes.
    pub merged: Vec<String>,
}

/// One path in `git diff-tree --raw` output.
struct Change {
    status: u8,
    mode: String,
    blob: String,
    path: String,
}

/// Fetches the branch and takes its head into the folder.
pub fn pull(folder: &mut Folder) -> Result<Pulled> {
    let link = Link::open(folder)?;
    let s = &link.settings;
    let theirs = format!("refs/heads/{}", s.branch);
    if link
        .git
        .remote(&["ls-remote", "--heads", &s.url, &theirs])?
        .is_empty()
    {
        return Ok(Pulled::default()); // the branch does not exist yet
    }
    let upstream = "refs/kide/upstream";
    let spec = format!("+{theirs}:{upstream}");
    link.git
        .remote(&["fetch", "--quiet", "--no-tags", &s.url, &spec])?;
    let up = link.git.run(&["rev-parse", upstream])?;
    let base = link.base()?;
    let from = match &base {
        None => None,
        Some(b) if *b == up => return Ok(Pulled::default()),
        Some(b) => match link.git.try_run(&["merge-base", b, &up])? {
            Some(m) => Some(m),
            None => {
                return Err(Error::Refused(
                    "the branch's history is unrelated to the link's".into(),
                ));
            }
        },
    };
    if from.as_deref() == Some(up.as_str()) {
        return Ok(Pulled::default()); // only local commits are new
    }
    let root = upstream_root(folder, &link, from.as_deref(), &up)?;
    // Retained like a past HEAD, so a later commit can start from it.
    roots::append(&folder.kide().join("roots"), now_secs(), &root, true)?;
    link.map(&up, &root)?;
    let network = InProcess::default();
    network.register("git", served(folder.store.clone(), move || root));
    let ws = folder.workspace();
    let with = Pulling {
        ws: &ws,
        source: &mut Rescan,
        resolver: &DecisionTable,
        transport: &network,
        platform: &Platform::current(),
        review: None,
    };
    let report = pull_root(folder, with, "git")?;
    let old = base.unwrap_or_else(|| "0".repeat(up.len()));
    link.git
        .run(&["update-ref", &link.branch_ref(), &up, &old])?;
    Ok(Pulled {
        commit: Some(up),
        decided: report.decided,
        merged: report.merged,
    })
}

/// The Kide root of `up`'s tree: the root of `from` (or an empty one) with
/// every path Git changed since given a new version after the old one.
fn upstream_root(folder: &Folder, link: &Link, from: Option<&str>, up: &str) -> Result<Hash> {
    refuse_unsupported(link, up)?;
    let store = folder.store.clone();
    let mut sink = |id: &Hash, b: &[u8]| store.put_known(id, b);
    let from_root = link.base_root(folder, from)?;
    let mut entries: BTreeMap<String, PathEntry> = read_manifest(&*store, &from_root)?
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let empty = String::from_utf8_lossy(&link.git.run_with(&["mktree"], &mut |_| Ok(()))?)
        .trim()
        .to_string();
    let at = link.git.run(&["show", "-s", "--format=%ct", up])?;
    let at = at.parse::<i64>().unwrap_or(0).saturating_mul(1_000_000_000);
    let mut blobs = link.git.blobs()?;
    for c in changes(link, from.unwrap_or(&empty), up)? {
        let old = entries.get(&c.path);
        let parents: Vec<Hash> = old.map(|e| vec![e.version]).unwrap_or_default();
        if c.status == b'D' {
            match old {
                Some(e) if !load::record(&*store, &e.version)?.is_tombstone() => {
                    let id = store.put(&Record::tombstone(parents).encode()?)?;
                    let e = PathEntry {
                        path: c.path.clone(),
                        version: id,
                        size: 0,
                        mtime: 0,
                    };
                    entries.insert(c.path, e);
                }
                _ => {}
            }
            continue;
        }
        let kind = match c.mode.as_str() {
            "100644" => Kind::Regular,
            "100755" => Kind::Executable,
            "120000" => Kind::Symlink,
            m => {
                return Err(Error::Refused(format!(
                    "{}: unsupported Git mode {m}",
                    c.path
                )));
            }
        };
        let mut w = ContentWriter::new();
        let size = blobs.read(&c.blob, &mut |b| w.write(b, &mut sink))?;
        let content = w.finish(&mut sink)?;
        let record = Record {
            parents,
            root: content.id,
            state: kind.state(),
            change: Vec::new(),
        };
        let version = store.put(&record.encode()?)?;
        let mtime = synced_mtime(kind, at);
        let e = PathEntry {
            path: c.path.clone(),
            version,
            size,
            mtime,
        };
        entries.insert(c.path, e);
    }
    let entries: Vec<PathEntry> = entries.into_values().collect();
    let root = build_manifest(&entries, &mut sink)?.id;
    validate_manifest(&*store, &root)?;
    Ok(root)
}

/// Submodules and Git LFS: refused before anything is written.
fn refuse_unsupported(link: &Link, up: &str) -> Result<()> {
    let tree = link.git.run(&["ls-tree", "-r", up])?;
    if tree.lines().any(|l| l.starts_with("160000")) {
        return Err(Error::Refused(
            "the branch has submodules, which Kide does not import".into(),
        ));
    }
    let lfs = [
        "grep",
        "-q",
        "-e",
        "filter=lfs",
        up,
        "--",
        ":(glob)**/.gitattributes",
    ];
    if link.git.try_run(&lfs)?.is_some() {
        return Err(Error::Refused(
            "the branch uses Git LFS, which Kide does not import".into(),
        ));
    }
    Ok(())
}

/// Every path that differs between two commits or trees.
fn changes(link: &Link, from: &str, to: &str) -> Result<Vec<Change>> {
    let args = [
        "diff-tree",
        "-r",
        "-z",
        "--no-renames",
        "--no-commit-id",
        from,
        to,
    ];
    let out = link.git.run_with(&args, &mut |_| Ok(()))?;
    let mut fields = out.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut list = Vec::new();
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        // ":<old mode> <new mode> <old id> <new id> <status>"
        let meta = String::from_utf8_lossy(meta);
        let m: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [_, mode, _, blob, status] = m.as_slice() else {
            return Err(Error::Invalid(format!("git diff-tree: {meta}")));
        };
        let Ok(path) = String::from_utf8(path.to_vec()) else {
            return Err(Error::Invalid("a Git path is not UTF-8".into()));
        };
        list.push(Change {
            status: status.as_bytes().first().copied().unwrap_or(b'?'),
            mode: mode.to_string(),
            blob: blob.to_string(),
            path,
        });
    }
    Ok(list)
}
