//! Test devices: a folder in a temporary directory, its workspace and what
//! it serves, on an in-process network.

use kide::change::Rescan;
use kide::conformance::TempDir;
use kide::folder::Folder;
use kide::folder::guard::Deletions;
use kide::folder::net;
use kide::folder::pull::{PullReport, Pulling, pull};
use kide::folder::scan::{ScanReport, scan};
use kide::format::manifest::Kind;
use kide::hash::Hash;
use kide::iface::{Meta, Resolver, Transport, Workspace};
use kide::names::{Platform, platform_key};
use kide::resolve::DecisionTable;
use kide::transport::{InProcess, Served};
use kide::workspace::PlainFolder;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// Long ago, so that written files are not racy.
pub const OLD: i64 = 1_600_000_000_000_000_000;

/// A distinct, increasing mtime for every write, a second apart.
pub fn tick() -> i64 {
    static T: AtomicI64 = AtomicI64::new(0);
    OLD + 1_000_000_000 * T.fetch_add(1, Ordering::Relaxed)
}

pub struct Device {
    pub name: String,
    pub folder: Folder,
    pub ws: Box<dyn Workspace>,
    pub platform: Platform,
    head: Arc<Mutex<Hash>>,
    _tmp: TempDir,
}

/// One file on disk: kind, content and, for a regular file, mtime.
pub type Files = BTreeMap<String, (Kind, Vec<u8>, i64)>;

impl Device {
    pub fn new(name: &str) -> Device {
        let tmp = TempDir::new(&format!("sync-{name}"));
        Folder::init(tmp.path()).unwrap();
        let mut folder = Folder::open_unsynced(tmp.path()).unwrap();
        // These tests delete freely; the guard has its own (recovery.rs).
        folder.deletions = Deletions::Allowed;
        Device {
            name: name.into(),
            ws: Box::new(PlainFolder::new(tmp.path())),
            head: Arc::new(Mutex::new(folder.head)),
            folder,
            platform: Platform::LINUX,
            _tmp: tmp,
        }
    }

    pub fn root(&self) -> &Path {
        &self.folder.root
    }

    /// Makes this device case-insensitive, as macOS is by default.
    pub fn conflate(&mut self) {
        self.ws = Box::new(Conflating(
            PlainFolder::new(self.root()),
            self.root().into(),
        ));
        self.platform = Platform::MACOS;
    }

    pub fn served(&self) -> Served {
        let head = self.head.clone();
        net::served(self.folder.store.clone(), move || *head.lock().unwrap())
    }

    pub fn put(&self, path: &str, kind: Kind, data: &[u8]) -> i64 {
        let t = tick();
        self.ws.write(path, kind, &mut &data[..], t).unwrap();
        t
    }

    pub fn write(&self, path: &str, data: &[u8]) -> i64 {
        self.put(path, Kind::Regular, data)
    }

    pub fn remove(&self, path: &str) {
        let p = self.root().join(path.trim_end_matches('/'));
        match path.ends_with('/') {
            true => std::fs::remove_dir(p).unwrap(),
            false => std::fs::remove_file(p).unwrap(),
        }
    }

    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(self.root().join(path)).ok()
    }

    pub fn scan(&mut self) -> ScanReport {
        let r = scan(&mut self.folder, &*self.ws, &mut Rescan, &self.platform).unwrap();
        *self.head.lock().unwrap() = r.head;
        r
    }

    pub fn try_pull_with(
        &mut self,
        t: &dyn Transport,
        peer: &str,
        resolver: &dyn Resolver,
    ) -> kide::Result<PullReport> {
        let with = Pulling {
            ws: &*self.ws,
            source: &mut Rescan,
            resolver,
            transport: t,
            platform: &self.platform,
            review: None,
        };
        let r = pull(&mut self.folder, with, peer);
        *self.head.lock().unwrap() = self.folder.head;
        r
    }

    pub fn try_pull_reviewed(
        &mut self,
        t: &dyn Transport,
        peer: &str,
        review: &mut kide::folder::pull::Review<'_>,
    ) -> kide::Result<PullReport> {
        let with = Pulling {
            ws: &*self.ws,
            source: &mut Rescan,
            resolver: &DecisionTable,
            transport: t,
            platform: &self.platform,
            review: Some(review),
        };
        let r = pull(&mut self.folder, with, peer);
        *self.head.lock().unwrap() = self.folder.head;
        r
    }

    pub fn try_pull(&mut self, t: &dyn Transport, peer: &str) -> kide::Result<PullReport> {
        self.try_pull_with(t, peer, &DecisionTable)
    }

    pub fn head(&self) -> Hash {
        self.folder.head
    }

    /// Every listed path with its kind, content and synchronised mtime.
    pub fn files(&self) -> Files {
        let mut out = Files::new();
        for (path, m) in self.ws.list().unwrap() {
            let mut data = Vec::new();
            self.ws.open(&path).unwrap().read_to_end(&mut data).unwrap();
            let mtime = match m.kind {
                Kind::Regular | Kind::Executable => m.mtime,
                _ => 0,
            };
            out.insert(path, (m.kind, data, mtime));
        }
        out
    }

    /// The paths that are conflict copies.
    pub fn copies(&self) -> Vec<String> {
        let files = self.files().into_keys();
        files.filter(|p| p.contains(".kide-conflict-")).collect()
    }
}

/// A plain folder on a file system that conflates names with equal platform
/// keys: a path reaches whatever name on disk has its key, component by
/// component, and a write keeps the name that is there.
pub struct Conflating(PlainFolder, PathBuf);

impl Conflating {
    /// The name on disk that `path` reaches.
    fn on_disk(&self, path: &str) -> String {
        let (mut at, mut out) = (self.1.clone(), Vec::new());
        for c in path.trim_end_matches('/').split('/') {
            let names = fs::read_dir(&at).into_iter().flatten().flatten();
            let mut names = names.filter_map(|e| e.file_name().into_string().ok());
            let name = names.find(|n| platform_key(n) == platform_key(c));
            let name = name.unwrap_or_else(|| c.to_string());
            at.push(&name);
            out.push(name);
        }
        let slash = if path.ends_with('/') { "/" } else { "" };
        format!("{}{slash}", out.join("/"))
    }
}

impl Workspace for Conflating {
    fn list(&self) -> kide::Result<Vec<(String, Meta)>> {
        self.0.list()
    }
    fn stat(&self, path: &str) -> kide::Result<Option<Meta>> {
        self.0.stat(&self.on_disk(path))
    }
    fn open(&self, path: &str) -> kide::Result<Box<dyn Read + Send>> {
        self.0.open(&self.on_disk(path))
    }
    fn write(&self, path: &str, kind: Kind, r: &mut dyn Read, mtime: i64) -> kide::Result<Meta> {
        self.0.write(&self.on_disk(path), kind, r, mtime)
    }
    fn set_mtime(&self, path: &str, mtime: i64) -> kide::Result<Meta> {
        self.0.set_mtime(&self.on_disk(path), mtime)
    }
    fn rename(&self, from: &str, to: &str) -> kide::Result<()> {
        self.0.rename(&self.on_disk(from), &self.on_disk(to))
    }
    fn trash(&self, path: &str) -> kide::Result<()> {
        self.0.trash(&self.on_disk(path))
    }
}

/// Devices on an in-process network, registered by name.
#[derive(Default)]
pub struct Net(pub InProcess);

impl Net {
    pub fn join(&self, dev: &Device) {
        self.0.register(&dev.name, dev.served());
    }

    /// `to` pulls from `from`, which scans first so that it serves its edits.
    pub fn pull(&self, to: &mut Device, from: &mut Device) -> PullReport {
        from.scan();
        to.try_pull(&self.0, &from.name)
            .unwrap_or_else(|e| panic!("{} pulls from {}: {e}", to.name, from.name))
    }

    /// Each device pulls from the other.
    pub fn sync(&self, a: &mut Device, b: &mut Device) {
        self.pull(a, b);
        self.pull(b, a);
    }
}

/// Two devices on one network.
/// Whether the file system of the temporary directories folds case (APFS,
/// NTFS by default): tests that give a device a platform which keeps names
/// apart cannot run on it.
pub fn folds_case() -> bool {
    let tmp = kide::conformance::TempDir::new("case");
    std::fs::write(tmp.path().join("Probe"), b"").unwrap();
    tmp.path().join("probe").exists()
}

pub fn pair() -> (Net, Device, Device) {
    let (net, a, b) = (Net::default(), Device::new("a"), Device::new("b"));
    net.join(&a);
    net.join(&b);
    (net, a, b)
}

/// Asserts that the devices hold the same files and the same `HEAD`.
pub fn assert_same(devs: &[&Device]) {
    let (first, rest) = devs.split_first().unwrap();
    for d in rest {
        assert_eq!(
            d.files(),
            first.files(),
            "{} and {} differ",
            d.name,
            first.name
        );
        assert_eq!(
            d.head(),
            first.head(),
            "{} and {}: HEAD",
            d.name,
            first.name
        );
    }
}
