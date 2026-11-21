//! A Kide folder linked to one branch of a Git repository. Commits go out
//! from the folder's `HEAD` (`commit`, `push`); upstream commits come in as
//! a root the folder pulls like a peer's (`pull`), so Kide's own rules
//! decide paths changed on both sides.
//!
//! The link lives in `.kide/git/`, which never syncs: a bare repository,
//! the link settings and the map from every commit made or imported here to
//! the Kide root holding its tree. The local branch is the last commit both
//! sides agree on. Git runs as the installed `git` program.

mod export;
mod git;
mod import;

pub use export::{Change, Status, commit, push, status};
pub use import::{Pulled, pull};

use git::Git;
use kide::folder::Folder;
use kide::format::manifest::build_manifest;
use kide::hash::Hash;
use kide::load::{self, read_manifest};
use kide::{Error, Result};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

/// The result of `link`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Linked {
    /// The branch head an empty folder took, if the branch existed.
    pub adopted: Option<String>,
}

/// Links `folder` to `branch` of the repository at `url`; `author` is
/// "Name <email>", used for commits made here. An empty folder takes the
/// branch's files; a folder with files needs an empty or new branch.
pub fn link(folder: &mut Folder, url: &str, branch: &str, author: &str) -> Result<Linked> {
    let dir = state(folder);
    if dir.exists() {
        return Err(Error::Invalid("the folder is already linked".into()));
    }
    let settings = Settings {
        url: url.into(),
        branch: branch.into(),
        author: identity(author)?,
    };
    let tmp = folder.kide().join("git.new");
    let _ = fs::remove_dir_all(&tmp); // left by an interrupted link
    let result = make(folder, &tmp, &settings);
    if result.is_err() {
        let _ = fs::remove_dir_all(state(folder));
        let _ = fs::remove_dir_all(&tmp);
    }
    result
}

fn make(folder: &mut Folder, tmp: &std::path::Path, s: &Settings) -> Result<Linked> {
    fs::create_dir_all(tmp)?;
    let git = Git::init(&tmp.join("repo"))?;
    git.run(&["check-ref-format", &format!("refs/heads/{}", s.branch)])?;
    fs::write(tmp.join("link"), s.encode())?;
    let exists = !git
        .remote(&[
            "ls-remote",
            "--heads",
            &s.url,
            &format!("refs/heads/{}", s.branch),
        ])?
        .is_empty();
    let mut files = false;
    for e in read_manifest(&*folder.store, &folder.head)? {
        files |= !load::record(&*folder.store, &e.version)?.is_tombstone();
    }
    if exists && files {
        return Err(Error::Refused(
            "both the folder and the branch have files; link an empty folder or an empty branch"
                .into(),
        ));
    }
    fs::rename(tmp, state(folder))?;
    let adopted = match exists {
        true => pull(folder)?.commit,
        false => None,
    };
    Ok(Linked { adopted })
}

/// The settings in `.kide/git/link`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub url: String,
    pub branch: String,
    pub author: String,
}

impl Settings {
    fn encode(&self) -> String {
        format!(
            "url = {}\nbranch = {}\nauthor = {}\n",
            self.url, self.branch, self.author
        )
    }

    fn parse(text: &str) -> Result<Settings> {
        let get = |key: &str| {
            text.lines()
                .filter_map(|l| l.split_once('='))
                .find(|(k, _)| k.trim() == key)
                .map(|(_, v)| v.trim().to_string())
                .ok_or_else(|| Error::Invalid(format!("the link has no {key}")))
        };
        Ok(Settings {
            url: get("url")?,
            branch: get("branch")?,
            author: get("author")?,
        })
    }
}

/// "Name <email>" with nothing that would break a commit header.
fn identity(s: &str) -> Result<String> {
    let ok = s.contains(" <") && s.ends_with('>') && !s.contains(['\n', '\0']);
    match ok && s.matches('<').count() == 1 {
        true => Ok(s.trim().to_string()),
        false => Err(Error::Invalid(format!(
            "not a Git identity \"Name <email>\": {s:?}"
        ))),
    }
}

fn state(folder: &Folder) -> PathBuf {
    folder.kide().join("git")
}

/// An open link.
pub(crate) struct Link {
    pub dir: PathBuf,
    pub settings: Settings,
    pub git: Git,
}

impl Link {
    pub fn open(folder: &Folder) -> Result<Link> {
        let dir = state(folder);
        let text = match fs::read_to_string(dir.join("link")) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Invalid("the folder is not linked to Git".into()));
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Link {
            git: Git::new(&dir.join("repo")),
            settings: Settings::parse(&text)?,
            dir,
        })
    }

    pub fn branch_ref(&self) -> String {
        format!("refs/heads/{}", self.settings.branch)
    }

    /// The local branch: the last commit both sides agree on.
    pub fn base(&self) -> Result<Option<String>> {
        self.git
            .try_run(&["rev-parse", "--verify", "-q", &self.branch_ref()])
    }

    /// The Kide root holding `commit`'s tree.
    pub fn root_of(&self, commit: &str) -> Result<Option<Hash>> {
        let text = match fs::read_to_string(self.dir.join("map")) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        for line in text.lines().rev() {
            if let Some((c, root)) = line.split_once(' ')
                && c == commit
            {
                return Ok(Some(Hash::from_hex(root)?));
            }
        }
        Ok(None)
    }

    /// Records that `root` holds `commit`'s tree, durably.
    pub fn map(&self, commit: &str, root: &Hash) -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("map"))?;
        writeln!(f, "{commit} {root}")?;
        f.sync_all()?;
        Ok(())
    }

    /// The Kide root of `base`, or of an empty folder without one.
    pub fn base_root(&self, folder: &Folder, base: Option<&str>) -> Result<Hash> {
        let Some(base) = base else {
            let store = folder.store.clone();
            return Ok(build_manifest(&[], &mut |id, b| store.put_known(id, b))?.id);
        };
        self.root_of(base)?.ok_or_else(|| {
            Error::Refused(format!(
                "no Kide root is known for commit {base}; relink the folder"
            ))
        })
    }
}

/// A conflict copy's name (SPEC §5.6).
pub(crate) fn is_conflict_copy(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.contains(".kide-conflict-")
}
