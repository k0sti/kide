use super::*;
use crate::change::Rescan;
use crate::conformance::{Rng, TempDir};
use crate::folder::checkout::checkout;
use crate::folder::verify::verify;
use crate::iface::Meta;
use crate::load::{read_content, validate_manifest};
use crate::workspace::PlainFolder;
use std::fs;
use std::path::Path;

const OLD: i64 = 1_600_000_000_000_000_000;

struct Fx {
    tmp: TempDir,
    folder: Folder,
    ws: PlainFolder,
}

impl Fx {
    fn new() -> Fx {
        let tmp = TempDir::new("scan");
        Folder::init(tmp.path()).unwrap();
        let folder = Folder::open_unsynced(tmp.path()).unwrap();
        let ws = PlainFolder::new(tmp.path());
        Fx { tmp, folder, ws }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    /// Writes with an old mtime, so the entry is not racy.
    fn put(&self, path: &str, kind: Kind, content: &[u8]) {
        self.ws.write(path, kind, &mut &content[..], OLD).unwrap();
    }

    fn scan(&mut self) -> ScanReport {
        self.scan_on(&Platform::LINUX)
    }

    fn try_scan(&mut self) -> Result<ScanReport> {
        scan(&mut self.folder, &self.ws, &mut Rescan, &Platform::LINUX)
    }

    fn scan_on(&mut self, p: &Platform) -> ScanReport {
        scan(&mut self.folder, &self.ws, &mut Rescan, p).unwrap()
    }

    fn entry(&self, path: &str) -> Entry {
        self.folder.index.0[path]
    }

    fn record(&self, path: &str) -> Record {
        load::record(&*self.folder.store, &self.entry(path).version).unwrap()
    }

    /// The manifest of `HEAD`, validated, as (path, size, mtime).
    fn manifest(&self) -> Vec<(String, u64, i64)> {
        let entries = validate_manifest(&*self.folder.store, &self.folder.head).unwrap();
        entries
            .into_iter()
            .map(|e| (e.path, e.size, e.mtime))
            .collect()
    }
}

#[test]
fn a_new_file_gets_a_record_without_parents() {
    let mut fx = Fx::new();
    fx.put("a.txt", Kind::Regular, b"hello\n");
    let r = fx.scan();
    assert_eq!((r.versions, r.opened), (1, 1));
    let rec = fx.record("a.txt");
    assert_eq!((rec.parents.len(), rec.state.as_slice()), (0, &[0][..]));
    assert_eq!(fx.manifest(), [("a.txt".into(), 6, OLD)]);
    assert_eq!(fx.folder.head, r.head);
    let head = fs::read_to_string(fx.root().join(".kide/HEAD")).unwrap();
    assert_eq!(head.trim(), r.head.to_hex(), "HEAD is on disk");
    let index = crate::folder::index::Index::load(&fx.root().join(".kide/index")).unwrap();
    assert_eq!(index, fx.folder.index, "the index is on disk");
}

// Windows has no executable bit to retype by.
#[cfg(unix)]
#[test]
fn edits_and_retyping_follow_the_current_record() {
    let mut fx = Fx::new();
    fx.put("run.sh", Kind::Regular, b"one\n");
    fx.scan();
    let first = fx.entry("run.sh").version;
    fx.ws
        .write("run.sh", Kind::Regular, &mut &b"two\n"[..], OLD + 1)
        .unwrap();
    assert_eq!(fx.scan().versions, 1);
    assert_eq!(fx.record("run.sh").parents, [first]);
    let second = fx.entry("run.sh").version;
    fx.put("run.sh", Kind::Executable, b"two\n");
    assert_eq!(fx.scan().versions, 1);
    let rec = fx.record("run.sh");
    assert_eq!((rec.parents, rec.state), (vec![second], vec![1]));
}

#[test]
fn a_touch_never_creates_a_version() {
    let mut fx = Fx::new();
    fx.put("a.txt", Kind::Regular, b"same\n");
    let before = fx.scan();
    let version = fx.entry("a.txt").version;
    fx.ws.set_mtime("a.txt", OLD + 5_000).unwrap();
    let r = fx.scan();
    assert_eq!((r.versions, r.metadata, r.opened), (0, 1, 1));
    assert_eq!(fx.entry("a.txt").version, version);
    assert_eq!(fx.manifest(), [("a.txt".into(), 5, OLD + 5_000)]);
    assert_ne!(r.head, before.head, "the entry's mtime changed");
}

#[test]
fn deletion_and_recreation() {
    let mut fx = Fx::new();
    fx.put("d/a.txt", Kind::Regular, b"bye\n");
    fx.scan();
    let live = fx.entry("d/a.txt").version;
    fs::remove_file(fx.root().join("d/a.txt")).unwrap();
    assert_eq!(fx.scan().tombstones, 1);
    let tomb = fx.entry("d/a.txt");
    assert!(!tomb.is_live());
    let rec = fx.record("d/a.txt");
    assert!(rec.is_tombstone() && rec.parents == [live]);
    let left = [("d/".into(), 0, 0), ("d/a.txt".into(), 0, 0)];
    assert_eq!(fx.manifest(), left, "the tombstone stays; d/ is now empty");
    let again = fx.scan();
    assert_eq!((again.tombstones, again.versions), (0, 0));
    fx.put("d/a.txt", Kind::Regular, b"back\n");
    let r = fx.scan();
    assert_eq!(
        (r.versions, r.tombstones),
        (1, 1),
        "d/ holds something again"
    );
    assert_eq!(fx.record("d/a.txt").parents, [tomb.version]);
}

#[test]
fn a_rename_is_a_deletion_and_a_creation() {
    let mut fx = Fx::new();
    fx.put("old/name.txt", Kind::Regular, b"moving\n");
    fx.scan();
    let before = fx.record("old/name.txt");
    let id = fx.entry("old/name.txt").version;
    fx.ws.rename("old/name.txt", "new/name.txt").unwrap();
    let r = fx.scan();
    assert_eq!((r.versions, r.tombstones), (1, 1));
    assert_eq!(fx.record("old/name.txt").parents, [id]);
    let moved = fx.record("new/name.txt");
    assert!(moved.parents.is_empty(), "no rename detection (A11)");
    assert_eq!(moved.value(), before.value());
}

#[test]
fn empty_directory_lifecycle() {
    let mut fx = Fx::new();
    fs::create_dir(fx.root().join("d")).unwrap();
    assert_eq!(fx.scan().versions, 1);
    let rec = fx.record("d/");
    assert_eq!(rec.state, Kind::Directory.state());
    assert_eq!(fx.manifest(), [("d/".into(), 0, 0)]);
    fx.put("d/x", Kind::Regular, b"x");
    let r = fx.scan();
    assert_eq!((r.versions, r.tombstones), (1, 1), "d/ is gone, d/x is new");
    let dir_tomb = fx.entry("d/").version;
    fs::remove_file(fx.root().join("d/x")).unwrap();
    let r = fx.scan();
    assert_eq!(
        (r.versions, r.tombstones),
        (1, 1),
        "d/x is gone, d/ is back"
    );
    assert_eq!(fx.record("d/").parents, [dir_tomb]);
    fs::remove_dir(fx.root().join("d")).unwrap();
    assert!(fx.try_scan().is_err(), "the folder is empty now: refused");
    guard::confirm(&fx.folder.kide(), &fx.folder.head).unwrap();
    assert_eq!(fx.scan().tombstones, 1);
    let paths: Vec<String> = fx.manifest().into_iter().map(|e| e.0).collect();
    assert_eq!(paths, ["d/", "d/x"]);
    assert!(fx.folder.index.0.values().all(|e| !e.is_live()));
}

#[test]
fn links_carry_their_target_length_and_no_mtime() {
    let mut fx = Fx::new();
    fx.put("l", Kind::Symlink, b"some/target");
    fx.scan();
    assert_eq!(fx.manifest(), [("l".into(), 11, 0)]);
    let rec = fx.record("l");
    assert_eq!(
        read_content(&*fx.folder.store, &rec.root).unwrap(),
        b"some/target"
    );
}

#[test]
fn names_are_scanned_as_nfc() {
    let mut fx = Fx::new();
    fs::write(fx.root().join("Cafe\u{301}.txt"), b"x").unwrap();
    fx.scan();
    let paths: Vec<String> = fx.manifest().into_iter().map(|e| e.0).collect();
    assert_eq!(paths, ["Café.txt"]);
    assert!(fx.root().join("Café.txt").exists(), "renamed on disk");
}

// Sets a directory's time through a plain open, which Windows refuses.
#[cfg(unix)]
mod opens {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A workspace that counts `open` calls.
    struct Counting(PlainFolder, AtomicUsize);

    impl Workspace for Counting {
        fn list(&self) -> Result<Vec<(String, Meta)>> {
            self.0.list()
        }
        fn stat(&self, path: &str) -> Result<Option<Meta>> {
            self.0.stat(path)
        }
        fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
            self.1.fetch_add(1, Ordering::Relaxed);
            self.0.open(path)
        }
        fn write(&self, p: &str, k: Kind, c: &mut dyn Read, m: i64) -> Result<Meta> {
            self.0.write(p, k, c, m)
        }
        fn set_mtime(&self, p: &str, m: i64) -> Result<Meta> {
            self.0.set_mtime(p, m)
        }
        fn rename(&self, a: &str, b: &str) -> Result<()> {
            self.0.rename(a, b)
        }
        fn trash(&self, p: &str) -> Result<()> {
            self.0.trash(p)
        }
    }

    #[test]
    fn an_unchanged_folder_is_rescanned_without_opening_a_file() {
        let mut fx = Fx::new();
        for i in 0..20 {
            fx.put(
                &format!("dir{}/f{i}", i % 3),
                Kind::Regular,
                format!("{i}\n").as_bytes(),
            );
        }
        fs::create_dir(fx.root().join("empty")).unwrap();
        let dir = fx.root().join("empty");
        let f = fs::File::open(&dir).unwrap();
        f.set_modified(crate::time::from_ns(OLD)).unwrap(); // not racy
        let first = fx.scan();
        assert_eq!(first.opened, 21);
        let index = fs::read(fx.root().join(".kide/index")).unwrap();
        let roots = fs::read(fx.root().join(".kide/roots")).unwrap();
        let ws = Counting(PlainFolder::new(fx.root()), AtomicUsize::new(0));
        let again = scan(&mut fx.folder, &ws, &mut Rescan, &Platform::LINUX).unwrap();
        assert_eq!(ws.1.load(Ordering::Relaxed), 0, "a file was opened");
        assert_eq!((again.opened, again.versions, again.metadata), (0, 0, 0));
        assert_eq!(again.head, first.head);
        assert_eq!(fs::read(fx.root().join(".kide/index")).unwrap(), index);
        assert_eq!(
            fs::read(fx.root().join(".kide/roots")).unwrap(),
            roots,
            "HEAD unchanged"
        );
    }
}

#[test]
fn racy_entries_are_reread_without_new_versions() {
    let mut fx = Fx::new();
    let now = crate::time::now();
    fx.ws
        .write("fresh", Kind::Regular, &mut &b"just written\n"[..], now)
        .unwrap();
    fx.scan();
    let version = fx.entry("fresh").version;
    let r = fx.scan();
    assert_eq!(
        (r.opened, r.versions, r.metadata),
        (1, 0, 0),
        "re-read, nothing new"
    );
    assert_eq!(fx.entry("fresh").version, version);
    // A same-size edit within the racy window, with the mtime put back.
    let path = fx.root().join("fresh");
    let ino = fx.ws.stat("fresh").unwrap().unwrap().inode;
    fs::write(&path, b"just edited!\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(crate::time::from_ns(now))
        .unwrap();
    assert_eq!(fx.ws.stat("fresh").unwrap().unwrap().inode, ino);
    assert_eq!(fx.scan().versions, 1, "the edit is found");
}

#[test]
fn the_manifest_root_does_not_depend_on_history() {
    let files: Vec<(String, Vec<u8>)> = (0..12)
        .map(|i| {
            (
                format!("p{}/q{}/f{i}.txt", i % 2, i % 3),
                vec![b'a' + i as u8; 100 * i],
            )
        })
        .collect();
    let mut a = Fx::new();
    for (p, c) in &files {
        a.put(p, Kind::Regular, c);
    }
    let one = a.scan().head;
    let mut b = Fx::new();
    for (i, (p, c)) in files.iter().rev().enumerate() {
        b.put(p, Kind::Regular, c);
        if i % 4 == 0 {
            b.scan();
        }
    }
    assert_eq!(b.scan().head, one, "other orders and batches, same root");
}

/// Edits the file each time it is opened, after the scan's first stat.
struct Meddling(PlainFolder);

impl Workspace for Meddling {
    fn list(&self) -> Result<Vec<(String, Meta)>> {
        self.0.list()
    }
    fn stat(&self, path: &str) -> Result<Option<Meta>> {
        self.0.stat(path)
    }
    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        let r = self.0.open(path)?;
        fs::write(self.0.root().join(path), b"changed while being read").unwrap();
        Ok(r)
    }
    fn write(&self, p: &str, k: Kind, c: &mut dyn Read, m: i64) -> Result<Meta> {
        self.0.write(p, k, c, m)
    }
    fn set_mtime(&self, p: &str, m: i64) -> Result<Meta> {
        self.0.set_mtime(p, m)
    }
    fn rename(&self, a: &str, b: &str) -> Result<()> {
        self.0.rename(a, b)
    }
    fn trash(&self, p: &str) -> Result<()> {
        self.0.trash(p)
    }
}

#[test]
fn a_change_during_the_read_is_retried() {
    let mut fx = Fx::new();
    fx.put("a", Kind::Regular, b"original");
    let ws = Meddling(PlainFolder::new(fx.root()));
    let r = scan(&mut fx.folder, &ws, &mut Rescan, &Platform::LINUX).unwrap();
    assert_eq!((r.raced, r.versions), (1, 0));
    assert!(fx.folder.index.0.is_empty(), "the old (absent) entry stays");
    assert_eq!(fx.scan().versions, 1, "the next scan records the file");
}

#[test]
fn colliding_entries_on_disk_are_both_scanned() {
    // Both files are on record, so a conflating platform setting is wrong (§5.3):
    // neither is held, and deleting one is an ordinary deletion. Held entries
    // that arrive from a peer are covered by the sync tests.
    let mut fx = Fx::new();
    fx.put("A", Kind::Regular, b"upper");
    if fx.root().join("a").exists() {
        return; // a file system that folds case holds only one of them
    }
    fx.put("a", Kind::Regular, b"lower");
    fx.scan();
    fs::remove_file(fx.root().join("a")).unwrap();
    let r = fx.scan_on(&Platform::MACOS);
    assert_eq!((r.held, r.tombstones), (vec![], 1));
    assert!(!fx.entry("a").is_live());
}

#[test]
fn checkout_reproduces_the_folder_and_it_verifies() {
    let mut fx = Fx::new();
    let mut rng = Rng::new(4);
    for i in 0..15u64 {
        let path = format!("d{}/f{i}", i % 4);
        let data = rng.bytes([0, 1, 5000, 40_000][i as usize % 4]);
        let kind = if i % 5 == 0 {
            Kind::Executable
        } else {
            Kind::Regular
        };
        fx.ws
            .write(&path, kind, &mut &data[..], OLD + i as i64)
            .unwrap();
    }
    fx.put("links/l", Kind::Symlink, b"../d0/f0");
    fx.put("e/f/", Kind::Directory, b"");
    fs::remove_file(fx.root().join("d1/f1")).unwrap();
    let head = fx.scan().head;
    let out = TempDir::new("checkout");
    let ws = PlainFolder::new(out.path());
    assert_eq!(checkout(&*fx.folder.store, &head, &ws).unwrap(), 16);
    let (mut want, got) = (fx.ws.list().unwrap(), ws.list().unwrap());
    want.retain(|(p, _)| !p.starts_with(".kide"));
    assert_eq!(want.len(), got.len());
    for ((p, a), (q, b)) in want.iter().zip(&got) {
        assert_eq!((p, a.kind, a.size), (q, b.kind, b.size));
        if a.kind != Kind::Symlink && a.kind != Kind::Directory {
            assert_eq!(a.mtime, b.mtime, "{p}");
        }
        let read = |w: &PlainFolder| {
            let mut v = Vec::new();
            w.open(p).unwrap().read_to_end(&mut v).unwrap();
            v
        };
        assert_eq!(read(&fx.ws), read(&ws), "{p}");
    }
    let r = verify(&*fx.folder.store, &head).unwrap();
    assert!(r.ok() && r.objects > 20, "{r:?}");
}

#[test]
fn verify_finds_corrupt_and_missing_objects() {
    let mut fx = Fx::new();
    fx.put("a", Kind::Regular, &[2; 3000]);
    fx.put("b", Kind::Regular, &[3; 3000]);
    let head = fx.scan().head;
    let store = fx.folder.store.clone();
    let chunk_of = |path: &str| {
        let root = fx.record(path).root;
        let node = load::node(&*store, &root).unwrap();
        node.children[0].id
    };
    let (bad, gone) = (chunk_of("a"), chunk_of("b"));
    let seg = fx.root().join(".kide/objects/00000001.seg");
    let mut bytes = fs::read(&seg).unwrap();
    let at = bytes.windows(32).position(|w| w == bad.0).unwrap() + 36;
    bytes[at] ^= 1;
    fs::write(&seg, bytes).unwrap();
    store.remove(&gone).unwrap();
    let r = verify(&*store, &head).unwrap();
    assert_eq!(r.corrupt, [bad]);
    assert_eq!(r.problems.len(), 2, "{r:?}");
    assert!(
        r.problems.iter().any(|p| p.starts_with("b: not found")),
        "{r:?}"
    );
}

fn files(fx: &Fx, n: usize) {
    for i in 0..n {
        fx.put(
            &format!("f{i:02}"),
            Kind::Regular,
            format!("{i}").as_bytes(),
        );
    }
}

fn refused(r: Result<ScanReport>) -> bool {
    matches!(r, Err(Error::Refused(_)))
}

#[test]
fn an_emptied_folder_is_refused_until_confirmed() {
    let mut fx = Fx::new();
    files(&fx, 3);
    let head = fx.scan().head;
    for i in 0..3 {
        fs::remove_file(fx.root().join(format!("f{i:02}"))).unwrap();
    }
    assert!(refused(fx.try_scan()));
    assert_eq!(fx.folder.head, head, "nothing recorded");
    assert!(fx.folder.index.0.values().all(|e| e.is_live()));
    guard::confirm(&fx.folder.kide(), &head).unwrap();
    assert_eq!(fx.scan().tombstones, 3);
    assert!(!fx.folder.kide().join("confirm").exists(), "used once");
}

#[test]
fn a_mass_deletion_is_refused_but_a_few_deletions_and_renames_are_not() {
    let mut fx = Fx::new();
    files(&fx, 30);
    fx.scan();
    for i in 0..5 {
        fs::remove_file(fx.root().join(format!("f{i:02}"))).unwrap();
    }
    assert_eq!(fx.scan().tombstones, 5, "a few deletions pass");
    fs::create_dir(fx.root().join("moved")).unwrap();
    for i in 5..25 {
        let name = format!("f{i:02}");
        fs::rename(fx.root().join(&name), fx.root().join("moved").join(&name)).unwrap();
    }
    assert_eq!(fx.scan().tombstones, 20, "a rename loses nothing net");
    fs::remove_dir_all(fx.root().join("moved")).unwrap();
    assert!(refused(fx.try_scan()), "20 of 25 gone at once");
}

#[test]
fn a_missing_root_is_refused() {
    let tmp = TempDir::new("scan");
    Folder::init(tmp.path()).unwrap();
    fs::write(tmp.path().join(".kide/config"), "folder = data\n").unwrap();
    let mut folder = Folder::open_unsynced(tmp.path()).unwrap();
    let ws = folder.workspace();
    let r = scan(&mut folder, &ws, &mut Rescan, &Platform::LINUX);
    assert!(refused(r));
    folder.deletions = Deletions::Allowed;
    fs::create_dir(tmp.path().join("data")).unwrap();
    assert!(scan(&mut folder, &ws, &mut Rescan, &Platform::LINUX).is_ok());
}

#[test]
fn restore_writes_vanished_files_back_and_records_no_deletion() {
    let mut fx = Fx::new();
    files(&fx, 3);
    fx.put("d/", Kind::Directory, b"");
    let head = fx.scan().head;
    for i in 0..3 {
        fs::remove_file(fx.root().join(format!("f{i:02}"))).unwrap();
    }
    fs::remove_dir(fx.root().join("d")).unwrap();
    fx.folder.deletions = Deletions::Restore;
    let r = fx.scan();
    assert_eq!((r.tombstones, r.written, r.head), (0, 4, head));
    assert_eq!(fs::read(fx.root().join("f01")).unwrap(), b"1");
    assert!(fx.root().join("d").is_dir());
}

#[test]
fn guard_off_records_an_emptied_folder() {
    let mut fx = Fx::new();
    files(&fx, 2);
    fx.scan();
    fx.folder.deletions = Deletions::Allowed;
    for i in 0..2 {
        fs::remove_file(fx.root().join(format!("f{i:02}"))).unwrap();
    }
    assert_eq!(fx.scan().tombstones, 2);
}
