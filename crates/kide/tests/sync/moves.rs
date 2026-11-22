use super::device::Device;
use kide::folder::{moves::move_path, restore::restore_tree};
use kide::format::change::Change;
use kide::history::{Event, history};
use kide::load;

fn version(d: &Device, path: &str) -> kide::Hash {
    d.folder.index.0[path].version
}

#[test]
fn moving_into_an_empty_parent_retains_its_directory_history() {
    use kide::format::manifest::Kind;
    for (from, to, old_path, new_path) in [
        ("a", "empty/a", "a", "empty/a"),
        ("source/", "empty/source/", "source/a", "empty/source/a"),
    ] {
        let mut a = Device::new("move-empty-parent");
        a.write(old_path, b"kept");
        a.put("empty/", Kind::Directory, b"");
        a.scan();
        let earlier = a.folder.head;
        let directory = version(&a, "empty/");
        let source = version(&a, old_path);
        let head = move_path(&mut a.folder, &*a.ws, from, to).unwrap();
        assert_eq!(a.read(new_path).unwrap(), b"kept");
        let parent = load::record(&*a.folder.store, &version(&a, "empty/")).unwrap();
        assert!(parent.is_tombstone());
        assert_eq!(parent.parents, [directory]);
        let rows = history(&*a.folder.store, &version(&a, new_path)).unwrap();
        assert_eq!((rows[0].event, rows[1].id), (Event::Move, source));
        load::validate_manifest(&*a.folder.store, &head).unwrap();
        assert_eq!(a.scan().head, head);
        let report = restore_tree(&mut a.folder, &*a.ws, &earlier, &head).unwrap();
        assert!(report.skipped.is_empty());
        assert_eq!(a.read(old_path).unwrap(), b"kept");
        assert!(a.root().join("empty").read_dir().unwrap().next().is_none());
        assert_eq!(a.folder.index.0["empty/"].kind, Some(Kind::Directory));
    }
}

#[test]
fn file_move_publishes_both_ends_and_survives_reopen() {
    let mut a = Device::new("move-file-reopen");
    a.write("old/a.bin", b"\0\xffbinary");
    a.scan();
    let before = a.folder.head;
    let original = version(&a, "old/a.bin");
    let value = load::record(&*a.folder.store, &original).unwrap();
    let mtime = a.folder.index.0["old/a.bin"].mtime;
    let head = move_path(&mut a.folder, &*a.ws, "old/a.bin", "new/a.bin").unwrap();
    assert_ne!(head, before);
    let reader = kide::folder::Folder::open_read_only(a.root()).unwrap();
    assert_eq!(reader.head, head);
    let entries = load::validate_manifest(&*reader.store, &head).unwrap();
    assert_eq!(entries.len(), 2);
    for path in ["old/a.bin", "new/a.bin"] {
        let entry = entries.iter().find(|e| e.path == path).unwrap();
        let rows = history(&*reader.store, &entry.version).unwrap();
        assert_eq!(rows[0].event, Event::Move);
        assert_eq!(rows[0].record.parents, [original]);
        assert_eq!(rows[0].record.is_tombstone(), path == "old/a.bin");
    }
    let moved = load::record(&*reader.store, &version(&a, "new/a.bin")).unwrap();
    assert_eq!(moved.value(), value.value());
    assert_eq!(reader.index.0["new/a.bin"].mtime, mtime);
    assert_eq!(a.scan().head, head, "a subsequent scan adds no changes");
}

#[test]
fn directory_move_and_restore_preserve_empty_directories_and_file_kinds() {
    use kide::format::manifest::Kind;
    let mut a = Device::new("move-kinds");
    a.put("old/empty/", Kind::Directory, b"");
    a.put("old/run", Kind::Executable, b"#!/bin/sh\n");
    #[cfg(unix)]
    a.put("old/link", Kind::Symlink, b"run");
    a.scan();
    let earlier = a.folder.head;
    let files = a.files();
    let originals = a.folder.index.clone();
    move_path(&mut a.folder, &*a.ws, "old/", "new/").unwrap();
    for (path, original) in &originals.0 {
        let to = path.replacen("old/", "new/", 1);
        let entry = a.folder.index.0[&to];
        assert_eq!(entry.kind, original.kind);
        let rows = history(&*a.folder.store, &entry.version).unwrap();
        assert_eq!((rows[0].event, rows[1].id), (Event::Move, original.version));
    }
    let expected = a.folder.head;
    assert!(
        restore_tree(&mut a.folder, &*a.ws, &earlier, &expected)
            .unwrap()
            .skipped
            .is_empty()
    );
    assert_eq!(a.files(), files);
}

#[test]
fn a_disk_rename_cannot_be_recorded_as_an_explicit_move_afterwards() {
    let mut a = Device::new("move-already-on-disk");
    a.write("old/a", b"kept");
    a.scan();
    let original = version(&a, "old/a");
    let before = a.folder.head;
    a.ws.rename("old/", "new/").unwrap();
    assert!(matches!(
        move_path(&mut a.folder, &*a.ws, "old/", "new/"),
        Err(kide::Error::Invalid(_))
    ));
    assert_eq!(a.folder.head, before);
    assert_eq!(a.read("new/a").unwrap(), b"kept");
    a.scan();
    let old = history(&*a.folder.store, &version(&a, "old/a")).unwrap();
    assert_eq!((old[0].event, old[1].id), (Event::Delete, original));
    let new = history(&*a.folder.store, &version(&a, "new/a")).unwrap();
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].event, Event::New);
}

#[test]
fn relocating_the_workspace_root_keeps_relative_paths_and_history() {
    use kide::change::Rescan;
    use kide::conformance::TempDir;
    use kide::folder::{Folder, scan::scan};
    use kide::names::Platform;
    let tmp = TempDir::new("move-workspace-root");
    let state = tmp.path().join("state");
    Folder::init(&state).unwrap();
    let config = state.join(".kide/config");
    std::fs::write(&config, "folder = ../old\n").unwrap();
    std::fs::create_dir(tmp.path().join("old")).unwrap();
    std::fs::write(tmp.path().join("old/a"), b"kept").unwrap();
    let mut f = Folder::open_unsynced(&state).unwrap();
    let ws = f.workspace();
    scan(&mut f, &ws, &mut Rescan, &Platform::current()).unwrap();
    let head = f.head;
    let original = f.index.0["a"].version;
    drop(f);
    std::fs::rename(tmp.path().join("old"), tmp.path().join("new")).unwrap();
    std::fs::write(config, "folder = ../new\n").unwrap();
    let mut f = Folder::open_unsynced(&state).unwrap();
    let ws = f.workspace();
    assert_eq!(
        scan(&mut f, &ws, &mut Rescan, &Platform::current())
            .unwrap()
            .head,
        head
    );
    assert_eq!(f.index.0["a"].version, original);
    assert_eq!(history(&*f.store, &original).unwrap().len(), 1);
}

#[test]
fn nested_move_keeps_lineage_and_restores_forward() {
    let mut a = Device::new("move-nested");
    a.write("old/nested/a.txt", b"before\n");
    a.write("old/b.bin", b"same");
    a.scan();
    let earlier = a.folder.head;
    let original = version(&a, "old/nested/a.txt");
    let inode = a.ws.stat("old/nested/a.txt").unwrap().unwrap().inode;
    move_path(&mut a.folder, &*a.ws, "old/", "new/").unwrap();
    assert_eq!(a.read("old/nested/a.txt"), None);
    assert_eq!(a.read("new/nested/a.txt").unwrap(), b"before\n");
    assert_eq!(a.ws.stat("new/nested/a.txt").unwrap().unwrap().inode, inode);
    let moved = version(&a, "new/nested/a.txt");
    let rows = history(&*a.folder.store, &moved).unwrap();
    assert_eq!((rows[0].event, rows[1].id), (Event::Move, original));
    assert_eq!(
        Change::decode(&rows[0].record.change).unwrap(),
        Some(Change::Move {
            from: "old/".into(),
            to: "new/".into()
        })
    );
    a.write("new/nested/a.txt", b"intervening edit\n");
    a.scan();
    let edited = version(&a, "new/nested/a.txt");
    let expected = a.folder.head;
    let r = restore_tree(&mut a.folder, &*a.ws, &earlier, &expected).unwrap();
    assert!(r.skipped.is_empty());
    assert_ne!(
        a.folder.head, earlier,
        "restore must publish new forward records"
    );
    assert_eq!(a.read("old/nested/a.txt").unwrap(), b"before\n");
    assert_eq!(a.read("new/nested/a.txt"), None);
    assert_eq!(
        load::record(&*a.folder.store, &version(&a, "new/nested/a.txt"))
            .unwrap()
            .parents,
        [edited]
    );
    assert_eq!(
        history(&*a.folder.store, &version(&a, "old/nested/a.txt")).unwrap()[0].event,
        Event::Restore
    );
}

#[test]
fn equal_content_copies_never_infer_a_move() {
    let mut a = Device::new("same-content");
    a.write("a.txt", b"same\n");
    a.write("b.txt", b"same\n");
    a.scan();
    move_path(&mut a.folder, &*a.ws, "a.txt", "renamed.txt").unwrap();
    a.write("b.txt", b"only b changed\n");
    a.scan();
    let b = version(&a, "b.txt");
    let rows = history(&*a.folder.store, &version(&a, "renamed.txt")).unwrap();
    assert!(!rows.iter().any(|r| r.id == b));
    a.write("copy.txt", b"same\n");
    a.scan();
    assert_eq!(
        history(&*a.folder.store, &version(&a, "copy.txt")).unwrap()[0].event,
        Event::New
    );
}

#[test]
fn move_refuses_collisions_unscanned_changes_and_overlap() {
    let mut a = Device::new("move-refuse");
    a.write("old/a", b"original");
    a.write("occupied", b"kept");
    a.scan();
    let before = a.folder.head;
    assert!(move_path(&mut a.folder, &*a.ws, "old/", "old/sub/").is_err());
    assert!(move_path(&mut a.folder, &*a.ws, "old/a", "occupied").is_err());
    a.write("old/a", b"not scanned");
    assert!(move_path(&mut a.folder, &*a.ws, "old/", "new/").is_err());
    assert_eq!(a.folder.head, before);
    assert_eq!(a.read("old/a").unwrap(), b"not scanned");
    assert_eq!(a.read("occupied").unwrap(), b"kept");
}

#[test]
fn restore_refuses_stale_expectations_and_unscanned_edits() {
    let mut a = Device::new("restore-refuse");
    a.write("a", b"first");
    a.scan();
    let earlier = a.folder.head;
    a.write("a", b"second");
    a.scan();
    let current = a.folder.head;
    assert!(restore_tree(&mut a.folder, &*a.ws, &earlier, &earlier).is_err());
    a.write("a", b"unscanned");
    assert!(restore_tree(&mut a.folder, &*a.ws, &earlier, &current).is_err());
    assert_eq!(a.read("a").unwrap(), b"unscanned");
    assert_eq!(a.folder.head, current);
}

#[test]
fn concurrent_rename_and_edit_preserve_both_contents_and_converge() {
    use super::device::{assert_same, pair};
    let (net, mut a, mut b) = pair();
    a.write("old.txt", b"before\n");
    net.sync(&mut a, &mut b);
    move_path(&mut a.folder, &*a.ws, "old.txt", "new.txt").unwrap();
    b.write("old.txt", b"concurrent edit\n");
    b.scan();
    for _ in 0..2 {
        net.sync(&mut a, &mut b);
    }
    assert_same(&[&a, &b]);
    let values: Vec<_> = a.files().into_values().map(|(_, b, _)| b).collect();
    assert!(values.contains(&b"before\n".to_vec()));
    assert!(values.contains(&b"concurrent edit\n".to_vec()));
    assert_eq!(a.read("new.txt").unwrap(), b"before\n");
}

#[test]
fn concurrent_moves_retain_both_destinations() {
    use super::device::{assert_same, pair};
    let (net, mut a, mut b) = pair();
    a.write("old.bin", b"kept");
    net.sync(&mut a, &mut b);
    move_path(&mut a.folder, &*a.ws, "old.bin", "one.bin").unwrap();
    move_path(&mut b.folder, &*b.ws, "old.bin", "two.bin").unwrap();
    for _ in 0..2 {
        net.sync(&mut a, &mut b);
    }
    assert_same(&[&a, &b]);
    assert_eq!(a.read("one.bin").unwrap(), b"kept");
    assert_eq!(a.read("two.bin").unwrap(), b"kept");
}

#[test]
fn an_explicit_move_back_keeps_the_deleted_destination_history() {
    let mut a = Device::new("move-back");
    a.write("a", b"kept");
    a.scan();
    move_path(&mut a.folder, &*a.ws, "a", "b").unwrap();
    let moved = version(&a, "b");
    let deleted = version(&a, "a");
    move_path(&mut a.folder, &*a.ws, "b", "a").unwrap();
    let rows = history(&*a.folder.store, &version(&a, "a")).unwrap();
    assert_eq!(rows[0].event, Event::Move);
    assert!(rows.iter().any(|r| r.id == moved));
    assert!(rows.iter().any(|r| r.id == deleted));
}

#[test]
fn forward_restore_syncs_and_retains_move_history_after_gc() {
    use super::device::{assert_same, pair};
    let (net, mut a, mut b) = pair();
    a.write("old/a.txt", b"before\n");
    net.sync(&mut a, &mut b);
    let earlier = a.folder.head;
    move_path(&mut a.folder, &*a.ws, "old/", "new/").unwrap();
    net.sync(&mut a, &mut b);
    let moved = version(&a, "new/a.txt");
    a.write("new/a.txt", b"later\n");
    a.scan();
    let expected = a.folder.head;
    restore_tree(&mut a.folder, &*a.ws, &earlier, &expected).unwrap();
    for _ in 0..2 {
        net.sync(&mut a, &mut b);
    }
    assert_same(&[&a, &b]);
    kide::folder::gc::gc(&a.folder, kide::time::now_secs()).unwrap();
    assert!(
        history(&*a.folder.store, &version(&a, "new/a.txt"))
            .unwrap()
            .iter()
            .any(|r| r.id == moved && r.event == Event::Move)
    );
    assert_eq!(a.read("old/a.txt").unwrap(), b"before\n");
}

#[test]
fn a_move_into_deleted_history_remains_visible_after_a_concurrent_merge() {
    use super::device::pair;
    let (net, mut a, mut b) = pair();
    a.write("source.bin", b"source");
    a.write("target.bin", b"old target");
    net.sync(&mut a, &mut b);
    a.remove("target.bin");
    a.scan();
    net.sync(&mut a, &mut b);
    move_path(&mut a.folder, &*a.ws, "source.bin", "target.bin").unwrap();
    let moved = version(&a, "target.bin");
    b.write("target.bin", b"concurrent");
    b.scan();
    for _ in 0..2 {
        net.sync(&mut a, &mut b);
    }
    let rows = history(&*a.folder.store, &version(&a, "target.bin")).unwrap();
    assert!(rows.iter().any(|r| r.id == moved && r.event == Event::Move));
}

#[test]
fn restore_with_a_missing_object_writes_nothing() {
    let mut a = Device::new("restore-missing");
    a.write("a", b"earlier a");
    a.write("z", b"earlier z");
    a.scan();
    let earlier = a.folder.head;
    let missing = load::record(&*a.folder.store, &version(&a, "z"))
        .unwrap()
        .root;
    a.write("a", b"current a");
    a.write("z", b"current z");
    a.scan();
    let expected = a.folder.head;
    a.folder.store.remove(&missing).unwrap();
    let result = restore_tree(&mut a.folder, &*a.ws, &earlier, &expected);
    assert!(matches!(result, Err(kide::Error::NotFound(h)) if h==missing));
    assert_eq!(a.read("a").unwrap(), b"current a");
    assert_eq!(a.read("z").unwrap(), b"current z");
    assert_eq!(a.folder.head, expected);
}

#[test]
fn ignored_move_descendants_are_refused_without_changing_the_tree() {
    let mut a = Device::new("move-ignored");
    a.folder.config.rules.add(false, "old/private.bin").unwrap();
    a.write("old/a", b"kept");
    a.write("old/private.bin", b"local only");
    a.scan();
    let expected = a.folder.head;
    assert!(move_path(&mut a.folder, &*a.ws, "old/", "new/").is_err());
    assert_eq!(a.folder.head, expected);
    assert_eq!(a.read("old/private.bin").unwrap(), b"local only");
}

#[test]
fn an_unlisted_destination_collision_leaves_no_pending_move() {
    let mut a = Device::new("move-hidden-collision");
    a.write("source", b"kept");
    a.scan();
    std::fs::create_dir(a.root().join("target")).unwrap();
    std::fs::write(a.root().join("target/skipped.kide-tmp"), b"local").unwrap();
    let expected = a.folder.head;
    assert!(move_path(&mut a.folder, &*a.ws, "source", "target").is_err());
    assert!(!a.folder.kide().join("move-intent").exists());
    assert_eq!(a.folder.head, expected);
    assert_eq!(a.read("source").unwrap(), b"kept");
    a.scan();
}
