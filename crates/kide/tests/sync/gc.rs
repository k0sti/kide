//! Garbage collection after synchronising (§5.7): what remains verifies,
//! conflicts included, and the devices go on synchronising.

use crate::device::{assert_same, pair};
use kide::folder::gc::gc;
use kide::folder::verify::verify;
use kide::time::now_secs;

#[test]
fn gc_after_a_sync_leaves_verify_clean() {
    let (net, mut a, mut b) = pair();
    a.write("notes.bin", b"base\n");
    a.write("old.txt", b"to be deleted\n");
    net.sync(&mut a, &mut b);
    a.write("notes.bin", b"edited on a\n");
    b.write("notes.bin", b"edited on b\n");
    b.remove("old.txt");
    a.scan();
    net.sync(&mut a, &mut b);
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
    assert_eq!(a.copies().len(), 1, "one conflict copy");
    // Past keep_days, only HEAD is a root.
    let later = now_secs() + 31 * 86_400;
    for d in [&a, &b] {
        let r = gc(&d.folder, later).unwrap();
        assert!(r.removed > 0, "{}: {r:?}", d.name);
        let v = verify(&*d.folder.store, &d.head()).unwrap();
        assert!(v.ok(), "{}: {v:?}", d.name);
    }
    b.write("notes.bin", b"after gc\n");
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
    assert_eq!(a.read("notes.bin").unwrap(), b"after gc\n");
}

#[test]
fn a_note_keeps_its_history_through_gc_and_merges_after_it() {
    let (net, mut a, mut b) = pair();
    a.write("n.md", b"1\n2\n3\n");
    net.sync(&mut a, &mut b);
    for i in 0..3 {
        a.write("n.md", format!("1\n2\n3\nv{i}\n").as_bytes());
        net.sync(&mut a, &mut b);
    }
    // Past keep_days, only HEAD is a root; the note's history keeps its
    // content, so the base of the next merge is still there.
    let later = now_secs() + 31 * 86_400;
    for d in [&a, &b] {
        gc(&d.folder, later).unwrap();
        let v = verify(&*d.folder.store, &d.head()).unwrap();
        assert!(v.ok(), "{}: {v:?}", d.name);
        let store = &*d.folder.store;
        let rows = kide::history::history(store, &d.folder.index.0["n.md"].version).unwrap();
        assert_eq!(rows.len(), 4);
        for r in rows {
            let old = kide::load::read_content(store, &r.record.root).unwrap();
            assert!(old.starts_with(b"1\n2\n3\n"), "{}: {old:?}", d.name);
        }
    }
    a.write("n.md", b"one\n2\n3\nv2\n");
    b.write("n.md", b"1\n2\nthree\nv2\n");
    a.scan();
    net.sync(&mut a, &mut b);
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
    assert_eq!(a.read("n.md").unwrap(), b"one\n2\nthree\nv2\n");
}
