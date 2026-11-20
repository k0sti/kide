//! Conformance suite for `ObjectStore`.

use super::Rng;
use crate::error::Error;
use crate::format::manifest::MAX_MANIFEST_CHUNK;
use crate::hash::{Hash, h};
use crate::iface::ObjectStore;
use std::collections::BTreeSet;

type Check = fn(&dyn ObjectStore);

/// Runs every check, each on a new, empty store from `make`.
pub fn object_store(make: &dyn Fn() -> Box<dyn ObjectStore>) {
    let checks: [(&str, Check); 12] = [
        ("put then get", put_then_get),
        ("put is idempotent", put_idempotent),
        ("get of an unknown id", get_unknown),
        ("put_known with a wrong id", put_known_wrong),
        ("put_known with the right id", put_known_right),
        ("has agrees with get", has_agrees),
        ("remove", remove),
        ("put after remove", put_after_remove),
        ("ids lists exactly what is stored", ids_exact),
        ("sync and compact change no content", sync_and_compact),
        ("clear removes everything; the store stays usable", clear),
        ("300 objects round-trip", many),
    ];
    for (name, check) in checks {
        eprintln!("object_store: {name}");
        check(&*make());
    }
}

/// The read half of the suite, for a read-only view: `store` holds exactly
/// `present`.
pub fn object_store_read(store: &dyn ObjectStore, present: &[Vec<u8>]) {
    for bytes in present {
        let id = h(bytes);
        assert!(store.has(&id).unwrap(), "has({id}) of a present object");
        assert_eq!(
            &store.get(&id).unwrap(),
            bytes,
            "get({id}) returned other bytes"
        );
    }
    let absent = h(b"kide-conformance-absent");
    assert!(!store.has(&absent).unwrap(), "has() of an absent object");
    expect_not_found(store, &absent);
    let ids: BTreeSet<Hash> = store.ids().unwrap().into_iter().collect();
    let want: BTreeSet<Hash> = present.iter().map(|b| h(b)).collect();
    assert_eq!(ids, want, "ids() of a read-only store");
}

fn expect_not_found(s: &dyn ObjectStore, id: &Hash) {
    match s.get(id) {
        Err(Error::NotFound(x)) => assert_eq!(&x, id, "NotFound carries another id"),
        other => panic!("get of absent {id}: expected NotFound, got {other:?}"),
    }
}

fn sorted_ids(s: &dyn ObjectStore) -> Vec<Hash> {
    let mut ids = s.ids().unwrap();
    ids.sort();
    ids
}

fn put_then_get(s: &dyn ObjectStore) {
    for bytes in [&b""[..], b"x", b"hello\n", &[0u8; 12_000]] {
        let id = s.put(bytes).unwrap();
        assert_eq!(id, h(bytes), "put must return H(bytes)");
        assert_eq!(s.get(&id).unwrap(), bytes, "get after put");
    }
}

fn put_idempotent(s: &dyn ObjectStore) {
    let a = s.put(b"same").unwrap();
    let b = s.put(b"same").unwrap();
    assert_eq!(a, b, "put twice gave different ids");
    assert_eq!(s.ids().unwrap(), vec![a], "put twice must store one object");
    assert_eq!(s.get(&a).unwrap(), b"same");
}

fn get_unknown(s: &dyn ObjectStore) {
    s.put(b"something else").unwrap();
    expect_not_found(s, &h(b"never stored"));
}

fn put_known_wrong(s: &dyn ObjectStore) {
    let id = h(b"claimed");
    match s.put_known(&id, b"actual") {
        Err(Error::Corrupt(x)) => assert_eq!(x, id, "Corrupt carries another id"),
        other => panic!("put_known with a wrong id: expected Corrupt, got {other:?}"),
    }
    assert!(!s.has(&id).unwrap(), "a rejected put_known stored the id");
    assert!(
        !s.has(&h(b"actual")).unwrap(),
        "a rejected put_known stored the bytes"
    );
    assert!(
        s.ids().unwrap().is_empty(),
        "a rejected put_known stored something"
    );
}

fn put_known_right(s: &dyn ObjectStore) {
    let id = h(b"known");
    s.put_known(&id, b"known").unwrap();
    s.put_known(&id, b"known").unwrap();
    assert_eq!(s.get(&id).unwrap(), b"known", "get after put_known");
    assert_eq!(s.ids().unwrap(), vec![id]);
}

fn has_agrees(s: &dyn ObjectStore) {
    let id = s.put(b"present").unwrap();
    assert!(s.has(&id).unwrap(), "has() is false after put");
    let absent = h(b"absent");
    assert!(!s.has(&absent).unwrap(), "has() is true for an absent id");
    expect_not_found(s, &absent);
}

fn remove(s: &dyn ObjectStore) {
    let a = s.put(b"to remove").unwrap();
    let b = s.put(b"to keep").unwrap();
    s.remove(&a).unwrap();
    expect_not_found(s, &a);
    assert!(!s.has(&a).unwrap(), "has() is true after remove");
    assert_eq!(
        s.get(&b).unwrap(),
        b"to keep",
        "remove touched another object"
    );
    s.remove(&a)
        .unwrap_or_else(|e| panic!("removing an absent object: {e}"));
    assert_eq!(s.ids().unwrap(), vec![b]);
}

fn put_after_remove(s: &dyn ObjectStore) {
    let a = s.put(b"back again").unwrap();
    s.remove(&a).unwrap();
    s.put_known(&a, b"back again").unwrap();
    assert_eq!(
        s.get(&a).unwrap(),
        b"back again",
        "an object put after its removal"
    );
    assert_eq!(s.ids().unwrap(), vec![a]);
}

fn ids_exact(s: &dyn ObjectStore) {
    assert!(s.ids().unwrap().is_empty(), "a new store is not empty");
    let mut want: Vec<Hash> = (0..20u8).map(|i| s.put(&[i; 3]).unwrap()).collect();
    want.sort();
    assert_eq!(sorted_ids(s), want, "ids() differs from what was put");
}

fn clear(s: &dyn ObjectStore) {
    let ids: Vec<Hash> = [&b"one"[..], b"two", b"three"]
        .iter()
        .map(|b| s.put(b).unwrap())
        .collect();
    s.sync().unwrap();
    s.clear().unwrap();
    assert!(s.ids().unwrap().is_empty(), "ids after clear");
    ids.iter().for_each(|id| expect_not_found(s, id));
    let again = s.put(b"two").unwrap();
    assert_eq!(s.get(&again).unwrap(), b"two", "put after clear");
}

fn sync_and_compact(s: &dyn ObjectStore) {
    let mut rng = Rng::new(2);
    // Distinct lengths, so distinct objects.
    let objects: Vec<Vec<u8>> = (0..200).map(|i| rng.bytes(i * 37 % 3000)).collect();
    let ids: Vec<Hash> = objects.iter().map(|b| s.put(b).unwrap()).collect();
    s.sync().unwrap();
    for id in ids.iter().step_by(3) {
        s.remove(id).unwrap();
    }
    s.compact().unwrap();
    s.sync().unwrap();
    s.compact().unwrap();
    for (i, (id, bytes)) in ids.iter().zip(&objects).enumerate() {
        if i % 3 == 0 {
            expect_not_found(s, id);
        } else {
            assert_eq!(&s.get(id).unwrap(), bytes, "object {i} after compact");
        }
    }
    assert_eq!(s.ids().unwrap().len(), 133, "ids() after compact");
}

fn many(s: &dyn ObjectStore) {
    let mut rng = Rng::new(0x6b69_6465);
    // Up to the largest object of the format, a manifest chunk.
    let objects: Vec<Vec<u8>> = (0..300)
        .map(|i| {
            let len = match i {
                0 => 0,
                1 => MAX_MANIFEST_CHUNK,
                _ => rng.below(MAX_MANIFEST_CHUNK as u64 + 1) as usize,
            };
            rng.bytes(len)
        })
        .collect();
    let ids: Vec<Hash> = objects.iter().map(|o| s.put(o).unwrap()).collect();
    for (id, o) in ids.iter().zip(&objects) {
        assert_eq!(&s.get(id).unwrap(), o, "object {id} did not round-trip");
    }
    let unique: BTreeSet<Hash> = ids.into_iter().collect();
    assert_eq!(s.ids().unwrap().len(), unique.len(), "ids() count");
}

#[cfg(test)]
mod tests {
    //! The suite is checked against two throwaway stores: a correct one must
    //! pass and a broken one must fail.
    use super::*;
    use crate::error::Result;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        unchecked: bool,
        map: Mutex<HashMap<Hash, Vec<u8>>>,
    }

    impl ObjectStore for Fake {
        fn has(&self, id: &Hash) -> Result<bool> {
            Ok(self.map.lock().unwrap().contains_key(id))
        }
        fn get(&self, id: &Hash) -> Result<Vec<u8>> {
            let map = self.map.lock().unwrap();
            map.get(id).cloned().ok_or(Error::NotFound(*id))
        }
        fn put(&self, bytes: &[u8]) -> Result<Hash> {
            let id = h(bytes);
            self.map.lock().unwrap().insert(id, bytes.to_vec());
            Ok(id)
        }
        fn put_known(&self, id: &Hash, bytes: &[u8]) -> Result<()> {
            if !self.unchecked && h(bytes) != *id {
                return Err(Error::Corrupt(*id));
            }
            self.map.lock().unwrap().insert(*id, bytes.to_vec());
            Ok(())
        }
        fn remove(&self, id: &Hash) -> Result<()> {
            self.map.lock().unwrap().remove(id);
            Ok(())
        }
        fn ids(&self) -> Result<Vec<Hash>> {
            Ok(self.map.lock().unwrap().keys().copied().collect())
        }
    }

    #[test]
    fn correct_store_passes() {
        object_store(&|| Box::new(Fake::default()));
    }

    #[test]
    #[should_panic(expected = "expected Corrupt")]
    fn store_that_skips_the_check_fails() {
        object_store(&|| {
            Box::new(Fake {
                unchecked: true,
                ..Fake::default()
            })
        });
    }
}
