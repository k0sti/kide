//! Conformance suite for `Workspace` (§5.2, §5.3, §5.5, §5.6).

use super::Rng;
use crate::error::{Error, Result};
use crate::format::manifest::Kind;
use crate::iface::{Meta, Workspace};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A new, empty workspace and the directory that holds it, where the suite
/// plants what the workspace must ignore and inspects the results.
pub struct WorkspaceFixture {
    pub ws: Box<dyn Workspace>,
    pub root: PathBuf,
}

/// An mtime with nanoseconds, in 2025.
pub(super) const MTIME: i64 = 1_761_134_400_123_456_789;

type Check = fn(&dyn Workspace, &Path);

/// Runs every check, each on a fresh workspace from `make`.
pub fn workspace(make: &dyn Fn() -> WorkspaceFixture) {
    let checks: [(&str, Check); 15] = [
        ("write sets kind, size and mtime", write_sets_metadata),
        ("write is atomic", write_is_atomic),
        ("symbolic links", links),
        ("empty directories", empty_dirs),
        ("a file replaces an empty directory", write_over_empty_dir),
        ("list skips what is not synchronised", list_skips),
        ("invalid paths are refused", invalid_paths),
        ("links are never followed", links_not_followed),
        ("absent paths", absent_paths),
        ("set_mtime", set_mtime),
        ("rename", rename),
        ("rename tree", rename_tree),
        ("trash", trash),
        ("trash twice in one second", trash_collisions),
        ("round trip", round_trip),
    ];
    for (name, check) in checks {
        eprintln!("workspace: {name}");
        let fx = make();
        check(&*fx.ws, &fx.root);
        assert_no_tmp(&fx.root);
    }
}

fn put(ws: &dyn Workspace, path: &str, kind: Kind, content: &[u8]) -> Meta {
    ws.write(path, kind, &mut &content[..], MTIME)
        .unwrap_or_else(|e| panic!("write({path:?}): {e}"))
}

fn read_all(ws: &dyn Workspace, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    ws.open(path).unwrap().read_to_end(&mut out).unwrap();
    out
}

fn listed(ws: &dyn Workspace) -> BTreeMap<String, Meta> {
    let all = ws.list().unwrap();
    let n = all.len();
    let map: BTreeMap<String, Meta> = all.into_iter().collect();
    assert_eq!(map.len(), n, "list returned a path twice");
    map
}

fn names(ws: &dyn Workspace) -> Vec<String> {
    listed(ws).into_keys().collect()
}

fn assert_no_tmp(dir: &Path) {
    for e in fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        assert!(!name.ends_with(".kide-tmp"), "{:?} remains", e.path());
        if e.file_type().unwrap().is_dir() {
            assert_no_tmp(&e.path());
        }
    }
}

fn write_sets_metadata(ws: &dyn Workspace, _: &Path) {
    let m = put(ws, "a/b/script.sh", Kind::Executable, b"#!/bin/sh\n");
    assert_eq!((m.kind, m.size, m.mtime), (Kind::Executable, 10, MTIME));
    assert_eq!(
        ws.stat("a/b/script.sh").unwrap(),
        Some(m),
        "stat after write"
    );
    let m = ws
        .write(
            "a/b/script.sh",
            Kind::Regular,
            &mut &b"echo\n"[..],
            MTIME + 1,
        )
        .unwrap();
    assert_eq!((m.kind, m.size, m.mtime), (Kind::Regular, 5, MTIME + 1));
    assert_eq!(read_all(ws, "a/b/script.sh"), b"echo\n");
    let m = put(ws, "empty.txt", Kind::Regular, b"");
    assert_eq!((m.kind, m.size), (Kind::Regular, 0));
}

fn write_is_atomic(ws: &dyn Workspace, root: &Path) {
    const LONG: usize = 256 << 10;
    const SHORT: usize = 100 << 10;
    let content = |i: usize| match i % 2 {
        0 => vec![b'a'; LONG],
        _ => vec![b'b'; SHORT],
    };
    put(ws, "atomic.bin", Kind::Regular, &content(0));
    let done = Arc::new(AtomicBool::new(false));
    let path = root.join("atomic.bin");
    let reader = {
        let done = done.clone();
        std::thread::spawn(move || {
            let mut reads = 0;
            while !done.load(Ordering::Relaxed) || reads == 0 {
                let got = fs::read(&path).expect("the file vanished during a write");
                let ok = (got.len() == LONG && got.iter().all(|&b| b == b'a'))
                    || (got.len() == SHORT && got.iter().all(|&b| b == b'b'));
                assert!(ok, "a reader saw partial content ({} bytes)", got.len());
                reads += 1;
            }
        })
    };
    for i in 1..20 {
        put(ws, "atomic.bin", Kind::Regular, &content(i));
    }
    done.store(true, Ordering::Relaxed);
    reader.join().expect("reader check failed");
}

fn links(ws: &dyn Workspace, root: &Path) {
    let target = b"../somewhere/else";
    let m = put(ws, "d/link", Kind::Symlink, target);
    assert_eq!((m.kind, m.size), (Kind::Symlink, target.len() as u64));
    let on_disk = fs::read_link(root.join("d/link")).expect("not a symbolic link");
    assert_eq!(on_disk.as_os_str().as_bytes(), target, "link target");
    assert_eq!(read_all(ws, "d/link"), target, "open reads the target");
    assert_eq!(
        ws.stat("d/link").unwrap(),
        Some(m),
        "stat of a dangling link"
    );
    put(ws, "keep", Kind::Regular, b"k");
    put(ws, "file", Kind::Regular, b"x");
    let m = put(ws, "file", Kind::Symlink, b"keep");
    assert_eq!(m.kind, Kind::Symlink, "a link replaces a file");
    assert_eq!(read_all(ws, "file"), b"keep");
    let m = put(ws, "file", Kind::Regular, b"again");
    assert_eq!(m.kind, Kind::Regular, "a file replaces a link");
    assert_eq!(
        fs::read(root.join("keep")).unwrap(),
        b"k",
        "a write followed a link"
    );
}

fn empty_dirs(ws: &dyn Workspace, root: &Path) {
    let m = put(ws, "e/f/", Kind::Directory, b"");
    assert_eq!((m.kind, m.size), (Kind::Directory, 0));
    assert!(root.join("e/f").is_dir());
    assert_eq!(ws.stat("e/f/").unwrap(), Some(m));
    assert_eq!(read_all(ws, "e/f/"), b"", "a directory has no content");
    assert_eq!(names(ws), ["e/f/"], "only the innermost empty directory");
    assert_eq!(
        ws.stat("e/").unwrap(),
        None,
        "a directory that holds something"
    );
    put(ws, "e/f/x", Kind::Regular, b"x");
    assert_eq!(
        names(ws),
        ["e/f/x"],
        "a directory with content has no entry"
    );
    assert_eq!(ws.stat("e/f/").unwrap(), None);
    let bad = [
        ("g", Kind::Directory, &b""[..]),
        ("g/", Kind::Regular, b""),
        ("g/", Kind::Symlink, b"t"),
        ("g/", Kind::Directory, b"content"),
    ];
    for (path, kind, content) in bad {
        let r = ws.write(path, kind, &mut &content[..], MTIME);
        expect_invalid(&format!("write({path:?}, {kind:?}, {content:?})"), r);
    }
    assert!(
        !root.join("g").exists(),
        "a refused write created something"
    );
}

fn write_over_empty_dir(ws: &dyn Workspace, root: &Path) {
    fs::create_dir_all(root.join("x/y")).unwrap();
    put(ws, "x/y", Kind::Regular, b"now a file");
    assert_eq!(read_all(ws, "x/y"), b"now a file");
    fs::create_dir_all(root.join("full")).unwrap();
    fs::write(root.join("full/inside"), b"keep").unwrap();
    let r = ws.write("full", Kind::Regular, &mut &b"no"[..], MTIME);
    assert!(r.is_err(), "a directory that holds something stays");
    assert_eq!(fs::read(root.join("full/inside")).unwrap(), b"keep");
}

fn list_skips(ws: &dyn Workspace, root: &Path) {
    put(ws, "keep.txt", Kind::Regular, b"1");
    put(ws, "d/e/deep.sh", Kind::Executable, b"22");
    put(ws, "link", Kind::Symlink, b"keep.txt");
    // What the workspace must skip, planted behind its back.
    let plant = |p: &str| {
        let p = root.join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"skip").unwrap();
    };
    plant(".kide/objects/x");
    plant(".KIDE/x");
    plant("half.kide-tmp");
    plant("d/.deep.sh.KIDE-TMP");
    fs::create_dir_all(root.join(".kide/empty")).unwrap();
    symlink("d", root.join("dirlink")).unwrap();
    let bad = std::ffi::OsStr::from_bytes(b"bad\xffname");
    // Some file systems (APFS) refuse such a name; then there is none to skip.
    if fs::write(root.join(bad), b"invalid UTF-8").is_ok() {
        fs::create_dir_all(root.join(bad).with_extension("dir")).unwrap();
    }
    let fifo = std::process::Command::new("mkfifo")
        .arg(root.join("fifo"))
        .status();
    assert!(fifo.is_ok_and(|s| s.success()), "mkfifo failed");
    let got = listed(ws);
    let kinds: Vec<(&str, Kind)> = got.iter().map(|(p, m)| (p.as_str(), m.kind)).collect();
    let want = [
        ("d/e/deep.sh", Kind::Executable),
        ("dirlink", Kind::Symlink),
        ("keep.txt", Kind::Regular),
        ("link", Kind::Symlink),
    ];
    assert_eq!(kinds, want, "list returned other paths");
    assert_eq!(got["dirlink"].size, 1);
    for (p, m) in &got {
        assert_eq!(
            ws.stat(p).unwrap().as_ref(),
            Some(m),
            "list and stat disagree"
        );
    }
    assert_eq!(ws.stat("fifo").unwrap(), None, "a special file");
    for p in ["half.kide-tmp", "d/.deep.sh.KIDE-TMP", "fifo"] {
        let _ = fs::remove_file(root.join(p));
    }
}

fn expect_invalid<T: std::fmt::Debug>(what: &str, r: Result<T>) {
    assert!(
        matches!(r, Err(Error::Invalid(_))),
        "{what}: expected Invalid, got {r:?}"
    );
}

fn invalid_paths(ws: &dyn Workspace, _: &Path) {
    put(ws, "ok.txt", Kind::Regular, b"x");
    let long = "n".repeat(256);
    let bad = [
        "",
        "/abs",
        "../up",
        "a/../b",
        "a/./b",
        ".",
        "..",
        "a//b",
        ".kide",
        ".kide/x",
        ".Kide/x",
        "x.kide-tmp",
        "a/x.Kide-Tmp/b",
        "nul\0byte",
        &long,
    ];
    for p in bad {
        let r = ws.write(p, Kind::Regular, &mut &b"x"[..], MTIME);
        expect_invalid(&format!("write({p:?})"), r);
        expect_invalid(&format!("stat({p:?})"), ws.stat(p));
        expect_invalid(&format!("open({p:?})"), ws.open(p).map(|_| ()));
        expect_invalid(&format!("set_mtime({p:?})"), ws.set_mtime(p, MTIME));
        expect_invalid(&format!("rename(ok.txt, {p:?})"), ws.rename("ok.txt", p));
        expect_invalid(&format!("trash({p:?})"), ws.trash(p));
    }
    assert_eq!(
        read_all(ws, "ok.txt"),
        b"x",
        "a refused call changed a file"
    );
    put(ws, &"n".repeat(255), Kind::Regular, b"x");
    put(ws, "sub/.kide", Kind::Regular, b"y"); // reserved only at the top
}

fn links_not_followed(ws: &dyn Workspace, root: &Path) {
    let outside = root.join(".kide/outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret"), b"s").unwrap();
    symlink(&outside, root.join("l")).unwrap();
    put(ws, "f", Kind::Regular, b"f");
    assert!(
        ws.write("l/x", Kind::Regular, &mut &b"x"[..], MTIME)
            .is_err()
    );
    assert!(
        ws.write("l/y/", Kind::Directory, &mut &b""[..], MTIME)
            .is_err()
    );
    assert!(
        ws.rename("f", "l/f").is_err(),
        "rename into a linked directory"
    );
    assert!(ws.trash("l/secret").is_err(), "trash through a link");
    assert!(
        ws.set_mtime("l/secret", MTIME).is_err(),
        "set_mtime through a link"
    );
    assert!(ws.open("l/secret").is_err(), "open through a link");
    let r = ws.stat("l/secret");
    assert!(!matches!(r, Ok(Some(_))), "stat through a link: {r:?}");
    let inside: Vec<String> = fs::read_dir(&outside)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(inside, ["secret"], "something was written through a link");
    assert_eq!(names(ws), ["f", "l"]);
}

fn absent_paths(ws: &dyn Workspace, _: &Path) {
    assert_eq!(ws.stat("missing.txt").unwrap(), None);
    assert_eq!(ws.stat("missing/").unwrap(), None);
    assert!(ws.open("missing.txt").is_err(), "open of an absent path");
    assert!(
        ws.set_mtime("missing.txt", MTIME).is_err(),
        "set_mtime of an absent path"
    );
    put(ws, "d/f", Kind::Regular, b"x");
    assert_eq!(ws.stat("d").unwrap(), None, "a directory without the slash");
    assert_eq!(ws.stat("d/f/below").unwrap(), None, "a path below a file");
    assert_eq!(ws.stat("d/f/").unwrap(), None, "a file with a slash");
}

fn set_mtime(ws: &dyn Workspace, _: &Path) {
    put(ws, "t.txt", Kind::Executable, b"touch");
    let m = ws.set_mtime("t.txt", MTIME + 7_000).unwrap();
    assert_eq!((m.kind, m.mtime), (Kind::Executable, MTIME + 7_000));
    assert_eq!(ws.stat("t.txt").unwrap(), Some(m));
    assert_eq!(read_all(ws, "t.txt"), b"touch");
    let m = ws.set_mtime("t.txt", -1).unwrap();
    assert_eq!(m.mtime, -1, "an mtime before the epoch");
}

fn rename(ws: &dyn Workspace, root: &Path) {
    put(ws, "from/sub/file.sh", Kind::Executable, b"moved");
    put(ws, "from/link", Kind::Symlink, b"sub");
    ws.rename("from/sub/file.sh", "to/other/file.sh").unwrap();
    ws.rename("from/link", "to/link").unwrap();
    assert_eq!(
        ws.stat("from/sub/file.sh").unwrap(),
        None,
        "the source remains"
    );
    let m = ws
        .stat("to/other/file.sh")
        .unwrap()
        .expect("the target is missing");
    assert_eq!(
        (m.kind, m.mtime),
        (Kind::Executable, MTIME),
        "rename lost metadata"
    );
    assert_eq!(read_all(ws, "to/other/file.sh"), b"moved");
    assert_eq!(read_all(ws, "to/link"), b"sub", "a renamed link");
    assert!(
        !root.join("from").exists(),
        "rename left emptied directories"
    );
}

fn rename_tree(ws: &dyn Workspace, root: &Path) {
    put(ws, "old/nested/a", Kind::Regular, b"one");
    put(ws, "old/link", Kind::Symlink, b"nested/a");
    let inode = ws.stat("old/nested/a").unwrap().unwrap().inode;
    ws.rename("old/", "new/").unwrap();
    assert!(!root.join("old").exists());
    assert_eq!(read_all(ws, "new/nested/a"), b"one");
    assert_eq!(ws.stat("new/nested/a").unwrap().unwrap().inode, inode);
    put(ws, "occupied/", Kind::Directory, b"");
    assert!(ws.rename("new/", "occupied/").is_err());
    assert!(ws.rename("new/", "new/inside/").is_err());
}

fn trash(ws: &dyn Workspace, root: &Path) {
    put(ws, "x/y/doomed.txt", Kind::Regular, b"bye");
    put(ws, "x/kept.txt", Kind::Regular, b"stay");
    put(ws, "x/link", Kind::Symlink, b"kept.txt");
    put(ws, "x/e/", Kind::Directory, b"");
    ws.trash("x/y/doomed.txt").unwrap();
    assert_eq!(ws.stat("x/y/doomed.txt").unwrap(), None, "the file remains");
    assert!(
        !root.join("x/y").exists(),
        "trash left an emptied directory"
    );
    assert!(root.join("x/kept.txt").exists(), "trash removed too much");
    let trash = root.join(".kide/trash");
    let mut found = fs::read_dir(&trash)
        .expect("no .kide/trash")
        .filter_map(|e| fs::read(e.ok()?.path().join("x/y/doomed.txt")).ok());
    assert!(
        found.any(|c| c == b"bye"),
        "the file is not at .kide/trash/<time>/x/y/doomed.txt"
    );
    ws.trash("x/link").unwrap();
    assert!(
        fs::symlink_metadata(root.join("x/link")).is_err(),
        "the link remains"
    );
    assert!(root.join("x/kept.txt").exists(), "trash followed the link");
    ws.trash("x/e/").unwrap();
    assert!(!root.join("x/e").exists(), "the empty directory remains");
    ws.trash("x/kept.txt").unwrap();
    assert!(!root.join("x").exists(), "trash left an emptied directory");
    assert!(root.exists(), "trash removed the folder");
    assert!(ws.list().unwrap().is_empty(), "list shows trashed paths");
}

fn trash_collisions(ws: &dyn Workspace, root: &Path) {
    // A file, then a directory of the same name, then the same file again.
    put(ws, "a", Kind::Regular, b"first");
    ws.trash("a").unwrap();
    put(ws, "a/b", Kind::Regular, b"second");
    ws.trash("a/b")
        .expect("a trashed file does not block a trashed directory");
    put(ws, "a", Kind::Regular, b"third");
    ws.trash("a").expect("a trashed name can be trashed again");
    fn walk(d: &Path, out: &mut Vec<Vec<u8>>) {
        for e in fs::read_dir(d).unwrap().flatten() {
            match e.file_type().unwrap().is_dir() {
                true => walk(&e.path(), out),
                false => out.push(fs::read(e.path()).unwrap()),
            }
        }
    }
    let mut found = Vec::new();
    walk(&root.join(".kide/trash"), &mut found);
    found.sort();
    assert_eq!(
        found,
        [&b"first"[..], b"second", b"third"],
        "every trashed file is kept"
    );
}

fn round_trip(ws: &dyn Workspace, _: &Path) {
    let mut rng = Rng::new(42);
    let mut want = BTreeMap::new();
    for i in 0..40 {
        let depth = rng.below(4);
        let mut path: Vec<String> = (0..depth)
            .map(|d| format!("dir{}", rng.below(3) + d * 10))
            .collect();
        let len = [0, 1, 100, 20_000][rng.below(4) as usize];
        let (kind, content) = match rng.below(6) {
            0 => (Kind::Executable, rng.bytes(len)),
            1 => (Kind::Symlink, format!("target{i}").into_bytes()),
            2 => (Kind::Directory, Vec::new()),
            _ => (Kind::Regular, rng.bytes(len)),
        };
        path.push(format!("entry{i}"));
        let mut path = path.join("/");
        if kind == Kind::Directory {
            path.push('/');
        }
        put(ws, &path, kind, &content);
        want.insert(path, (kind, content));
    }
    let got = listed(ws);
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>(),
        "listed paths"
    );
    for (path, (kind, content)) in &want {
        let m = &got[path];
        assert_eq!((m.kind, m.size), (*kind, content.len() as u64), "{path}");
        if *kind != Kind::Symlink && *kind != Kind::Directory {
            assert_eq!(m.mtime, MTIME, "{path}: mtime");
        }
        assert_eq!(&read_all(ws, path), content, "{path}: content");
    }
}
