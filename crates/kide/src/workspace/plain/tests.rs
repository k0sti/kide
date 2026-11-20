use super::*;
use crate::conformance::{TempDir, WorkspaceFixture, workspace};

#[test]
fn conformance() {
    let tmp = TempDir::new("plain");
    workspace(&|| {
        let root = tmp.subdir();
        WorkspaceFixture {
            ws: Box::new(PlainFolder::new(&root)),
            root,
        }
    });
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

/// Whether the file system keeps NFD and NFC names apart (APFS does not).
fn tells_nfd_from_nfc(dir: &Path) -> bool {
    let probe = dir.join("probe-e\u{301}");
    fs::write(&probe, b"").unwrap();
    let apart = !dir.join("probe-\u{e9}").exists();
    fs::remove_file(probe).unwrap();
    apart
}

#[test]
fn names_become_nfc_on_disk() {
    let tmp = TempDir::new("plain-nfc");
    if !tells_nfd_from_nfc(tmp.path()) {
        return;
    }
    let ws = PlainFolder::new(tmp.path());
    fs::create_dir(tmp.path().join("Cafe\u{301}")).unwrap();
    fs::write(tmp.path().join("Cafe\u{301}/n\u{303}o.txt"), b"x").unwrap();
    let listed: Vec<String> = ws.list().unwrap().into_iter().map(|(p, _)| p).collect();
    assert_eq!(listed, ["Café/ño.txt"]);
    assert_eq!(names(tmp.path()), ["Café"], "renamed on disk");
    assert_eq!(names(&tmp.path().join("Café")), ["ño.txt"]);
    assert!(ws.stat("Café/ño.txt").unwrap().is_some());
}

#[test]
fn a_taken_nfc_name_is_skipped() {
    let tmp = TempDir::new("plain-nfc");
    if !tells_nfd_from_nfc(tmp.path()) {
        return;
    }
    let ws = PlainFolder::new(tmp.path());
    fs::write(tmp.path().join("e\u{301}"), b"decomposed").unwrap();
    fs::write(tmp.path().join("é"), b"composed").unwrap();
    let listed: Vec<String> = ws.list().unwrap().into_iter().map(|(p, _)| p).collect();
    assert_eq!(listed, ["é"]);
    assert_eq!(fs::read(tmp.path().join("é")).unwrap(), b"composed");
    assert_eq!(names(tmp.path()).len(), 2, "nothing renamed or removed");
}

#[test]
fn long_names_get_a_short_temporary() {
    let tmp = TempDir::new("plain-long");
    let ws = PlainFolder::new(tmp.path());
    let name = "ä".repeat(127); // 254 bytes
    ws.write(&name, Kind::Regular, &mut &b"x"[..], 0).unwrap();
    assert_eq!(names(tmp.path()), [name]);
}

#[test]
fn rename_never_overwrites() {
    let tmp = TempDir::new("plain-rename");
    let ws = PlainFolder::new(tmp.path());
    ws.write("a", Kind::Regular, &mut &b"a"[..], 0).unwrap();
    ws.write("b", Kind::Regular, &mut &b"b"[..], 0).unwrap();
    assert!(matches!(ws.rename("a", "b"), Err(Error::Io(_))));
    assert_eq!(fs::read(tmp.path().join("b")).unwrap(), b"b");
}

#[test]
fn a_read_only_folder_is_never_written() {
    let tmp = TempDir::new("plain-ro");
    fs::write(tmp.path().join("e\u{301}.md"), b"decomposed").unwrap();
    fs::write(tmp.path().join("a"), b"a").unwrap();
    let ws = PlainFolder::read_only(tmp.path());
    let listed: Vec<String> = ws.list().unwrap().into_iter().map(|(p, _)| p).collect();
    assert_eq!(listed, ["a"], "the non-NFC name is skipped, not renamed");
    assert_eq!(names(tmp.path()), ["a", "e\u{301}.md"]);
    let refused = |r: Result<()>| assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    refused(ws.write("b", Kind::Regular, &mut &b"b"[..], 0).map(drop));
    refused(ws.set_mtime("a", 0).map(drop));
    refused(ws.rename("a", "c"));
    refused(ws.trash("a"));
    assert_eq!(names(tmp.path()), ["a", "e\u{301}.md"]);
    assert_eq!(fs::read(tmp.path().join("a")).unwrap(), b"a");
    assert!(ws.stat("a").unwrap().is_some());
}

#[test]
fn trash_goes_to_the_given_directory() {
    let tmp = TempDir::new("plain-trash");
    let (root, trash) = (tmp.path().join("files"), tmp.path().join("state/trash"));
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&trash).unwrap();
    let ws = PlainFolder::with_trash(&root, &trash);
    ws.write("d/a", Kind::Regular, &mut &b"a"[..], 0).unwrap();
    ws.trash("d/a").unwrap();
    assert!(names(&root).is_empty());
    let slot = fs::read_dir(&trash)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(fs::read(slot.join("d/a")).unwrap(), b"a");
}

#[test]
fn write_if_replaces_only_what_is_expected_and_keeps_it_in_the_trash() {
    let tmp = TempDir::new("plain-if");
    let root = tmp.path().join("f");
    let trash = tmp.path().join("trash");
    fs::create_dir(&root).unwrap();
    let ws = PlainFolder::with_trash(&root, &trash);
    let old = ws
        .write("a.txt", Kind::Regular, &mut &b"old"[..], 1_000_000_000)
        .unwrap();
    let stale = Meta { size: 99, ..old };
    let r = ws.write_if(
        "a.txt",
        Kind::Regular,
        &mut &b"new"[..],
        2_000_000_000,
        Some(stale),
    );
    assert_eq!(r.unwrap(), None, "changed since: nothing written");
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"old");
    assert_eq!(names(&root), ["a.txt"], "no temporary left");
    let r = ws.write_if(
        "a.txt",
        Kind::Regular,
        &mut &b"new"[..],
        2_000_000_000,
        Some(old),
    );
    assert!(r.unwrap().is_some());
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"new");
    let slot = fs::read_dir(&trash)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        fs::read(slot.join("a.txt")).unwrap(),
        b"old",
        "the replaced file is kept"
    );
    let r = ws.write_if("b.txt", Kind::Regular, &mut &b"b"[..], 0, Some(old));
    assert_eq!(r.unwrap(), None, "expected a file where there is none");
    assert!(
        ws.write_if("b.txt", Kind::Regular, &mut &b"b"[..], 0, None)
            .unwrap()
            .is_some()
    );
}
