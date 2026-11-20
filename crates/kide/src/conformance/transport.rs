//! Conformance suite for `Transport` and `Session` (§4.2).

use super::Rng;
use crate::error::{Error, Result};
use crate::format::node::{chunk_entry, encode_node};
use crate::format::record::Record;
use crate::hash::{Hash, h};
use crate::iface::{MAX_GET, Session, Transport};
use crate::protocol::PROTOCOL as VERSION;
use std::collections::HashSet;

/// A transport, the address of a running peer, and a way to stop that peer.
pub struct TransportFixture {
    pub transport: Box<dyn Transport>,
    pub peer: String,
    /// Makes the peer unreachable: open sessions break, new connections fail.
    pub stop: Box<dyn Fn()>,
}

/// `make(objects, root, protocol)` starts a peer whose store holds exactly
/// `objects`, whose root is `root`, and which sends `protocol` in Hello. It
/// pushes with the profile `Versions` and runs `echo` on its live sessions.
pub type MakePeer = dyn Fn(&[Vec<u8>], Hash, u8) -> TransportFixture;

/// What the peer holds: chunks, level-1 and level-2 nodes, a record, blobs.
struct Sample {
    objects: Vec<Vec<u8>>,
    root: Hash,
    /// The chunks of `whole`, in node order: one appears twice.
    chunks: Vec<Vec<u8>>,
    whole: Hash,
    /// A level-1 node with one chunk the peer lacks.
    gap: Hash,
    /// The node of empty content.
    empty: Hash,
    /// A level-2 node over `whole`.
    upper: Hash,
    /// Another record over the root's content.
    sibling: Hash,
}

impl Sample {
    fn new() -> Sample {
        let mut rng = Rng::new(7);
        let (a, b, c) = (b"hello\n".to_vec(), rng.bytes(3000), rng.bytes(8192));
        let chunks = vec![a.clone(), b.clone(), c.clone(), a.clone()];
        let entries: Vec<_> = chunks.iter().map(|c| chunk_entry(c)).collect();
        let whole = encode_node(1, &entries);
        let gap = encode_node(1, &[chunk_entry(&a), chunk_entry(b"not on the peer")]);
        let empty = encode_node(1, &[]);
        let upper = encode_node(2, &[chunk_entry(&whole)]);
        let record = |state| {
            let r = Record {
                change: Vec::new(),
                parents: vec![],
                root: h(&whole),
                state: vec![state],
            };
            r.encode().expect("a valid record")
        };
        let (record, sibling) = (record(0), record(1));
        let mut objects = vec![a, b, c];
        let more = [&whole, &gap, &empty, &upper, &record, &sibling];
        objects.extend(more.map(|o| o.clone()));
        objects.extend((0..30).map(|i| rng.bytes(i * 401)));
        Sample {
            root: h(&record),
            chunks,
            whole: h(&whole),
            gap: h(&gap),
            empty: h(&empty),
            upper: h(&upper),
            sibling: h(&sibling),
            objects,
        }
    }
}

type Check = fn(&TransportFixture, &Sample);

/// Runs every check against peers from `make`.
pub fn transport(make: &MakePeer) {
    let sample = Sample::new();
    let fx = make(&sample.objects, sample.root, VERSION);
    let checks: [(&str, Check); 7] = [
        ("hello returns the peer's root", hello),
        ("get answers in request order", get_in_order),
        ("get_children answers in request order", get_children),
        ("fetch pushes what want reaches and the haves do not", fetch),
        ("at most 4,096 ids", limits),
        ("sessions are independent", independent),
        ("a stopped peer is unavailable", stopped),
    ];
    for (name, check) in checks {
        eprintln!("transport: {name}");
        check(&fx, &sample);
    }
    eprintln!("transport: another protocol is incompatible");
    let other = make(&sample.objects, sample.root, VERSION + 1);
    let mut s = connect(&other);
    let r = s.hello(&h(b"client"));
    assert!(
        matches!(r, Err(Error::Incompatible(_))),
        "hello with protocol {}: expected Incompatible, got {r:?}",
        VERSION + 1
    );
    (other.stop)();
}

fn connect(fx: &TransportFixture) -> Box<dyn Session> {
    fx.transport
        .connect(&fx.peer)
        .unwrap_or_else(|e| panic!("connect: {e}"))
}

fn session(fx: &TransportFixture, sample: &Sample) -> Box<dyn Session> {
    let mut s = connect(fx);
    let root = s.hello(&h(b"client")).unwrap();
    assert_eq!(root, sample.root, "hello returned another root");
    s
}

fn expect_missing(what: &str, r: &Result<Vec<u8>>, id: &Hash) {
    assert!(
        matches!(r, Err(Error::NotFound(x)) if x == id),
        "{what} {id}: expected NotFound, got {r:?}"
    );
}

fn hello(fx: &TransportFixture, sample: &Sample) {
    session(fx, sample).bye().unwrap();
}

fn get_in_order(fx: &TransportFixture, sample: &Sample) {
    let mut s = session(fx, sample);
    let mut request = Vec::new();
    let mut want = Vec::new();
    for (i, o) in sample.objects.iter().enumerate() {
        request.push(h(o));
        want.push(Some(o));
        if i % 4 == 0 {
            request.push(h(&[b'u', i as u8]));
            want.push(None);
        }
    }
    request.push(h(&sample.objects[0])); // twice
    want.push(Some(&sample.objects[0]));
    let got = s.get(&request).unwrap();
    assert_eq!(got.len(), request.len(), "one result per requested id");
    for ((id, r), want) in request.iter().zip(&got).zip(want) {
        match want {
            Some(bytes) => assert_eq!(r.as_ref().ok(), Some(bytes), "bytes of {id}"),
            None => expect_missing("get of unknown", r, id),
        }
    }
    assert!(
        s.get(&[]).unwrap().is_empty(),
        "an empty get returns nothing"
    );
    s.bye().unwrap();
}

fn get_children(fx: &TransportFixture, sample: &Sample) {
    let mut s = session(fx, sample);
    let chunk = h(&sample.chunks[0]);
    let unknown = h(b"unknown node");
    let nodes = [
        sample.whole,
        sample.gap,
        sample.empty,
        sample.upper,
        chunk,
        unknown,
        sample.whole,
    ];
    let got = s.get_children(&nodes).unwrap();
    assert_eq!(got.len(), nodes.len(), "one result per node");
    let all = sample.chunks.concat();
    for i in [0, 6] {
        assert_eq!(
            got[i].as_ref().ok(),
            Some(&all),
            "a level-1 node's chunks, concatenated in node order"
        );
    }
    assert_eq!(
        got[2].as_ref().ok(),
        Some(&Vec::new()),
        "the node of empty content has no chunks"
    );
    for (i, why) in [
        (1, "with a missing chunk"),
        (3, "of level 2"),
        (4, "that is a chunk"),
        (5, "that is unknown"),
    ] {
        expect_missing(&format!("get_children of a node {why}"), &got[i], &nodes[i]);
    }
    s.bye().unwrap();
}

fn fetch(fx: &TransportFixture, sample: &Sample) {
    let mut s = session(fx, sample);
    let ids = |pushed: Vec<(Hash, Vec<u8>)>| {
        let n = pushed.len();
        for (id, bytes) in &pushed {
            assert_eq!(h(bytes), *id, "a pushed object under its own id");
        }
        let set: HashSet<Hash> = pushed.into_iter().map(|p| p.0).collect();
        assert_eq!(set.len(), n, "each object pushed once");
        set
    };
    let mut closure: HashSet<Hash> = sample.chunks.iter().map(|c| h(c)).collect();
    closure.extend([sample.root, sample.whole]);
    let all = ids(s.fetch(&sample.root, &[], b"").unwrap());
    assert_eq!(all, closure, "the closure of the root");
    let top = ids(s.fetch(&sample.root, &[sample.sibling], b"").unwrap());
    assert_eq!(top, HashSet::from([sample.root]), "not what a have reaches");
    let same = s.fetch(&sample.root, &[sample.root], b"").unwrap();
    assert!(same.is_empty(), "nothing when want is had");
    let none = s.fetch(&h(b"unknown"), &[], b"").unwrap();
    assert!(none.is_empty(), "nothing for an unknown want");
    let many = vec![sample.root; MAX_GET + 1];
    let r = s.fetch(&sample.root, &many, b"");
    assert!(
        matches!(r, Err(Error::Invalid(_))),
        "fetch with 4,097 haves: expected Invalid, got {:?}",
        r.map(|v| v.len())
    );
    let first = s
        .get(&[sample.root])
        .expect("a refused fetch sends nothing");
    assert!(first[0].is_ok(), "the session works after a refused fetch");
    s.bye().unwrap();
}

fn limits(fx: &TransportFixture, sample: &Sample) {
    let mut s = session(fx, sample);
    let ids: Vec<Hash> = (0..=MAX_GET as u32).map(|i| h(&i.to_le_bytes())).collect();
    let got = s.get(&ids[..MAX_GET]).unwrap();
    assert_eq!(got.len(), MAX_GET, "a get of exactly 4,096 ids");
    for (r, id) in got.iter().zip(&ids) {
        expect_missing("get of unknown", r, id);
    }
    let r = s.get(&ids);
    assert!(
        matches!(r, Err(Error::Invalid(_))),
        "get of 4,097 ids: expected Invalid, got {:?}",
        r.map(|v| v.len())
    );
    let r = s.get_children(&ids);
    assert!(
        matches!(r, Err(Error::Invalid(_))),
        "get_children of 4,097 nodes: expected Invalid, got {:?}",
        r.map(|v| v.len())
    );
    let first = s
        .get(&[sample.root])
        .expect("a refused request sends nothing");
    assert!(
        first[0].is_ok(),
        "the session works after a refused request"
    );
    s.bye().unwrap();
}

fn independent(fx: &TransportFixture, sample: &Sample) {
    let (mut a, mut b) = (session(fx, sample), session(fx, sample));
    let [x, y] = [&sample.objects[1], &sample.objects[2]];
    assert_eq!(b.get(&[h(x)]).unwrap()[0].as_ref().ok(), Some(x));
    assert_eq!(a.get(&[h(y)]).unwrap()[0].as_ref().ok(), Some(y));
    a.bye().unwrap();
    assert_eq!(b.get(&[h(y)]).unwrap()[0].as_ref().ok(), Some(y));
    b.bye().unwrap();
}

/// Runs last: a stopped peer's answers are `Unavailable`, never `NotFound`.
fn stopped(fx: &TransportFixture, sample: &Sample) {
    let mut s = session(fx, sample);
    (fx.stop)();
    let ids = [sample.root, h(b"unknown")];
    match s.get(&ids) {
        Err(Error::Unavailable(_)) => {}
        Ok(rs) if rs.iter().all(|r| matches!(r, Err(Error::Unavailable(_)))) => {}
        other => panic!("get from a stopped peer: expected Unavailable, got {other:?}"),
    }
    match fx.transport.connect(&fx.peer) {
        Err(Error::Unavailable(_)) => {}
        Err(e) => panic!("connect to a stopped peer: expected Unavailable, got {e:?}"),
        Ok(_) => panic!("connect to a stopped peer succeeded"),
    }
}

#[cfg(test)]
mod tests {
    //! The suite is checked against an in-process fake: a correct one passes,
    //! one that reports a stopped peer's objects as missing fails.
    use super::*;
    use crate::format::node::Node;
    use crate::iface::{Live, LiveEvents};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Peer {
        objects: HashMap<Hash, Vec<u8>>,
        root: Hash,
        protocol: u8,
        stopped: AtomicBool,
        /// Answers `NotFound` instead of `Unavailable` once stopped.
        lies: bool,
    }

    impl Peer {
        fn up(&self) -> Result<()> {
            match self.stopped.load(Ordering::Relaxed) && !self.lies {
                true => Err(Error::Unavailable("stopped".into())),
                false => Ok(()),
            }
        }

        fn object(&self, id: &Hash) -> Result<Vec<u8>> {
            match self.stopped.load(Ordering::Relaxed) {
                true => Err(Error::NotFound(*id)),
                false => self.objects.get(id).cloned().ok_or(Error::NotFound(*id)),
            }
        }

        /// Every stored id the profile `Versions` reaches from `roots`.
        fn closure(&self, roots: &[Hash]) -> HashSet<Hash> {
            use crate::iface::Profile;
            use crate::walk::{Kind, Versions, children};
            let root = Kind::from(Versions.root());
            let mut todo: Vec<(Hash, Kind)> = roots.iter().map(|r| (*r, root)).collect();
            let mut out = HashSet::new();
            while let Some((id, kind)) = todo.pop() {
                let Ok(bytes) = self.object(&id) else {
                    continue;
                };
                if out.insert(id) {
                    let below = children(&Versions, &id, kind, &bytes).unwrap_or_default();
                    todo.extend(below.into_iter().map(|c| (c.id, c.kind)));
                }
            }
            out
        }

        fn children(&self, id: &Hash) -> Result<Vec<u8>> {
            let node = Node::decode(&self.object(id)?).map_err(|_| Error::NotFound(*id))?;
            if node.level != 1 {
                return Err(Error::NotFound(*id));
            }
            let chunks = node.children.iter().map(|c| self.object(&c.id));
            let chunks: Result<Vec<_>> = chunks.collect();
            Ok(chunks.map_err(|_| Error::NotFound(*id))?.concat())
        }
    }

    struct Fake(Arc<Peer>);

    impl Transport for Fake {
        fn connect(&self, peer: &str) -> Result<Box<dyn Session>> {
            assert_eq!(peer, "peer");
            if self.0.stopped.load(Ordering::Relaxed) {
                return Err(Error::Unavailable("stopped".into()));
            }
            Ok(Box::new(Fake(self.0.clone())))
        }

        fn live(&self, _: &str, _: LiveEvents) -> Result<Arc<dyn Live>> {
            Err(Error::Invalid("the fake has no live sessions".into()))
        }
    }

    impl Fake {
        fn each(
            &self,
            ids: &[Hash],
            f: impl Fn(&Peer, &Hash) -> Result<Vec<u8>>,
        ) -> Result<Vec<Result<Vec<u8>>>> {
            if ids.len() > MAX_GET {
                return Err(Error::Invalid("too many ids".into()));
            }
            self.0.up()?;
            Ok(ids.iter().map(|id| f(&self.0, id)).collect())
        }
    }

    impl Session for Fake {
        fn hello(&mut self, _: &Hash) -> Result<Hash> {
            self.0.up()?;
            match self.0.protocol == VERSION {
                true => Ok(self.0.root),
                false => Err(Error::Incompatible("protocol".into())),
            }
        }
        fn get(&mut self, ids: &[Hash]) -> Result<Vec<Result<Vec<u8>>>> {
            self.each(ids, Peer::object)
        }
        fn get_children(&mut self, nodes: &[Hash]) -> Result<Vec<Result<Vec<u8>>>> {
            self.each(nodes, Peer::children)
        }
        fn fetch(&mut self, want: &Hash, have: &[Hash], _: &[u8]) -> Result<Vec<(Hash, Vec<u8>)>> {
            if have.len() > MAX_GET {
                return Err(Error::Invalid("too many haves".into()));
            }
            self.0.up()?;
            let had = self.0.closure(have);
            let pushed = self.0.closure(&[*want]).into_iter();
            Ok(pushed
                .filter(|id| !had.contains(id))
                .filter_map(|id| Some((id, self.0.object(&id).ok()?)))
                .collect())
        }
        fn bye(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn make(lies: bool) -> impl Fn(&[Vec<u8>], Hash, u8) -> TransportFixture {
        move |objects, root, protocol| {
            let peer = Arc::new(Peer {
                objects: objects.iter().map(|o| (h(o), o.clone())).collect(),
                root,
                protocol,
                stopped: AtomicBool::new(false),
                lies,
            });
            let p = peer.clone();
            TransportFixture {
                transport: Box::new(Fake(peer)),
                peer: "peer".into(),
                stop: Box::new(move || p.stopped.store(true, Ordering::Relaxed)),
            }
        }
    }

    #[test]
    fn correct_transport_passes() {
        transport(&make(false));
    }

    #[test]
    #[should_panic(expected = "expected Unavailable")]
    fn transport_that_reports_unreachable_as_missing_fails() {
        transport(&make(true));
    }
}
