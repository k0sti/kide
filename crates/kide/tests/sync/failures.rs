//! A pull that fails changes nothing (§4.4): a missing or unreachable
//! object is never a deletion, and a corrupt one applies nothing.

use crate::device::{Device, Files, pair};
use kide::Error;
use kide::hash::Hash;
use kide::iface::{Live, LiveEvents, ObjectStore, Session, Transport};
use kide::load;
use kide::transport::Served;
use std::sync::Arc;

/// b deletes `gone` and edits `kept`; a has pulled both before.
pub fn deleted_on_b() -> (crate::device::Net, Device, Device) {
    let (net, mut a, mut b) = pair();
    a.write("gone", b"deleted on b\n");
    a.write("kept", b"base\n");
    net.sync(&mut a, &mut b);
    b.remove("gone");
    b.write("kept", b"edited on b\n");
    b.scan();
    (net, a, b)
}

pub fn snapshot(d: &Device) -> (Files, Hash) {
    (d.files(), d.head())
}

#[test]
fn a_missing_object_deletes_nothing() {
    let (net, mut a, b) = deleted_on_b();
    let v = b.folder.index.0["kept"].version;
    let root = load::record(&*b.folder.store, &v).unwrap().root;
    b.folder.store.remove(&root).unwrap();
    let before = snapshot(&a);
    let r = a.try_pull(&net.0, "b");
    assert!(matches!(r, Err(Error::NotFound(x)) if x == root), "{r:?}");
    assert_eq!(snapshot(&a), before);
}

#[test]
fn an_unreachable_device_deletes_nothing() {
    let (net, mut a, _b) = deleted_on_b();
    let before = snapshot(&a);
    let r = a.try_pull(&net.0, "nobody");
    assert!(matches!(r, Err(Error::Unavailable(_))), "{r:?}");
    assert_eq!(snapshot(&a), before);

    /// Sessions that break after Hello.
    struct Breaks<'a>(&'a crate::device::Net);
    struct Broken(Box<dyn Session>);
    impl Transport for Breaks<'_> {
        fn connect(&self, peer: &str) -> kide::Result<Box<dyn Session>> {
            Ok(Box::new(Broken(self.0.0.connect(peer)?)))
        }
        fn live(&self, peer: &str, events: LiveEvents) -> kide::Result<Arc<dyn Live>> {
            self.0.0.live(peer, events)
        }
    }
    impl Session for Broken {
        fn hello(&mut self, root: &Hash) -> kide::Result<Hash> {
            self.0.hello(root)
        }
        fn get(&mut self, ids: &[Hash]) -> kide::Result<Vec<kide::Result<Vec<u8>>>> {
            Ok(ids
                .iter()
                .map(|_| Err(Error::Unavailable("reset".into())))
                .collect())
        }
        fn get_children(&mut self, ids: &[Hash]) -> kide::Result<Vec<kide::Result<Vec<u8>>>> {
            self.get(ids)
        }
        fn fetch(&mut self, _: &Hash, _: &[Hash], _: &[u8]) -> kide::Result<Vec<(Hash, Vec<u8>)>> {
            Ok(Vec::new()) // broken before anything arrived
        }
        fn bye(&mut self) -> kide::Result<()> {
            Ok(())
        }
    }
    let r = a.try_pull(&Breaks(&net), "b");
    assert!(matches!(r, Err(Error::Unavailable(_))), "{r:?}");
    assert_eq!(snapshot(&a), before);
}

/// A store whose bytes for one id are damaged.
pub struct Damaged(pub Arc<dyn ObjectStore>, pub Hash);

impl ObjectStore for Damaged {
    fn has(&self, id: &Hash) -> kide::Result<bool> {
        self.0.has(id)
    }
    fn get(&self, id: &Hash) -> kide::Result<Vec<u8>> {
        let mut b = self.0.get(id)?;
        if *id == self.1 {
            b[0] ^= 1;
        }
        Ok(b)
    }
    fn put(&self, bytes: &[u8]) -> kide::Result<Hash> {
        self.0.put(bytes)
    }
    fn put_known(&self, id: &Hash, bytes: &[u8]) -> kide::Result<()> {
        self.0.put_known(id, bytes)
    }
    fn remove(&self, id: &Hash) -> kide::Result<()> {
        self.0.remove(id)
    }
    fn ids(&self) -> kide::Result<Vec<Hash>> {
        self.0.ids()
    }
}

#[test]
fn a_corrupt_object_applies_nothing() {
    let (net, mut a, b) = deleted_on_b();
    let v = b.folder.index.0["kept"].version;
    let served = Served {
        store: Arc::new(Damaged(b.folder.store.clone(), v)),
        ..b.served()
    };
    net.0.register("b", served);
    let before = snapshot(&a);
    let r = a.try_pull(&net.0, "b");
    assert!(matches!(r, Err(Error::Corrupt(x)) if x == v), "{r:?}");
    assert_eq!(snapshot(&a), before);
    assert_eq!(a.read("gone").unwrap(), b"deleted on b\n");
}
