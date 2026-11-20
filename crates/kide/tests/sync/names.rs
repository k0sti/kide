//! Entries a device cannot represent (§5.3): held, never written, never
//! read as deletions, reported, and written once they no longer collide;
//! the entries already on disk are the ones shown.

use crate::device::{assert_same, folds_case, pair};
use kide::names::Platform;
use kide::report::{Why, reported_paths};

#[test]
fn colliding_entries_are_held_until_they_no_longer_collide() {
    if folds_case() {
        return; // b must keep `Held-Dir` and `held-dir` apart
    }
    let (net, mut a, mut b) = pair();
    a.platform = Platform::MACOS; // on a file system that keeps names apart
    for p in ["Held-Dir/x", "held-dir/y", "README-held", "readme-held"] {
        b.write(p, p.as_bytes());
    }
    net.pull(&mut a, &mut b);
    assert_eq!(a.head(), b.head(), "held entries stay in the manifest");
    assert!(a.read("held-dir/y").is_none() && a.read("readme-held").is_none());
    assert_eq!(a.read("Held-Dir/x").unwrap(), b"Held-Dir/x");
    let held = reported_paths(Why::Held);
    for p in ["held-dir/y", "readme-held"] {
        assert!(held.contains(&p.to_string()), "{p} is reported: {held:?}");
    }
    assert_eq!(a.scan().tombstones, 0, "absence is no deletion");
    // An edit of a held entry is recorded, not written.
    b.write("held-dir/y", b"edited\n");
    net.pull(&mut a, &mut b);
    assert_eq!(a.head(), b.head());
    assert!(a.read("held-dir/y").is_none());
    // Once the shown entry is gone, the held one is written.
    // (Removing only `Held-Dir/x` would leave `Held-Dir/`, which collides.)
    std::fs::remove_dir_all(b.root().join("Held-Dir")).unwrap();
    net.pull(&mut a, &mut b);
    assert_eq!(a.read("held-dir/y").unwrap(), b"edited\n");
    assert!(a.read("Held-Dir/x").is_none());
    net.sync(&mut a, &mut b);
    assert_eq!(a.head(), b.head());
    // The other held entry goes the same way.
    b.remove("README-held");
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
}

#[test]
fn a_file_on_disk_keeps_its_name_against_a_smaller_new_one() {
    if folds_case() {
        return; // b, as a platform that keeps names apart, must be able to
    }
    let (net, mut a, mut b) = pair();
    a.conflate();
    a.write("shown-first", b"local\n");
    b.write("Shown-first", b"remote\n"); // sorts first, but is new to a
    a.scan();
    net.pull(&mut a, &mut b);
    net.pull(&mut b, &mut a);
    assert_eq!(a.head(), b.head(), "converged");
    assert_eq!(a.read("shown-first").unwrap(), b"local\n");
    assert!(a.read("Shown-first").is_none(), "held, not written");
    let held = reported_paths(Why::Held);
    assert!(held.contains(&"Shown-first".to_string()), "{held:?}");
    let head = a.head();
    net.sync(&mut a, &mut b);
    assert_eq!((a.head(), b.head()), (head, head), "and stays converged");
    // Once the local file is gone, the scan that sees it writes the other.
    a.remove("shown-first");
    let r = a.scan();
    assert_eq!((r.tombstones, r.written, r.held.len()), (1, 1, 0), "{r:?}");
    assert_eq!(a.read("Shown-first").unwrap(), b"remote\n");
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
}
