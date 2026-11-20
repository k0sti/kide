//! Recovery (§5.5, §5.6): the guard on a lost folder, joining without
//! history, and resetting to a peer's state.

use crate::device::{Device, Net, assert_same, pair};
use kide::Error;
use kide::folder::guard::Deletions;
use kide::folder::verify::verify;
use kide::resolve::{Join, Reset};

/// `a` and `b` share x (edited twice), y, z and gone (deleted on `a`).
fn shared() -> (Net, Device, Device) {
    let (net, mut a, mut b) = pair();
    for p in ["x", "y", "z", "gone"] {
        a.write(p, format!("{p} 1\n").as_bytes());
    }
    net.sync(&mut a, &mut b);
    a.write("x", b"x 2\n");
    a.remove("gone");
    net.sync(&mut a, &mut b);
    a.write("x", b"x 3\n");
    a.scan();
    (net, a, b)
}

#[test]
fn a_device_without_history_joins_and_keeps_only_its_own_changes() {
    let (net, mut a, _) = shared();
    // `c` holds an old copy of the files but no `.kide` history.
    let mut c = Device::new("c");
    net.join(&c);
    c.write("x", b"x 2\n"); // stale: in a's history
    c.write("gone", b"gone 1\n"); // deleted on a since
    c.write("z", b"z local\n"); // changed here only
    c.write("mine", b"only here\n");
    c.try_pull_with(&net.0, "a", &Join).unwrap();
    assert_eq!(c.read("x").unwrap(), b"x 3\n", "the stale copy gives way");
    assert!(c.read("gone").is_none(), "a deletion stays a deletion");
    assert_eq!(c.read("z").unwrap(), b"z 1\n", "the peer's version wins");
    assert_eq!(c.copies().len(), 1, "{:?}", c.copies());
    assert_eq!(c.read(&c.copies()[0]).unwrap(), b"z local\n");
    assert_eq!(c.read("mine").unwrap(), b"only here\n");
    net.pull(&mut a, &mut c);
    assert_same(&[&a, &c]);
}

#[test]
fn an_emptied_folder_refuses_to_pull_until_reset() {
    let (net, mut a, mut b) = shared();
    b.folder.deletions = Deletions::Guarded;
    for p in ["x", "y", "z"] {
        b.remove(p);
    }
    let r = b.try_pull(&net.0, "a");
    assert!(matches!(r, Err(Error::Refused(_))), "{r:?}");
    b.folder.deletions = Deletions::Restore;
    b.try_pull_with(&net.0, "a", &Reset).unwrap();
    assert_same(&[&a, &b]);
    assert_eq!(b.read("x").unwrap(), b"x 3\n");
    net.pull(&mut a, &mut b);
    assert_eq!(a.read("y").unwrap(), b"y 1\n", "nothing was deleted");
}

#[test]
fn reset_discards_local_changes() {
    let (net, a, mut b) = shared();
    b.write("y", b"y edited here\n");
    b.write("new", b"new here\n");
    b.remove("z");
    b.try_pull_with(&net.0, "a", &Reset).unwrap();
    assert_eq!(b.files(), a.files());
    assert_eq!(b.read("y").unwrap(), b"y 1\n");
    assert!(b.read("new").is_none());
}

/// Flips a byte of the stored object that holds `needle` in `d`'s store.
fn corrupt(d: &Device, needle: &[u8]) {
    let dir = d.folder.kide().join("objects");
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let mut b = std::fs::read(e.path()).unwrap();
        if let Some(i) = b.windows(needle.len()).position(|w| w == needle) {
            b[i] ^= 0x20;
            std::fs::write(e.path(), b).unwrap();
            return;
        }
    }
    panic!("{} holds no {needle:?}", d.name);
}

#[test]
fn verify_drops_a_corrupt_object_and_the_next_pull_fetches_it() {
    let (net, mut a, mut b) = pair();
    a.write("x", b"corruptible content\n");
    net.sync(&mut a, &mut b);
    corrupt(&b, b"corruptible");
    let v = verify(&*b.folder.store, &b.head()).unwrap();
    assert_eq!(v.corrupt.len(), 1);
    assert!(b.folder.damaged());
    b.try_pull(&net.0, "a").unwrap();
    assert!(!b.folder.damaged());
    assert!(verify(&*b.folder.store, &b.head()).unwrap().ok());
}

#[test]
fn a_pull_that_meets_a_corrupt_object_repairs_and_merges() {
    let (net, mut a, mut b) = pair();
    a.write("x.md", b"corruptible content\n");
    net.sync(&mut a, &mut b);
    corrupt(&b, b"corruptible");
    a.write("x.md", b"corruptible content\na\n");
    a.scan();
    b.write("x.md", b"b\ncorruptible content\n");
    b.try_pull(&net.0, "a").unwrap();
    assert_eq!(b.read("x.md").unwrap(), b"b\ncorruptible content\na\n");
    assert!(verify(&*b.folder.store, &b.head()).unwrap().ok());
}
