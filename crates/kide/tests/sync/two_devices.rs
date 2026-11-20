//! Two devices: every kind of change in both directions, conflicts, clashes
//! and modification times (§5.6).

use crate::device::{Device, Net, OLD, assert_same, pair, tick};
use kide::format::manifest::Kind;
use kide::hash::Hash;
use kide::iface::{Live, LiveEvents, Session, Transport};
use kide::load;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

/// The version record of `path` in the device's index.
fn version(d: &Device, path: &str) -> Hash {
    d.folder.index.0[path].version
}

/// Every kind of change made on `src`, pulled by `dst`, in `src`'s own
/// directory.
fn lifecycle(net: &Net, src: &mut Device, dst: &mut Device) {
    let tag = src.name.clone();
    let p = |name: &str| format!("{tag}/{name}");
    src.write(&p("new.txt"), b"created\n");
    src.put(&p("run.sh"), Kind::Executable, b"#!/bin/sh\n");
    src.put(&p("link"), Kind::Symlink, b"new.txt");
    src.put(&p("empty/"), Kind::Directory, b"");
    net.pull(dst, src);
    assert_same(&[src, dst]);
    let m = dst.ws.stat(&p("run.sh")).unwrap().unwrap();
    assert_eq!(m.kind, Kind::Executable, "the exec bit");
    let target = fs::read_link(dst.root().join(p("link"))).unwrap();
    assert_eq!(target.to_str(), Some("new.txt"), "a symbolic link");
    assert!(dst.root().join(p("empty")).is_dir());

    // Edit, delete, rename, mode change, the empty directory filled.
    src.write(&p("new.txt"), b"edited\n");
    src.remove(&p("link"));
    let renamed = src.root().join(p("renamed.sh"));
    fs::rename(src.root().join(p("run.sh")), &renamed).unwrap();
    fs::set_permissions(&renamed, fs::Permissions::from_mode(0o644)).unwrap();
    src.write(&p("empty/inside"), b"no longer empty\n");
    net.pull(dst, src);
    assert_same(&[src, dst]);
    assert_eq!(dst.read(&p("new.txt")).unwrap(), b"edited\n");
    assert!(dst.read(&p("run.sh")).is_none(), "renamed away");
    let m = dst.ws.stat(&p("renamed.sh")).unwrap().unwrap();
    assert_eq!(m.kind, Kind::Regular, "the exec bit cleared");
    assert!(fs::symlink_metadata(dst.root().join(p("link"))).is_err());
    let dir = dst.folder.index.0[&p("empty/")];
    assert!(
        !dir.is_live(),
        "the filled directory's entry is a tombstone"
    );

    // The directory emptied again, then removed.
    src.remove(&p("empty/inside"));
    net.pull(dst, src);
    assert_same(&[src, dst]);
    assert!(dst.folder.index.0[&p("empty/")].is_live());
    src.remove(&p("empty/"));
    net.pull(dst, src);
    assert_same(&[src, dst]);
    assert!(!dst.root().join(p("empty")).exists());
}

#[test]
fn every_change_travels_both_ways() {
    let (net, mut a, mut b) = pair();
    lifecycle(&net, &mut a, &mut b);
    lifecycle(&net, &mut b, &mut a);
    // Pulls in both directions change nothing more.
    let head = a.head();
    net.sync(&mut a, &mut b);
    assert_eq!((a.head(), b.head()), (head, head));
    assert!(a.copies().is_empty());
}

#[test]
fn replaced_files_go_to_the_trash() {
    let (net, mut a, mut b) = pair();
    a.write("n.md", b"first\n");
    net.sync(&mut a, &mut b);
    a.write("n.md", b"second\n");
    net.pull(&mut b, &mut a);
    assert_eq!(b.read("n.md").unwrap(), b"second\n");
    let trash = b.root().join(".kide/trash");
    let slots = fs::read_dir(&trash).unwrap().flatten();
    let kept: Vec<Vec<u8>> = slots
        .filter_map(|s| fs::read(s.path().join("n.md")).ok())
        .collect();
    assert_eq!(kept, [b"first\n"]);
}

#[test]
fn deleted_files_go_to_the_trash() {
    let (net, mut a, mut b) = pair();
    a.write("d/e/gone.txt", b"bye\n");
    net.sync(&mut a, &mut b);
    a.write("d/e/stays", b"kept\n");
    net.sync(&mut a, &mut b);
    // Removing the file leaves an empty directory, which has an entry.
    a.remove("d/e/gone.txt");
    net.pull(&mut b, &mut a);
    assert!(b.root().join("d/e/stays").is_file());
    a.remove("d/e/stays");
    net.pull(&mut b, &mut a);
    assert!(b.folder.index.0["d/e/"].is_live(), "an emptied directory");
    // Removing the whole directory leaves no entry: its parents go too.
    fs::remove_dir_all(a.root().join("d")).unwrap();
    net.pull(&mut b, &mut a);
    assert!(!b.root().join("d").exists(), "emptied parents are removed");
    assert_same(&[&a, &b]);
    let trash = b.root().join(".kide/trash");
    let trashed = |p: &str| {
        let slots = fs::read_dir(&trash).unwrap().flatten();
        slots
            .filter_map(|s| fs::read(s.path().join(p)).ok())
            .collect::<Vec<_>>()
    };
    assert_eq!(trashed("d/e/gone.txt"), [b"bye\n"]);
    assert_eq!(trashed("d/e/stays"), [b"kept\n"]);
}

#[test]
fn concurrent_edits_merge_once_with_one_copy() {
    let (net, mut a, mut b) = pair();
    a.write("notes.bin", b"base\n");
    net.sync(&mut a, &mut b);
    let base = version(&a, "notes.bin");
    let ma = a.write("notes.bin", b"edited on a\n");
    let mb = b.write("notes.bin", b"edited on b\n");
    a.scan();
    net.pull(&mut a, &mut b);
    let (va, vb) = (version(&a, "notes.bin"), version(&b, "notes.bin"));
    let merge = load::record(&*a.folder.store, &va).unwrap();
    assert_eq!(merge.parents.len(), 2, "one merge record");
    let rb = load::record(&*a.folder.store, &vb).unwrap();
    assert_eq!(rb.parents, [base]);
    let (winner, loser, mw, ml) = match merge.root == rb.root {
        true => (b"edited on b\n", b"edited on a\n", mb, ma),
        false => (b"edited on a\n", b"edited on b\n", ma, mb),
    };
    assert_eq!(a.read("notes.bin").unwrap(), winner);
    let copies = a.copies();
    assert_eq!(copies.len(), 1, "{copies:?}");
    let lost = merge
        .parents
        .iter()
        .find(|p| load::record(&*a.folder.store, p).unwrap().root != merge.root);
    let want = format!("notes.kide-conflict-{}.bin", &lost.unwrap().to_hex()[..12]);
    assert_eq!(copies[0], want);
    assert_eq!(a.read(&want).unwrap(), loser);
    let files = a.files();
    assert_eq!(
        files["notes.bin"].2, mw,
        "the merge takes the winner's mtime"
    );
    assert_eq!(files[&want].2, ml, "the copy takes its side's mtime");
    net.pull(&mut b, &mut a);
    assert_same(&[&a, &b]);
    net.sync(&mut a, &mut b);
    assert_eq!(a.copies().len(), 1, "no second copy");
}

#[test]
fn concurrent_edits_of_a_note_merge_as_text() {
    let (net, mut a, mut b) = pair();
    a.write("n/plan.md", b"# Plan\none\ntwo\nthree\n");
    net.sync(&mut a, &mut b);
    a.write("n/plan.md", b"# Plan\nONE\ntwo\nthree\n");
    b.write("n/plan.md", b"# Plan\none\ntwo\nTHREE\n");
    a.scan();
    let r = net.pull(&mut a, &mut b);
    assert_eq!(r.merged, ["n/plan.md"], "a text merge is reported");
    net.pull(&mut b, &mut a);
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
    assert_eq!(a.read("n/plan.md").unwrap(), b"# Plan\nONE\ntwo\nTHREE\n");
    assert!(a.copies().is_empty(), "{:?}", a.copies());
    // The merge goes on like any version.
    b.write("n/plan.md", b"# Plan\nONE\n2\nTHREE\n");
    net.sync(&mut a, &mut b);
    assert_eq!(a.read("n/plan.md").unwrap(), b"# Plan\nONE\n2\nTHREE\n");
}

#[test]
fn concurrent_edits_of_code_or_data_keep_a_conflict_copy() {
    for path in ["src/main.rs", "app/data.json", "Makefile"] {
        let (net, mut a, mut b) = pair();
        a.write(path, b"one\ntwo\nthree\n");
        net.sync(&mut a, &mut b);
        a.write(path, b"ONE\ntwo\nthree\n");
        b.write(path, b"one\ntwo\nTHREE\n");
        a.scan();
        net.sync(&mut a, &mut b);
        net.sync(&mut a, &mut b);
        assert_same(&[&a, &b]);
        assert_eq!(a.copies().len(), 1, "{path}: not merged as text");
        let kept = [a.read(path).unwrap(), a.read(&a.copies()[0]).unwrap()];
        assert!(kept.contains(&b"ONE\ntwo\nthree\n".to_vec()), "{path}");
        assert!(kept.contains(&b"one\ntwo\nTHREE\n".to_vec()), "{path}");
    }
}

#[test]
fn a_merge_is_adopted_without_a_second_copy() {
    let (net, mut a, mut b) = pair();
    a.write("f.bin", b"base\n");
    net.sync(&mut a, &mut b);
    a.write("f.bin", b"from a\n");
    b.write("f.bin", b"from b\n");
    a.scan();
    net.pull(&mut a, &mut b);
    let merge = version(&a, "f.bin");
    // b's edit is a leaf of a's merge: b adopts it, and a's copy arrives.
    let r = net.pull(&mut b, &mut a);
    assert_eq!(version(&b, "f.bin"), merge);
    assert!(r.apply.written >= 1, "at least the copy: {r:?}");
    assert_same(&[&a, &b]);
    assert_eq!(a.copies().len(), 1);
}

#[test]
fn content_beats_a_deletion() {
    let (net, mut a, mut b) = pair();
    a.write("f.txt", b"base\n");
    net.sync(&mut a, &mut b);
    a.write("f.txt", b"kept\n");
    b.remove("f.txt");
    b.scan();
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
    assert_eq!(b.read("f.txt").unwrap(), b"kept\n");
    assert!(a.copies().is_empty(), "a deletion leaves no copy");
}

#[test]
fn two_deletions_agree() {
    let (net, mut a, mut b) = pair();
    a.write("f.txt", b"base\n");
    net.sync(&mut a, &mut b);
    a.remove("f.txt");
    b.remove("f.txt");
    a.scan();
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
    assert!(!a.folder.index.0["f.txt"].is_live());
    assert!(a.files().is_empty() && a.copies().is_empty());
}

#[test]
fn a_directory_keeps_a_clashing_name() {
    let (net, mut a, mut b) = pair();
    a.write("x", b"a file\n");
    b.write("x/inside", b"a directory\n");
    a.scan();
    net.pull(&mut a, &mut b);
    let file = version(&b, "x/inside");
    assert_eq!(version(&a, "x/inside"), file);
    let copies = a.copies();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert!(copies[0].starts_with("x.kide-conflict-"));
    assert_eq!(a.read(&copies[0]).unwrap(), b"a file\n");
    assert!(!a.folder.index.0["x"].is_live(), "x leaves a tombstone");
    net.pull(&mut b, &mut a);
    assert_same(&[&a, &b]);
    // The other way round: a file meets a directory that was pulled first.
    let (net, mut c, mut d) = pair();
    c.write("y", b"a file\n");
    d.write("y/inside", b"a directory\n");
    d.scan();
    net.pull(&mut d, &mut c);
    net.pull(&mut c, &mut d);
    assert_same(&[&c, &d]);
    assert_eq!(c.copies().len(), 1);
    assert_eq!(c.read("y/inside").unwrap(), b"a directory\n");
}

#[test]
fn a_file_replaced_by_a_directory_is_no_clash() {
    let (net, mut a, mut b) = pair();
    a.write("x", b"a file\n");
    net.sync(&mut a, &mut b);
    a.remove("x");
    a.write("x/inside", b"now a directory\n");
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
    assert!(b.copies().is_empty());
    assert_eq!(b.read("x/inside").unwrap(), b"now a directory\n");
}

#[test]
fn modification_times_alone_make_no_version() {
    let (net, mut a, mut b) = pair();
    a.write("f", b"same\n");
    net.sync(&mut a, &mut b);
    let v = version(&a, "f");
    let later = OLD + 999_000_000_000;
    let f = fs::File::options()
        .write(true)
        .open(a.root().join("f"))
        .unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(later as u64))
        .unwrap();
    // b touches it to an earlier time: the later mtime wins on both.
    let g = fs::File::options()
        .write(true)
        .open(b.root().join("f"))
        .unwrap();
    g.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(tick() as u64))
        .unwrap();
    b.scan();
    net.sync(&mut a, &mut b);
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
    assert_eq!((version(&a, "f"), version(&b, "f")), (v, v), "no version");
    assert_eq!(b.files()["f"].2, later);
    assert!(a.copies().is_empty() && b.copies().is_empty());
}

/// Runs a change on the pulling device when its pull connects: after its
/// scan, before it applies.
struct OnConnect<'a>(&'a Net, Box<dyn Fn() + Send + Sync + 'a>);

impl Transport for OnConnect<'_> {
    fn connect(&self, peer: &str) -> kide::Result<Box<dyn Session>> {
        (self.1)();
        self.0.0.connect(peer)
    }

    fn live(&self, peer: &str, events: LiveEvents) -> kide::Result<Arc<dyn Live>> {
        self.0.0.live(peer, events)
    }
}

#[test]
fn a_file_edited_after_the_scan_is_not_overwritten() {
    let (net, mut a, mut b) = pair();
    a.write("f.bin", b"base\n");
    a.write("g", b"other\n");
    net.sync(&mut a, &mut b);
    b.write("f.bin", b"remote edit\n");
    b.write("g", b"remote edit of g\n");
    b.scan();
    let f = a.root().join("f.bin");
    let t = OnConnect(
        &net,
        Box::new(move || fs::write(&f, b"edited during the pull\n").unwrap()),
    );
    let r = a.try_pull(&t, "b").unwrap();
    assert_eq!(r.apply.skipped, ["f.bin"]);
    assert_eq!(a.read("f.bin").unwrap(), b"edited during the pull\n");
    assert_eq!(
        a.read("g").unwrap(),
        b"remote edit of g\n",
        "other paths apply"
    );
    // The next pull sees a conflict and keeps both.
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
    let mut all: Vec<Vec<u8>> = a.copies().iter().map(|p| a.read(p).unwrap()).collect();
    all.push(a.read("f.bin").unwrap());
    all.sort();
    assert_eq!(
        all,
        [
            b"edited during the pull\n".to_vec(),
            b"remote edit\n".to_vec()
        ]
    );
}

#[test]
fn nothing_is_written_through_a_symbolic_link() {
    let (net, mut a, mut b) = pair();
    let outside = kide::conformance::TempDir::new("sync-outside");
    b.write("d/x", b"remote\n");
    b.write("e", b"remote\n");
    b.scan();
    // A link appears where the folder has nothing, after the scan.
    let (link, to) = (a.root().join("d"), outside.path().to_path_buf());
    let t = OnConnect(
        &net,
        Box::new(move || std::os::unix::fs::symlink(&to, &link).unwrap()),
    );
    let r = a.try_pull(&t, "b").unwrap();
    assert_eq!(r.apply.skipped, ["d/x"]);
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    assert_eq!(a.read("e").unwrap(), b"remote\n");
    // The link is then a file of the folder, and gives way to the directory.
    net.sync(&mut a, &mut b);
    net.sync(&mut a, &mut b);
    assert_same(&[&a, &b]);
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    assert_eq!(a.read("d/x").unwrap(), b"remote\n");
    assert_eq!(a.copies().len(), 1, "the link, at its conflict-copy path");
}

#[test]
fn equal_heads_need_no_fetch() {
    let (net, mut a, mut b) = pair();
    a.write("f", b"x");
    net.sync(&mut a, &mut b);
    let r = net.pull(&mut a, &mut b);
    assert!(r.fetch.is_none() && r.decided == 0);
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "pushes about 300 objects on macOS CI; to look at on a Mac"
)]
fn a_pull_with_a_new_local_head_is_pushed_only_the_change() {
    let (net, mut a, mut b) = pair();
    for i in 0..100 {
        a.write(&format!("d/f{i}.md"), format!("file {i}\n").as_bytes());
    }
    net.sync(&mut a, &mut b);
    // b's HEAD moves on by its own change, so a has never seen it.
    b.write("b-only.txt", b"local\n");
    b.scan();
    a.write("d/f1.md", b"changed on a\n");
    let r = net.pull(&mut b, &mut a);
    let f = r.fetch.clone().expect("a fetch");
    assert_eq!(r.decided, 2, "{r:?}");
    assert!(f.pushed < 20, "only the change is pushed: {f:?}");
}

#[test]
fn settings_sync_while_per_device_files_stay_local() {
    let (net, mut a, mut b) = pair();
    for d in [&mut a, &mut b] {
        let r = &mut d.folder.config.rules;
        r.add(false, ".obsidian").unwrap();
        r.add(true, ".obsidian/*.json").unwrap();
        r.add(true, ".obsidian/plugins/*/main.js").unwrap();
        r.add(true, ".obsidian/plugins/*/data.json").unwrap();
        r.add(false, ".obsidian/workspace*.json").unwrap();
    }
    b.write(".obsidian/workspace.json", b"{\"b\": 1}");
    a.write(".obsidian/workspace.json", b"{\"a\": 1}");
    a.write(".obsidian/app.json", b"{\"vimMode\": true}");
    a.write(".obsidian/plugins/tasks/main.js", b"code");
    a.write(".obsidian/plugins/tasks/data.json", b"{}");
    a.write(".obsidian/plugins/tasks/cache.db", b"index of a");
    a.write("note.md", b"text\n");
    net.sync(&mut a, &mut b);
    net.sync(&mut a, &mut b);
    for p in [
        ".obsidian/app.json",
        ".obsidian/plugins/tasks/main.js",
        ".obsidian/plugins/tasks/data.json",
        "note.md",
    ] {
        assert_eq!(b.read(p), a.read(p), "{p} is synced");
    }
    assert_eq!(
        b.read(".obsidian/workspace.json").unwrap(),
        b"{\"b\": 1}",
        "b's own"
    );
    assert_eq!(
        a.read(".obsidian/workspace.json").unwrap(),
        b"{\"a\": 1}",
        "a's own"
    );
    assert_eq!(
        b.read(".obsidian/plugins/tasks/cache.db"),
        None,
        "a's cache stays on a"
    );
    // A settings change travels back the other way.
    b.write(".obsidian/app.json", b"{\"vimMode\": false}");
    net.sync(&mut a, &mut b);
    assert_eq!(
        a.read(".obsidian/app.json").unwrap(),
        b"{\"vimMode\": false}"
    );
}

#[test]
fn a_review_refuses_paths_or_the_whole_pull() {
    let (net, mut a, mut b) = pair();
    a.write("open/f", b"base\n");
    a.write("kept/f", b"base\n");
    net.sync(&mut a, &mut b);
    b.write("open/f", b"from b\n");
    b.write("kept/f", b"from b\n");
    b.write("kept/new", b"new\n");
    b.scan();
    // A failing review changes nothing.
    let before = a.head();
    let mut fail = |_: &mut kide::folder::plan::Plan, _: &kide::folder::index::Index| {
        Err(kide::Error::Invalid("over quota".into()))
    };
    assert!(a.try_pull_reviewed(&net.0, "b", &mut fail).is_err());
    assert_eq!(
        (a.head(), a.read("open/f").unwrap()),
        (before, b"base\n".to_vec())
    );
    // Refused paths keep the local entry; the rest applies.
    let mut seen = Vec::new();
    let mut refuse = |p: &mut kide::folder::plan::Plan, index: &kide::folder::index::Index| {
        let changed: Vec<String> = p
            .changes(index)
            .iter()
            .map(|(x, _)| x.to_string())
            .collect();
        for path in changed.iter().filter(|x| x.starts_with("kept/")) {
            p.refuse(path, index);
        }
        seen = changed;
        Ok(())
    };
    a.try_pull_reviewed(&net.0, "b", &mut refuse).unwrap();
    drop(refuse);
    assert_eq!(seen, ["kept/f", "kept/new", "open/f"]);
    assert_eq!(a.read("open/f").unwrap(), b"from b\n");
    assert_eq!(a.read("kept/f").unwrap(), b"base\n");
    assert!(a.read("kept/new").is_none());
}
