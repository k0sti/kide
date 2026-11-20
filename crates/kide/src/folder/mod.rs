//! A Kide folder (§5.4): the user's directory and its local state in
//! `.kide/`, beside it or, with `folder` in the configuration, apart from
//! it; with the engines of the folder profile: scanning, checkout,
//! verification, pulling from another device (compare, plan, apply) and
//! garbage collection. The daemon is the `kide` program's (`kide-cli`).

pub mod apply;
pub mod checkout;
pub mod compare;
pub mod config;
pub mod gc;
pub mod guard;
pub mod index;
pub mod moves;
pub mod net;
pub mod npub;
pub mod plan;
pub mod pull;
pub mod repair;
pub mod restore;
pub mod roots;
pub mod rules;
pub mod scan;
pub mod verify;
#[cfg(all(feature = "watch", not(target_arch = "wasm32")))]
pub mod watch;

use crate::error::{Error, Result, invalid};
use crate::file::write_atomic;
use crate::format::FORMAT;
use crate::format::content::write_content;
use crate::hash::Hash;
use crate::iface::ObjectStore;
use crate::store::{DAMAGED, PackStore};
use crate::time::now_secs;
use crate::workspace::PlainFolder;
use config::{Config, Mode, TEMPLATE};
use index::Index;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// An open folder. A writable one holds the lock on `.kide/lock` until it
/// is dropped.
pub struct Folder {
    /// The synchronised directory.
    pub root: PathBuf,
    /// The local state: `.kide/` of the directory the folder was opened at.
    state: PathBuf,
    pub config: Config,
    pub index: Index,
    pub head: Hash,
    pub store: Arc<dyn ObjectStore>,
    /// What a scan does with files gone from the disk (§5.5): guarded
    /// unless `guard = off`.
    pub deletions: guard::Deletions,
    fsync: bool,
    lock: Option<File>,
}

impl Folder {
    /// Creates `.kide/`: a configuration template, an empty index, `HEAD`
    /// at the root of empty content, no retained roots, and the stores.
    pub fn init(root: &Path) -> Result<()> {
        let kide = root.join(".kide");
        if fs::symlink_metadata(&kide).is_ok() {
            return invalid(format!("{} is already a Kide folder", root.display()));
        }
        for d in ["objects", "staging", "trash"] {
            fs::create_dir_all(kide.join(d))?;
        }
        fs::write(kide.join("config"), TEMPLATE)?;
        fs::write(kide.join("format"), format!("{FORMAT}\n"))?;
        for f in ["index", "roots", "lock"] {
            File::create(kide.join(f))?;
        }
        let store = PackStore::open(kide.join("objects"))?;
        let empty = write_content(b"", &mut |id, b| store.put_known(id, b))?;
        store.sync()?;
        write_atomic(&kide.join("HEAD"), head_line(&empty.id).as_bytes(), true)
    }

    /// Opens the initialised folder at `dir` for changes, taking the lock:
    /// `Unavailable` while another process holds it.
    pub fn open(dir: &Path) -> Result<Folder> {
        Folder::open_with(dir, true)
    }

    /// `open` without forcing anything to disk: for tests (`conformance`).
    #[cfg(any(test, feature = "conformance"))]
    pub fn open_unsynced(dir: &Path) -> Result<Folder> {
        Folder::open_with(dir, false)
    }

    /// Opens a folder to read, without the lock: a running daemon may
    /// change it meanwhile.
    pub fn open_read_only(dir: &Path) -> Result<Folder> {
        let kide = kide_dir(dir)?;
        let store = PackStore::open_read_only(kide.join("objects"))?;
        Folder::load(kide, Arc::new(store), true, None)
    }

    fn open_with(dir: &Path, fsync: bool) -> Result<Folder> {
        let kide = kide_dir(dir)?;
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(kide.join("lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let m = format!("{} is locked by another process", dir.display());
                return Err(Error::Unavailable(m));
            }
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        let store = PackStore::open(kide.join("objects"))?;
        let store = if fsync { store } else { store.without_fsync() };
        Folder::load(kide, Arc::new(store), fsync, Some(lock))
    }

    fn load(
        kide: PathBuf,
        store: Arc<dyn ObjectStore>,
        fsync: bool,
        lock: Option<File>,
    ) -> Result<Self> {
        check_format(&kide)?;
        let head = Hash::from_hex(fs::read_to_string(kide.join("HEAD"))?.trim())?;
        let config = Config::parse(&fs::read_to_string(kide.join("config"))?)?;
        // `kide` is `<dir>/.kide`, so it has a parent.
        let dir = kide.parent().unwrap_or(&kide);
        let deletions = match !config.guard {
            true => guard::Deletions::Allowed,
            false => guard::Deletions::Guarded,
        };
        let mut folder = Folder {
            root: config.folder.as_ref().map_or(dir.into(), |f| dir.join(f)),
            state: kide.clone(),
            config,
            index: Index::load(&kide.join("index"))?,
            head,
            store,
            deletions,
            fsync,
            lock,
        };
        if folder.lock.is_some() {
            let ws = folder.workspace();
            moves::recover(&mut folder, &ws)?;
        }
        Ok(folder)
    }

    pub fn kide(&self) -> PathBuf {
        self.state.clone()
    }

    /// The synchronised directory as a workspace: read-only in a send folder.
    pub fn workspace(&self) -> PlainFolder {
        match self.config.mode {
            Mode::Send => PlainFolder::read_only(&self.root),
            Mode::Sync => PlainFolder::with_trash(&self.root, self.kide().join("trash")),
        }
    }

    /// The staging store of an unfinished pull (§4.3): a pack store of its
    /// own, so an interrupted pull keeps what it fetched.
    pub fn staging(&self) -> Result<PackStore> {
        self.writable()?;
        let s = PackStore::open(self.kide().join("staging"))?;
        Ok(if self.fsync { s } else { s.without_fsync() })
    }

    fn writable(&self) -> Result<()> {
        if self.lock.is_none() {
            return invalid("the folder is open read-only");
        }
        if self.kide().join("move-intent").try_exists()? {
            return invalid("a move is pending; close and reopen the folder before further writes");
        }
        Ok(())
    }

    /// Rewrites the index whole.
    pub fn save_index(&self) -> Result<()> {
        self.writable()?;
        self.index.save(&self.kide().join("index"), self.fsync)
    }

    /// Syncs the store, retains the old root in `roots`, then rewrites
    /// `HEAD` whole (§4.1: a root is written only once its objects are
    /// durable).
    pub fn set_head(&mut self, head: Hash) -> Result<()> {
        self.writable()?;
        self.publish_head(head)
    }

    /// The move recovery path also publishes, while its intent is present.
    fn publish_head(&mut self, head: Hash) -> Result<()> {
        if self.lock.is_none() {
            return invalid("the folder is open read-only");
        }
        self.store.sync()?;
        let roots = self.kide().join("roots");
        roots::append(&roots, now_secs(), &self.head, self.fsync)?;
        write_atomic(
            &self.kide().join("HEAD"),
            head_line(&head).as_bytes(),
            self.fsync,
        )?;
        self.head = head;
        Ok(())
    }

    /// Whether the store dropped a corrupt object (§4.1) since the last
    /// repair: objects in it may then lack part of their closure.
    pub fn damaged(&self) -> bool {
        self.kide().join("objects").join(DAMAGED).exists()
    }

    /// Clears the damage mark once a repair found every closure whole.
    pub fn repaired(&self) -> Result<()> {
        self.writable()?;
        match fs::remove_file(self.kide().join("objects").join(DAMAGED)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// The retained earlier roots (§5.4, §5.7): unix time and root.
    pub fn roots(&self) -> Result<Vec<(u64, Hash)>> {
        roots::read(&self.kide().join("roots"))
    }
}

fn kide_dir(dir: &Path) -> Result<PathBuf> {
    let kide = dir.join(".kide");
    match kide.join("HEAD").is_file() {
        true => Ok(kide),
        false => invalid(format!(
            "{} is not a Kide folder (run kide init)",
            dir.display()
        )),
    }
}

/// `.kide/format` (§5.4): `Incompatible` unless it names `FORMAT`.
fn check_format(kide: &Path) -> Result<()> {
    let path = kide.join("format");
    let text = match fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Incompatible(
                "folder has no format marker; create a fresh folder".into(),
            ));
        }
        r => r?,
    };
    match text.trim().parse::<u16>() {
        Ok(FORMAT) => Ok(()),
        Ok(f) => Err(Error::Incompatible(format!(
            "{} is folder format {f}; this program reads format {FORMAT}",
            kide.display()
        ))),
        Err(_) => invalid(format!("{}: not a format number", path.display())),
    }
}

fn head_line(head: &Hash) -> String {
    format!("{head}\n")
}

#[cfg(test)]
mod tests;
