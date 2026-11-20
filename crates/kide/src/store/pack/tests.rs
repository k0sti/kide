use super::*;
use crate::conformance::{Rng, TempDir, object_store, object_store_read};

fn objects(seed: u64, n: usize) -> Vec<Vec<u8>> {
    let mut rng = Rng::new(seed);
    (0..n)
        .map(|_| {
            let len = rng.below(3000) as usize;
            rng.bytes(len)
        })
        .collect()
}

fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name, fs::read(e.path()).unwrap())
        })
        .collect()
}

fn sorted(s: &dyn ObjectStore) -> Vec<Hash> {
    let mut ids = s.ids().unwrap();
    ids.sort();
    ids
}

fn open(dir: &Path, max: u64) -> PackStore {
    PackStore::open_with(dir.into(), max)
        .unwrap()
        .without_fsync()
}

#[test]
fn conformance() {
    let tmp = TempDir::new("pack");
    object_store(&|| Box::new(PackStore::open_with(tmp.subdir(), 16 << 10).unwrap()));
}

#[test]
fn read_only_conformance() {
    let tmp = TempDir::new("pack");
    let objs = objects(9, 40);
    let s = open(tmp.path(), 16 << 10);
    for o in &objs {
        s.put(o).unwrap();
    }
    s.sync().unwrap();
    object_store_read(&PackStore::open_read_only(tmp.path()).unwrap(), &objs);
    let none = PackStore::open_read_only(tmp.path().join("absent")).unwrap();
    object_store_read(&none, &[]);
}

#[test]
fn reopening_keeps_everything_across_segments() {
    let tmp = TempDir::new("pack");
    let objs = objects(1, 300);
    let s = open(tmp.path(), 16 << 10);
    for o in &objs {
        s.put(o).unwrap();
    }
    s.sync().unwrap();
    assert!(s.state().segs.len() > 10, "many segments");
    drop(s);
    let s = open(tmp.path(), 16 << 10);
    for o in &objs {
        assert_eq!(&s.get(&h(o)).unwrap(), o);
    }
}

#[test]
fn unsynced_records_survive_a_reopen() {
    let tmp = TempDir::new("pack");
    let objs = objects(2, 50);
    let s = open(tmp.path(), SEGMENT_MAX);
    for o in &objs[..20] {
        s.put(o).unwrap();
    }
    s.sync().unwrap();
    for o in &objs[20..] {
        s.put(o).unwrap();
    }
    drop(s); // no sync: the index lacks the last 30 entries
    let s = open(tmp.path(), SEGMENT_MAX);
    assert_eq!(s.ids().unwrap().len(), 50);
}

#[test]
fn a_cut_segment_loses_only_the_cut_object() {
    for synced in [false, true] {
        let tmp = TempDir::new("pack");
        let s = open(tmp.path(), SEGMENT_MAX);
        let ids: Vec<Hash> = (0..3u8).map(|i| s.put(&[i; 1000]).unwrap()).collect();
        if synced {
            s.sync().unwrap();
        }
        drop(s);
        let seg = tmp.path().join(seg_name(1));
        let len = fs::metadata(&seg).unwrap().len();
        let f = File::options().write(true).open(&seg).unwrap();
        f.set_len(len - 500).unwrap();
        let s = open(tmp.path(), SEGMENT_MAX);
        assert!(s.has(&ids[0]).unwrap() && s.has(&ids[1]).unwrap());
        assert!(!s.has(&ids[2]).unwrap(), "synced {synced}");
        assert_eq!(fs::metadata(&seg).unwrap().len(), 2 * 1036);
        // The repaired store takes new objects where the cut one was.
        s.put(&[2; 1000]).unwrap();
        assert_eq!(s.get(&ids[2]).unwrap(), [2; 1000]);
    }
}

#[test]
fn a_bad_record_ends_the_surviving_prefix() {
    let tmp = TempDir::new("pack");
    let s = open(tmp.path(), 3000);
    let ids: Vec<Hash> = (0..6u8).map(|i| s.put(&[i; 1000]).unwrap()).collect();
    drop(s); // segments 1, 2, 3 with two records each, none synced
    let seg2 = tmp.path().join(seg_name(2));
    let mut b = fs::read(&seg2).unwrap();
    b[100] ^= 1; // inside the first record of segment 2
    fs::write(&seg2, b).unwrap();
    let s = open(tmp.path(), 3000);
    let present: Vec<bool> = ids.iter().map(|id| s.has(id).unwrap()).collect();
    assert_eq!(present, [true, true, false, false, false, false]);
    assert!(!tmp.path().join(seg_name(3)).exists(), "newer segments go");
}

#[test]
fn a_damaged_synced_record_reads_as_corrupt() {
    let tmp = TempDir::new("pack");
    let s = open(tmp.path(), SEGMENT_MAX);
    let id = s.put(&[5; 100]).unwrap();
    s.sync().unwrap();
    let seg = tmp.path().join(seg_name(1));
    let mut b = fs::read(&seg).unwrap();
    b[HEADER + 10] ^= 1;
    fs::write(&seg, b).unwrap();
    let ro = PackStore::open_read_only(tmp.path()).unwrap();
    assert!(matches!(ro.get(&id), Err(Error::Corrupt(x)) if x == id));
    assert!(ro.has(&id).unwrap(), "a read-only store only reports it");
    assert!(matches!(s.get(&id), Err(Error::Corrupt(x)) if x == id));
    assert!(!s.has(&id).unwrap(), "dropped: it counts as absent");
    assert!(tmp.path().join(DAMAGED).exists(), "and the store is marked");
    s.put(&[5; 100]).unwrap();
    assert_eq!(s.get(&id).unwrap(), [5; 100], "stored again");
    s.sync().unwrap();
    let again = open(tmp.path(), SEGMENT_MAX);
    assert_eq!(again.get(&id).unwrap(), [5; 100], "after a reopen too");
}

#[test]
fn a_torn_index_entry_is_ignored() {
    let tmp = TempDir::new("pack");
    let s = open(tmp.path(), SEGMENT_MAX);
    let mut ids: Vec<Hash> = (0..4u8).map(|i| s.put(&[i; 10]).unwrap()).collect();
    s.sync().unwrap();
    drop(s);
    let index = tmp.path().join("index");
    let b = fs::read(&index).unwrap();
    // An append of an unsynced entry that was cut short.
    fs::write(&index, [&b[..], &[9; 20]].concat()).unwrap();
    let ro = PackStore::open_read_only(tmp.path()).unwrap();
    assert_eq!(
        ro.ids().unwrap().len(),
        4,
        "read-only ignores the torn entry"
    );
    let s = open(tmp.path(), SEGMENT_MAX);
    ids.sort();
    assert_eq!(sorted(&s), ids);
    assert_eq!(
        fs::read(&index).unwrap(),
        b,
        "repair removes the torn entry"
    );
}

#[test]
fn read_only_changes_nothing() {
    let tmp = TempDir::new("pack");
    let s = open(tmp.path(), SEGMENT_MAX);
    let synced = s.put(b"synced").unwrap();
    s.sync().unwrap();
    let unsynced = s.put(b"not yet synced").unwrap();
    let seg = tmp.path().join(seg_name(1));
    let mut b = fs::read(&seg).unwrap();
    b.extend([7; 10]); // an incomplete tail
    fs::write(&seg, b).unwrap();
    let before = files(tmp.path());
    let ro = PackStore::open_read_only(tmp.path()).unwrap();
    assert_eq!(ro.get(&synced).unwrap(), b"synced");
    assert!(!ro.has(&unsynced).unwrap());
    assert!(matches!(ro.put(b"x"), Err(Error::Invalid(_))));
    assert!(matches!(ro.remove(&synced), Err(Error::Invalid(_))));
    assert!(matches!(ro.clear(), Err(Error::Invalid(_))));
    ro.sync().unwrap();
    ro.compact().unwrap();
    assert_eq!(files(tmp.path()), before);
}

/// Puts, removes and re-puts across small segments, with compaction.
fn churn(dir: &Path) -> PackStore {
    let s = open(dir, 8 << 10);
    let objs = objects(3, 120);
    let mut rng = Rng::new(4);
    for round in 0..4 {
        for o in &objs {
            if rng.below(3) == 0 {
                s.remove(&h(o)).unwrap();
            } else {
                s.put(o).unwrap();
            }
        }
        if round % 2 == 1 {
            s.compact().unwrap();
        }
    }
    s.sync().unwrap();
    s
}

#[test]
fn a_rebuilt_index_equals_the_written_one() {
    let tmp = TempDir::new("pack");
    let s = churn(tmp.path());
    let want = sorted(&s);
    drop(s);
    let index = fs::read(tmp.path().join("index")).unwrap();
    fs::remove_file(tmp.path().join("index")).unwrap();
    fs::remove_file(tmp.path().join("synced")).unwrap();
    let s = open(tmp.path(), 8 << 10);
    assert_eq!(fs::read(tmp.path().join("index")).unwrap(), index);
    assert_eq!(sorted(&s), want);
}

#[test]
fn compaction_keeps_live_objects_and_frees_dead_space() {
    let tmp = TempDir::new("pack");
    let s = open(tmp.path(), 8 << 10);
    let objs = objects(5, 200);
    let ids: Vec<Hash> = objs.iter().map(|o| s.put(o).unwrap()).collect();
    for id in ids.iter().filter(|id| id.0[0] % 4 != 0) {
        s.remove(id).unwrap();
    }
    let size = |s: &PackStore| s.state().segs.values().map(|g| g.size).sum::<u64>();
    let before = size(&s);
    s.compact().unwrap();
    assert!(size(&s) * 2 < before, "{} of {before} bytes left", size(&s));
    drop(s);
    let s = open(tmp.path(), 8 << 10);
    for (id, o) in ids.iter().zip(&objs) {
        match id.0[0] % 4 {
            0 => assert_eq!(&s.get(id).unwrap(), o),
            _ => assert!(!s.has(id).unwrap()),
        }
    }
}

#[test]
fn a_removal_outlives_compaction_while_an_older_segment_holds_the_object() {
    let tmp = TempDir::new("pack");
    let s = open(tmp.path(), 4096);
    let x = s.put(&[1; 100]).unwrap();
    s.put(&[2; 3000]).unwrap(); // keeps segment 1 mostly live
    let b = s.put(&[3; 3000]).unwrap(); // segment 2
    s.remove(&x).unwrap(); // removal record in segment 2
    s.remove(&b).unwrap();
    s.put(&[4; 3000]).unwrap(); // segment 3, the newest
    s.compact().unwrap();
    assert!(tmp.path().join(seg_name(1)).exists());
    assert!(!tmp.path().join(seg_name(2)).exists());
    drop(s);
    for name in ["index", "synced"] {
        fs::remove_file(tmp.path().join(name)).unwrap();
    }
    let s = open(tmp.path(), 4096);
    assert!(!s.has(&x).unwrap(), "the removal was kept");
    assert!(!s.has(&b).unwrap());
}

#[test]
fn clear_empties_the_store() {
    let tmp = TempDir::new("pack");
    let s = open(tmp.path(), SEGMENT_MAX);
    s.put(b"staged").unwrap();
    s.sync().unwrap();
    s.clear().unwrap();
    assert!(s.ids().unwrap().is_empty());
    assert!(files(tmp.path()).is_empty());
    let id = s.put(b"again").unwrap();
    assert_eq!(s.get(&id).unwrap(), b"again");
}
