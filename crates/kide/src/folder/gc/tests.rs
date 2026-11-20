use super::*;
use crate::change::Rescan;
use crate::conformance::TempDir;
use crate::folder::scan::scan;
use crate::folder::verify::verify;
use crate::hash::{Hash, h};
use crate::iface::ObjectStore;
use crate::load;
use crate::names::Platform;
use crate::store::PackStore;
use crate::time::now_secs;
use crate::workspace::PlainFolder;

struct Fx {
    tmp: TempDir,
    folder: Folder,
    ws: PlainFolder,
}

impl Fx {
    fn new() -> Fx {
        let tmp = TempDir::new("gc");
        Folder::init(tmp.path()).unwrap();
        let folder = Folder::open_unsynced(tmp.path()).unwrap();
        let ws = PlainFolder::new(tmp.path());
        Fx { tmp, folder, ws }
    }

    fn edit(&mut self, path: &str, data: &[u8]) {
        fs::write(self.tmp.path().join(path), data).unwrap();
        scan(&mut self.folder, &self.ws, &mut Rescan, &Platform::LINUX).unwrap();
    }

    /// The content roots of `path`'s current record and its first ancestors.
    fn history(&self, path: &str) -> Vec<(Hash, Hash)> {
        let store = &*self.folder.store;
        let mut id = self.folder.index.0[path].version;
        let mut out = Vec::new();
        loop {
            let r = load::record(store, &id).unwrap();
            out.push((id, r.root));
            match r.parents.first() {
                Some(p) => id = *p,
                None => return out,
            }
        }
    }
}

#[test]
fn retained_roots_keep_old_content_until_keep_days() {
    let mut fx = Fx::new();
    for i in 0..3 {
        fx.edit("f.bin", format!("version {i}\n").as_bytes());
    }
    fx.edit("g", b"kept\n");
    let history = fx.history("f.bin");
    assert_eq!(history.len(), 3);
    assert_eq!(fx.folder.roots().unwrap().len(), 4, "init's root and three");
    let now = now_secs();
    let r = gc(&fx.folder, now).unwrap();
    assert_eq!((r.removed, r.roots_dropped), (0, 0), "{r:?}");

    let later = now + 31 * DAY;
    let r = gc(&fx.folder, later).unwrap();
    assert_eq!(r.roots_dropped, 4);
    assert!(fx.folder.roots().unwrap().is_empty());
    // Two old contents of f and three old manifests, a node and a chunk
    // each, and the empty manifest of init, a node.
    assert_eq!(r.removed, 2 * 2 + 3 * 2 + 1, "{r:?}");
    let store = &*fx.folder.store;
    for (i, (record, root)) in history.iter().enumerate() {
        assert!(store.has(record).unwrap(), "history record {i} stays");
        assert_eq!(store.has(root).unwrap(), i == 0, "content {i}");
    }
    assert!(verify(store, &fx.folder.head).unwrap().ok());
    assert_eq!(gc(&fx.folder, later).unwrap().removed, 0);

    // Removed objects stay absent after the store is reopened.
    let gone = history[1].1;
    let head = fx.folder.head;
    drop(fx.folder);
    let store = PackStore::open(fx.tmp.path().join(".kide/objects")).unwrap();
    assert!(!store.has(&gone).unwrap());
    assert_eq!(store.ids().unwrap().len(), r.kept);
    assert!(verify(&store, &head).unwrap().ok());
}

#[test]
fn staging_is_cleared_and_old_trash_deleted() {
    let fx = Fx::new();
    let staging = fx.folder.staging().unwrap();
    staging.put_known(&h(b"staged"), b"staged").unwrap();
    drop(staging);
    let trash = fx.tmp.path().join(".kide/trash");
    let now = 1000 + 31 * DAY;
    let recent = (now - DAY).to_string();
    for slot in ["1000", "1000-1", recent.as_str(), "notes", "1000-x"] {
        fs::create_dir_all(trash.join(slot).join("d")).unwrap();
        fs::write(trash.join(slot).join("d/f"), b"x").unwrap();
    }
    let r = gc(&fx.folder, now).unwrap();
    assert_eq!((r.unstaged, r.trash_removed), (1, 2), "{r:?}");
    assert!(fx.folder.staging().unwrap().ids().unwrap().is_empty());
    let mut left: Vec<_> = fs::read_dir(&trash)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, ["1000-x", recent.as_str(), "notes"]);
}

#[test]
fn slot_names() {
    assert_eq!(slot_time("1700000000"), Some(1_700_000_000));
    assert_eq!(slot_time("1700000000-3"), Some(1_700_000_000));
    for bad in ["", "-1", "17-", "17-a", "x17", "17 "] {
        assert_eq!(slot_time(bad), None, "{bad:?}");
    }
}

#[test]
fn a_read_only_folder_is_refused() {
    let fx = Fx::new();
    let ro = Folder::open_read_only(fx.tmp.path()).unwrap();
    assert!(matches!(gc(&ro, 0), Err(crate::Error::Invalid(_))));
}
