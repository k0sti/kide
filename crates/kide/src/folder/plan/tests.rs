use super::*;
use crate::folder::index::Entry;
use crate::format::content::write_content;
use crate::format::manifest::build_manifest;
use crate::report::Why;
use crate::resolve::DecisionTable;
use crate::store::MemStore;

/// A store with manifests made of entries.
struct World(MemStore);

impl World {
    fn record(&self, data: &[u8], parents: &[Hash]) -> (Hash, u64) {
        let root = write_content(data, &mut |id, b| self.0.put_known(id, b)).unwrap();
        let r = Record {
            change: Vec::new(),
            parents: parents.to_vec(),
            root: root.id,
            state: Kind::Regular.state(),
        };
        (self.0.put(&r.encode().unwrap()).unwrap(), data.len() as u64)
    }

    fn manifest(&self, entries: &[(&str, (Hash, u64))]) -> (Hash, Index) {
        let entries: Vec<PathEntry> = entries
            .iter()
            .map(|(p, (v, size))| PathEntry {
                path: p.to_string(),
                version: *v,
                size: *size,
                mtime: if p.ends_with('/') { 0 } else { 5 },
            })
            .collect();
        let root = build_manifest(&entries, &mut |id, b| self.0.put_known(id, b)).unwrap();
        let mut index = Index::default();
        for e in entries {
            let entry = Entry {
                kind: Some(Kind::Regular),
                version: e.version,
                size: e.size,
                mtime: e.mtime,
                disk: None,
            };
            index.0.insert(e.path, entry);
        }
        (root.id, index)
    }

    fn plan(&self, local: &(Hash, Index), remote: &Hash) -> Plan {
        let p = &Platform::LINUX;
        plan(&self.0, &DecisionTable, &local.1, &local.0, remote, p).unwrap()
    }
}

#[test]
fn a_long_copy_name_is_cut_and_a_long_path_gets_no_copy() {
    let w = World(MemStore::new());
    let base = w.record(b"base", &[]);
    let (x, y) = (w.record(b"x", &[base.0]), w.record(b"y", &[base.0]));
    let (name, deep) = (
        format!("{}.bin", "n".repeat(246)),
        format!("{}f.bin", "d/".repeat(2038)),
    );
    let local = w.manifest(&[(&deep, x), (&name, x)]);
    let (remote, _) = w.manifest(&[(&deep, y), (&name, y)]);
    let p = w.plan(&local, &remote);
    let copies: Vec<&String> = p
        .target
        .keys()
        .filter(|k| k.contains("kide-conflict"))
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copies[0].len(), 255);
    assert!(copies[0].starts_with(&"n".repeat(224)), "{}", copies[0]);
    let merge = load::record(&w.0, &p.target[&deep].version).unwrap();
    assert_eq!(merge.parents.len(), 2, "the merge, without its copy");
    let reported = crate::report::reported_paths(Why::NotCopied);
    assert!(
        reported.iter().any(|r| r.starts_with("d/d/")),
        "{reported:?}"
    );
}

#[test]
fn a_clash_leaves_a_tombstone_and_decides_the_copy_at_its_path() {
    let w = World(MemStore::new());
    let f = w.record(b"file", &[]);
    let copy = conflict_path("x", &f.0);
    let local = w.manifest(&[("x", f)]);
    let dir = w.record(b"dir", &[]);
    let (remote, _) = w.manifest(&[("x/inside", dir)]);
    let p = w.plan(&local, &remote);
    let t = load::record(&w.0, &p.target["x"].version).unwrap();
    assert_eq!(t, Record::tombstone(vec![f.0]));
    let c = load::record(&w.0, &p.target[&copy].version).unwrap();
    assert_eq!(
        (c.parents, c.root),
        (vec![f.0], load::record(&w.0, &f.0).unwrap().root)
    );
    // The copy path already holds another file: the copy is decided against
    // it, here by a new merge that copies its loser in turn.
    let other = w.record(b"other", &[]);
    let (remote, _) = w.manifest(&[(&copy, other), ("x/inside", dir)]);
    let p = w.plan(&local, &remote);
    assert!(p.target["x"].kind.is_none() && p.moves.is_empty());
    let m = load::record(&w.0, &p.target[&copy].version).unwrap();
    assert_eq!(m.parents.len(), 2, "a merge at the copy path: {m:?}");
    let further = p
        .target
        .keys()
        .filter(|k| k.starts_with(&copy[..14]) && **k != copy);
    assert_eq!(
        further.count(),
        1,
        "the loser's own copy: {:?}",
        p.target.keys()
    );
}

#[test]
fn a_copy_already_there_or_edited_is_not_made_again() {
    let w = World(MemStore::new());
    let base = w.record(b"base", &[]);
    let (x, y) = (w.record(b"x", &[base.0]), w.record(b"y", &[base.0]));
    let (remote, _) = w.manifest(&[("f.bin", y)]);
    let first = w.plan(&w.manifest(&[("f.bin", x)]), &remote);
    let (copy, c) = first.target.iter().find(|(p, _)| *p != "f.bin").unwrap();
    // The copy was edited since; the same conflict, met again, keeps that.
    let edited = w.record(b"edited copy", &[c.version]);
    let local = w.manifest(&[("f.bin", x), (copy.as_str(), edited)]);
    let again = w.plan(&local, &remote);
    assert_eq!(again.target[copy].version, edited.0);
    assert_eq!(again.target.len(), 2);
}

#[test]
fn a_filled_empty_directory_becomes_a_tombstone() {
    let w = World(MemStore::new());
    let empty = write_content(b"", &mut |id, b| w.0.put_known(id, b)).unwrap();
    let dir = Record {
        change: Vec::new(),
        parents: vec![],
        root: empty.id,
        state: Kind::Directory.state(),
    };
    let d = (w.0.put(&dir.encode().unwrap()).unwrap(), 0);
    let mut local = w.manifest(&[("d/", d)]);
    local.1.0.get_mut("d/").unwrap().kind = Some(Kind::Directory);
    let (remote, _) = w.manifest(&[("d/x", w.record(b"x", &[]))]);
    let p = w.plan(&local, &remote);
    assert_eq!(p.target["d/"].kind, None, "{p:?}");
    let t = load::record(&w.0, &p.target["d/"].version).unwrap();
    assert_eq!(t.parents, [d.0]);
}
