use super::*;
use crate::change::Rescan;
use crate::conformance::TempDir;
use crate::folder::scan::{scan, store_content};
use crate::format::record::Record;
use crate::names::Platform;
use crate::workspace::PlainFolder;
use std::fs;

struct Fx {
    _tmp: TempDir,
    folder: Folder,
    ws: PlainFolder,
}

impl Fx {
    fn new() -> Fx {
        let tmp = TempDir::new("apply");
        Folder::init(tmp.path()).unwrap();
        let folder = Folder::open_unsynced(tmp.path()).unwrap();
        let ws = PlainFolder::new(tmp.path());
        Fx {
            _tmp: tmp,
            folder,
            ws,
        }
    }

    fn scan(&mut self) {
        scan(&mut self.folder, &self.ws, &mut Rescan, &Platform::LINUX).unwrap();
    }

    /// A plan that replaces `path` with new content after its current version.
    fn replace(&self, path: &str, data: &[u8]) -> Plan {
        let store = &*self.folder.store;
        let root = store_content(store, &mut &data[..]).unwrap();
        let parent = self.folder.index.0[path].version;
        let r = Record {
            change: Vec::new(),
            parents: vec![parent],
            root: root.id,
            state: Kind::Regular.state(),
        };
        let version = store.put(&r.encode().unwrap()).unwrap();
        let mut plan = Plan::default();
        for (p, e) in &self.folder.index.0 {
            let t = Target {
                version: e.version,
                kind: e.kind,
                size: e.size,
                mtime: e.mtime,
            };
            plan.target.insert(p.clone(), t);
        }
        let t = Target {
            version,
            kind: Some(Kind::Regular),
            size: data.len() as u64,
            mtime: 1_600_000_000_000_000_000,
        };
        plan.target.insert(path.into(), t);
        plan
    }

    fn apply(&mut self, plan: &Plan) -> ApplyReport {
        apply(&mut self.folder, &self.ws, plan).unwrap()
    }
}

#[test]
fn a_racy_file_is_checked_by_its_content() {
    let mut fx = Fx::new();
    let now = time::now();
    let path = fx.folder.root.join("r.txt");
    fx.ws
        .write("r.txt", Kind::Regular, &mut &b"aaaa\n"[..], now)
        .unwrap();
    fx.scan();
    // The same size, mtime and inode, other content: only reading tells.
    fs::write(&path, b"bbbb\n").unwrap();
    fx.ws.set_mtime("r.txt", now).unwrap();
    let plan = fx.replace("r.txt", b"remote\n");
    assert_eq!(fx.apply(&plan).skipped, ["r.txt"]);
    assert_eq!(fs::read(&path).unwrap(), b"bbbb\n");
    // Unchanged content passes the check.
    fs::write(&path, b"aaaa\n").unwrap();
    fx.ws.set_mtime("r.txt", now).unwrap();
    let r = fx.apply(&plan);
    assert_eq!((r.written, r.skipped.len()), (1, 0), "{r:?}");
    assert_eq!(fs::read(&path).unwrap(), b"remote\n");
}

#[test]
fn applying_writes_the_index_then_head_and_retains_the_old_root() {
    let mut fx = Fx::new();
    fx.ws
        .write("f", Kind::Regular, &mut &b"one\n"[..], 7)
        .unwrap();
    fx.scan();
    let before = fx.folder.head;
    let plan = fx.replace("f", b"two\n");
    let r = fx.apply(&plan);
    assert_eq!(r.head, fx.folder.head);
    assert_ne!(r.head, before);
    let roots: Vec<Hash> = fx
        .folder
        .roots()
        .unwrap()
        .into_iter()
        .map(|r| r.1)
        .collect();
    assert_eq!(roots.last(), Some(&before), "the replaced HEAD is retained");
    let saved = Index::load(&fx.folder.kide().join("index")).unwrap();
    assert_eq!(saved, fx.folder.index, "the index is on disk");
    let head = fs::read_to_string(fx.folder.kide().join("HEAD")).unwrap();
    assert_eq!(head.trim(), r.head.to_hex());
    fx.scan();
    assert_eq!(fx.folder.head, r.head, "the next scan finds nothing new");
}

#[test]
fn a_failed_write_skips_only_that_path() {
    let mut fx = Fx::new();
    fx.ws
        .write("x", Kind::Regular, &mut &b"x\n"[..], 7)
        .unwrap();
    fx.ws
        .write("y", Kind::Regular, &mut &b"y\n"[..], 7)
        .unwrap();
    fx.scan();
    let mut plan = fx.replace("x", b"remote x\n");
    let y = fx.replace("y", b"remote y\n").target["y"];
    plan.target.insert("y".into(), y);
    // x becomes a directory holding a file after the scan.
    fs::remove_file(fx.folder.root.join("x")).unwrap();
    fs::create_dir(fx.folder.root.join("x")).unwrap();
    fs::write(fx.folder.root.join("x/mine"), b"mine").unwrap();
    let r = fx.apply(&plan);
    assert_eq!(r.skipped, ["x"]);
    assert_eq!(fs::read(fx.folder.root.join("y")).unwrap(), b"remote y\n");
    assert_eq!(fs::read(fx.folder.root.join("x/mine")).unwrap(), b"mine");
}
