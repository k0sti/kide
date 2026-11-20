//! Conformance suite for `ChangeSource` (§5.4, §5.5).

use super::workspace::MTIME;
use crate::format::manifest::Kind;
use crate::iface::{ChangeSource, Checked, CheckedIndex, Workspace};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A new, empty workspace, the directory that holds it, and a change source
/// for it.
pub struct ChangeFixture {
    pub ws: Box<dyn Workspace>,
    pub root: PathBuf,
    pub source: Box<dyn ChangeSource>,
}

const SEED: [(&str, Kind, &[u8]); 9] = [
    ("keep.txt", Kind::Regular, b"unchanged"),
    ("edit.txt", Kind::Regular, b"before"),
    ("touch.txt", Kind::Regular, b"touched"),
    ("gone.txt", Kind::Regular, b"deleted"),
    ("old/name.txt", Kind::Regular, b"renamed"),
    ("mode.sh", Kind::Regular, b"#!/bin/sh\n"),
    ("link", Kind::Symlink, b"keep.txt"),
    ("empty/", Kind::Directory, b""),
    ("fill/", Kind::Directory, b""),
];

type Op = fn(&Path);

/// Runs every scenario on a fresh fixture. Files are seeded with an old mtime,
/// so an edit shows even on a file system with coarse timestamps.
pub fn change_source(make: &dyn Fn() -> ChangeFixture) {
    let scenarios: [(&str, Op, &[&str]); 12] = [
        ("create", |r| write(r, "new.txt", b"new"), &["new.txt"]),
        (
            "edit, same size",
            |r| write(r, "edit.txt", b"after!"),
            &["edit.txt"],
        ),
        ("touch", |r| touch(&r.join("touch.txt")), &["touch.txt"]),
        (
            "delete",
            |r| fs::remove_file(r.join("gone.txt")).unwrap(),
            &["gone.txt"],
        ),
        ("rename", rename, &["old/name.txt", "new/name.txt"]),
        ("executable bit", chmod_x, &["mode.sh"]),
        ("link retargeted", retarget, &["link"]),
        ("file replaced by a link", file_to_link, &["keep.txt"]),
        (
            "new empty directory",
            |r| fs::create_dir(r.join("made")).unwrap(),
            &["made/"],
        ),
        (
            "empty directory removed",
            |r| fs::remove_dir(r.join("empty")).unwrap(),
            &["empty/"],
        ),
        (
            "empty directory filled",
            |r| write(r, "fill/x", b"x"),
            &["fill/", "fill/x"],
        ),
        ("no change", |_| {}, &[]),
    ];
    for (name, op, expect) in scenarios {
        run(make, name, op, expect, false);
    }
    run(make, "racy same-size edit", racy_edit, &["edit.txt"], true);
}

fn run(make: &dyn Fn() -> ChangeFixture, name: &str, op: Op, expect: &[&str], racy: bool) {
    eprintln!("change_source: {name}");
    let mut fx = make();
    for (path, kind, content) in SEED {
        fx.ws.write(path, kind, &mut &content[..], MTIME).unwrap();
    }
    // A racy index was checked within 2 seconds of the files' mtime.
    let at = if racy { MTIME + 1 } else { now() };
    let index: CheckedIndex = fx
        .ws
        .list()
        .unwrap()
        .into_iter()
        .map(|(p, meta)| (p, Checked { meta, at }))
        .collect();
    // A source may report the seeding itself; that call is not checked.
    fx.source.changed(&*fx.ws, &index).unwrap();
    op(&fx.root);
    let got = fx.source.changed(&*fx.ws, &index).unwrap();
    for p in expect {
        assert!(
            got.iter().any(|g| g == p),
            "{name}: {p:?} not reported; got {got:?}"
        );
    }
}

fn now() -> i64 {
    let d = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
    d.map_or(0, |d| d.as_nanos() as i64)
}

fn write(r: &Path, p: &str, content: &[u8]) {
    let p = r.join(p);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// The content changes but size, mtime and inode stay: a coarse clock (§5.4).
fn racy_edit(r: &Path) {
    let path = r.join("edit.txt");
    fs::write(&path, b"after!").unwrap();
    let f = fs::File::options().write(true).open(&path).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_nanos(MTIME as u64))
        .unwrap();
}

fn touch(path: &Path) {
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(1))
        .unwrap();
}

fn rename(r: &Path) {
    fs::create_dir_all(r.join("new")).unwrap();
    fs::rename(r.join("old/name.txt"), r.join("new/name.txt")).unwrap();
    fs::remove_dir(r.join("old")).unwrap();
}

fn chmod_x(r: &Path) {
    fs::set_permissions(r.join("mode.sh"), fs::Permissions::from_mode(0o755)).unwrap();
}

/// A target of the same length.
fn retarget(r: &Path) {
    fs::remove_file(r.join("link")).unwrap();
    symlink("edit.txt", r.join("link")).unwrap();
}

fn file_to_link(r: &Path) {
    fs::remove_file(r.join("keep.txt")).unwrap();
    symlink("unchanged", r.join("keep.txt")).unwrap();
}
