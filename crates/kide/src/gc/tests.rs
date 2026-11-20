use super::*;
use crate::conformance::Plain;
use crate::error::Error;
use crate::format::content::write_content;
use crate::format::record::Record;
use crate::iface::{Content, Link, Want};
use crate::load;
use crate::store::MemStore;

/// A profile whose content's chunks are concatenated record ids.
struct Ids;

impl Profile for Ids {
    fn root(&self) -> Want {
        Want::Objects
    }

    fn children(&self, bytes: &[u8]) -> Result<Vec<Link>> {
        let ids = bytes.as_chunks::<32>().0.iter();
        Ok(ids
            .map(|c| Link {
                id: Hash(*c),
                want: Want::Record(Content::Bytes),
                label: None,
            })
            .collect())
    }
}

fn content(s: &MemStore, data: &[u8]) -> Hash {
    write_content(data, &mut |id, b| s.put_known(id, b))
        .unwrap()
        .id
}

fn record(s: &MemStore, mut parents: Vec<Hash>, data: &[u8]) -> Hash {
    parents.sort();
    let root = content(s, data);
    let r = Record {
        change: Vec::new(),
        parents,
        root,
        state: vec![0],
    };
    s.put(&r.encode().unwrap()).unwrap()
}

/// Profile content naming `records`.
fn root(s: &MemStore, records: &[Hash]) -> Hash {
    content(s, &records.iter().flat_map(|r| r.0).collect::<Vec<_>>())
}

#[test]
fn keeps_what_the_roots_need_and_removes_the_rest() {
    let s = MemStore::new();
    let old = record(&s, vec![], b"old content");
    let new = record(&s, vec![old], b"new content");
    let (p_old, p_new) = (root(&s, &[old]), root(&s, &[new]));
    content(&s, b"unrelated");
    // Each piece of content is a node and one chunk.
    assert_eq!(s.ids().unwrap().len(), 12);
    let keep = mark(&s, &Ids, &[p_new], false).unwrap();
    assert!(keep.contains(&old), "history is kept");
    let old_root = load::record(&s, &old).unwrap().root;
    assert!(!keep.contains(&old_root), "but not the content of history");

    let both = collect(&s, &Ids, &[p_new, p_old], false).unwrap();
    assert_eq!(
        (both.kept, both.removed),
        (10, 2),
        "only the unrelated content"
    );
    let one = collect(&s, &Ids, &[p_new], false).unwrap();
    assert_eq!((one.kept, one.removed), (6, 4), "old content and old root");
    assert!(s.has(&old).unwrap() && !s.has(&old_root).unwrap());
    assert_eq!(
        collect(&s, &Ids, &[p_new], false).unwrap().removed,
        0,
        "idempotent"
    );
}

#[test]
fn the_leaves_of_a_merge_keep_their_content() {
    let s = MemStore::new();
    let x0 = record(&s, vec![], b"x before");
    let x = record(&s, vec![x0], b"x");
    let y = record(&s, vec![], b"y");
    let gone = load::record(&s, &y).unwrap().root;
    let dead = s
        .put(&Record::tombstone(vec![y]).encode().unwrap())
        .unwrap();
    let m = record(&s, vec![x, dead], b"x");
    let p = root(&s, &[m]);
    let keep = mark(&s, &Ids, &[p], false).unwrap();
    for (what, id) in [("merge", m), ("leaf", x), ("tombstone leaf", dead)] {
        assert!(keep.contains(&id), "{what}");
    }
    let leaf_root = load::record(&s, &x).unwrap().root;
    assert!(keep.contains(&leaf_root), "a live leaf's content");
    assert!(keep.contains(&x0) && keep.contains(&y), "history");
    let x0_root = load::record(&s, &x0).unwrap().root;
    assert!(!keep.contains(&x0_root) && !keep.contains(&gone));
}

#[test]
fn a_missing_object_removes_nothing() {
    let s = MemStore::new();
    let a = record(&s, vec![], b"a");
    let b = record(&s, vec![a], b"b");
    let p = root(&s, &[b]);
    content(&s, b"garbage");
    s.remove(&a).unwrap();
    let n = s.ids().unwrap().len();
    let r = collect(&s, &Ids, &[p], false);
    assert!(matches!(r, Err(Error::NotFound(x)) if x == a), "{r:?}");
    assert_eq!(s.ids().unwrap().len(), n);
}

#[test]
fn a_profile_of_plain_objects_keeps_what_it_links() {
    let s = MemStore::new();
    let rec = record(&s, vec![], b"named");
    let kept = s
        .put(&Plain::object(&[(Want::Record(Content::Bytes), rec)]))
        .unwrap();
    let root = s.put(&Plain::object(&[(Want::Object, kept)])).unwrap();
    let dropped = s.put(&Plain::object(&[(Want::Object, root)])).unwrap();
    let n = s.ids().unwrap().len();
    let c = collect(&s, &Plain, &[root], false).unwrap();
    assert_eq!((c.kept, c.removed), (n - 1, 1));
    assert!(!s.has(&dropped).unwrap() && s.has(&kept).unwrap());
}

#[test]
fn partial_trees_skip_what_is_absent() {
    let s = MemStore::new();
    let below = s.put(&Plain::object(&[])).unwrap();
    let absent = s.put(&Plain::object(&[(Want::Object, below)])).unwrap();
    let root = s.put(&Plain::object(&[(Want::Object, absent)])).unwrap();
    s.remove(&absent).unwrap();
    let r = collect(&s, &Plain, &[root], false);
    assert!(matches!(r, Err(Error::NotFound(x)) if x == absent), "{r:?}");
    let c = collect(&s, &Plain, &[root], true).unwrap();
    assert_eq!(
        (c.kept, c.removed),
        (1, 1),
        "unreachable through the absent"
    );
    assert!(s.has(&root).unwrap() && !s.has(&below).unwrap());
}

#[test]
fn a_kept_checkpoint_keeps_its_objects_and_history_drops_them() {
    let s = MemStore::new();
    let (old, old_object) = Plain::checkpoint(&s, vec![], b"old").unwrap();
    let (new, new_object) = Plain::checkpoint(&s, vec![old], b"new").unwrap();
    let want = Want::Record(Content::Objects);
    let root = s.put(&Plain::object(&[(want, new)])).unwrap();
    collect(&s, &Plain, &[root], false).unwrap();
    assert!(s.has(&new_object).unwrap() && s.has(&old).unwrap());
    assert!(!s.has(&old_object).unwrap(), "history only");

    let (old, old_object) = Plain::checkpoint(&s, vec![], b"old").unwrap();
    let both = s.put(&Plain::object(&[(want, new), (want, old)])).unwrap();
    collect(&s, &Plain, &[both], false).unwrap();
    assert!(s.has(&new_object).unwrap() && s.has(&old_object).unwrap());
}
