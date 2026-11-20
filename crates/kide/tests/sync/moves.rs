use super::device::Device;
use kide::folder::{moves::move_path, restore::restore_tree};
use kide::format::change::Change;
use kide::history::{Event, history};
use kide::load;

fn version(d: &Device, path: &str) -> kide::Hash {
    d.folder.index.0[path].version
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
