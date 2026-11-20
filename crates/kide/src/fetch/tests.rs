use super::*;
use crate::auth::PeerKey;
use crate::change::Rescan;
use crate::conformance::Plain;
use crate::conformance::{Rng, TempDir};
use crate::folder::Folder;
use crate::folder::net::Manifests;
use crate::folder::scan::scan;
use crate::folder::verify::verify;
use crate::format::content::write_content;
use crate::format::manifest::{Kind as FileKind, PathEntry, build_manifest};
use crate::format::record::Record;
use crate::iface::MAX_GET;
use crate::iface::{Content, Profile, Want};
use crate::iface::{Transport, Workspace};
use crate::names::Platform;
use crate::store::{MemStore, PackStore};
use crate::transport::{InProcess, Served, Server, Tcp};
use crate::workspace::PlainFolder;
use std::sync::{Arc, Mutex};

const OLD: i64 = 1_600_000_000_000_000_000;

/// A scanned folder: the source of a fetch.
struct Source {
    tmp: TempDir,
    folder: Folder,
}

impl Source {
    fn new(files: usize) -> Source {
        let tmp = TempDir::new("fetch");
        Folder::init(tmp.path()).unwrap();
        let folder = Folder::open_unsynced(tmp.path()).unwrap();
        let mut s = Source { tmp, folder };
        let mut rng = Rng::new(5);
        for i in 0..files {
            s.write(&format!("d{}/f{i}", i % 3), &rng.bytes(200 + 900 * i));
        }
        s.write("empty/", b"");
        s.scan();
        s
    }

    fn write(&self, path: &str, data: &[u8]) {
        let kind = match path.ends_with('/') {
            true => FileKind::Directory,
            false => FileKind::Regular,
        };
        let ws = PlainFolder::new(self.tmp.path());
        ws.write(path, kind, &mut &data[..], OLD).unwrap();
    }

    fn scan(&mut self) {
        let ws = PlainFolder::new(self.tmp.path());
        scan(&mut self.folder, &ws, &mut Rescan, &Platform::LINUX).unwrap();
    }

    fn served(&self) -> Served {
        let head = self.folder.head;
        Served::new(self.folder.store.clone(), move || head)
    }

    fn session(&self) -> Box<dyn Session> {
        let t = InProcess::default();
        t.register("source", self.served());
        let mut s = t.connect("source").unwrap();
        assert_eq!(s.hello(&Hash::ZERO).unwrap(), self.folder.head);
        s
    }
}

/// The fetching side: an empty store and its staging.
struct Target {
    tmp: TempDir,
    store: MemStore,
    staging: PackStore,
}

impl Target {
    fn new() -> Target {
        let tmp = TempDir::new("fetch-target");
        let staging = PackStore::open(tmp.path()).unwrap();
        Target {
            tmp,
            store: MemStore::new(),
            staging,
        }
    }

    fn fetch(&self, s: &mut dyn Session, root: &Hash) -> Result<FetchReport> {
        fetch(s, &self.store, &self.staging, &Manifests, root, &[])
    }

    fn assert_holds(&self, root: &Hash) {
        let r = verify(&self.store, root).unwrap();
        assert!(r.ok(), "{r:?}");
        assert!(self.staging.ids().unwrap().is_empty(), "staging is cleared");
    }

    fn assert_unchanged(&self) {
        assert!(self.store.ids().unwrap().is_empty(), "nothing is committed");
        let _ = self.tmp.path();
    }
}

/// Alters one answer that arrived: (GetChildren, id, answer).
type Tamper = fn(bool, &Hash, &mut Result<Vec<u8>>);

/// A session that records requests, breaks after `left` answers, or alters
/// answers with `tamper`.
struct Wrap {
    inner: Box<dyn Session>,
    left: usize,
    tamper: Tamper,
    gets: Arc<Mutex<Vec<Hash>>>,
    nodes: Arc<Mutex<Vec<Hash>>>,
}

impl Wrap {
    fn new(inner: Box<dyn Session>) -> Wrap {
        Wrap {
            inner,
            left: usize::MAX,
            tamper: |_, _, _| {},
            gets: Arc::default(),
            nodes: Arc::default(),
        }
    }

    fn pass(&mut self, children: bool, ids: &[Hash], rs: &mut [Result<Vec<u8>>]) {
        for (id, r) in ids.iter().zip(rs) {
            if self.left == 0 {
                *r = Err(Error::Unavailable("broken".into()));
            } else if r.is_ok() {
                self.left -= 1;
                (self.tamper)(children, id, r);
            }
        }
    }
}

impl Session for Wrap {
    fn hello(&mut self, root: &Hash) -> Result<Hash> {
        self.inner.hello(root)
    }
    fn get(&mut self, ids: &[Hash]) -> Result<Vec<Result<Vec<u8>>>> {
        self.gets.lock().unwrap().extend(ids);
        let mut rs = self.inner.get(ids)?;
        self.pass(false, ids, &mut rs);
        Ok(rs)
    }
    fn get_children(&mut self, nodes: &[Hash]) -> Result<Vec<Result<Vec<u8>>>> {
        self.nodes.lock().unwrap().extend(nodes);
        let mut rs = self.inner.get_children(nodes)?;
        self.pass(true, nodes, &mut rs);
        Ok(rs)
    }
    fn fetch(&mut self, want: &Hash, have: &[Hash], scope: &[u8]) -> Result<Vec<(Hash, Vec<u8>)>> {
        self.inner.fetch(want, have, scope)
    }
    fn bye(&mut self) -> Result<()> {
        self.inner.bye()
    }
}

#[test]
fn a_folder_is_fetched_over_in_process() {
    let src = Source::new(12);
    let t = Target::new();
    let mut s = Wrap::new(src.session());
    let nodes = s.nodes.clone();
    let r = t.fetch(&mut s, &src.folder.head).unwrap();
    t.assert_holds(&src.folder.head);
    assert!(
        r.nodes > 0,
        "whole level-1 nodes come by GetChildren: {r:?}"
    );
    assert_eq!(
        r.nodes,
        nodes.lock().unwrap().len(),
        "every one is answered"
    );
    assert_eq!(r.committed, t.store.ids().unwrap().len());
    let again = t.fetch(&mut s, &src.folder.head).unwrap();
    // Only the opening Fetch, answered with nothing.
    let nothing = FetchReport {
        rounds: 1,
        ..FetchReport::default()
    };
    assert_eq!(again, nothing, "a stored root needs nothing");
}

#[test]
fn a_folder_is_fetched_over_tcp() {
    let src = Source::new(12);
    let local = vec![([1; 32], "127.0.0.1".parse().unwrap())];
    let server = Server::start("127.0.0.1:0".parse().unwrap(), local, src.served()).unwrap();
    let mut s = Tcp::default().connect(&server.addr().to_string()).unwrap();
    let root = s.hello(&Hash::ZERO).unwrap();
    let t = Target::new();
    t.fetch(&mut *s, &root).unwrap();
    s.bye().unwrap();
    t.assert_holds(&src.folder.head);
}

#[test]
fn history_comes_without_its_content() {
    let mut src = Source::new(3);
    let path = "d0/photo.jpg";
    src.write(path, b"first");
    src.scan();
    let old = src.folder.index.0[path].version;
    src.write(path, b"edited");
    src.scan();
    let t = Target::new();
    t.fetch(&mut *src.session(), &src.folder.head).unwrap();
    t.assert_holds(&src.folder.head);
    let old = load::record(&t.store, &old).expect("the parent is history");
    assert!(!t.store.has(&old.root).unwrap(), "history has no content");
}

#[test]
fn an_interrupted_fetch_resumes_from_staging() {
    let src = Source::new(20);
    let t = Target::new();
    let mut s = Wrap::new(src.session());
    s.left = 25;
    let r = t.fetch(&mut s, &src.folder.head);
    assert!(matches!(r, Err(Error::Unavailable(_))), "{r:?}");
    t.assert_unchanged();
    let staged: HashSet<Hash> = t.staging.ids().unwrap().into_iter().collect();
    assert!(staged.len() >= 25, "{} staged", staged.len());

    let mut s = Wrap::new(src.session());
    let (gets, nodes) = (s.gets.clone(), s.nodes.clone());
    t.fetch(&mut s, &src.folder.head).unwrap();
    t.assert_holds(&src.folder.head);
    let gets = gets.lock().unwrap();
    assert!(!gets.is_empty());
    assert!(
        gets.iter().all(|id| !staged.contains(id)),
        "no staged object is requested again"
    );
    for n in nodes.lock().unwrap().iter() {
        let node = load::node(&t.store, n).unwrap();
        assert!(node.children.iter().all(|c| !staged.contains(&c.id)));
    }
}

#[test]
fn chunks_of_a_node_without_children_come_by_get() {
    let src = Source::new(6);
    let t = Target::new();
    let mut s = Wrap::new(src.session());
    let nodes = s.nodes.clone();
    s.tamper = |children, id, r| {
        if children {
            *r = Err(Error::NotFound(*id));
        }
    };
    let r = t.fetch(&mut s, &src.folder.head).unwrap();
    t.assert_holds(&src.folder.head);
    assert_eq!(r.nodes, 0, "{r:?}");
    assert!(!nodes.lock().unwrap().is_empty(), "GetChildren was tried");
}

#[test]
fn a_corrupt_object_commits_nothing() {
    let src = Source::new(4);
    let t = Target::new();
    let mut s = Wrap::new(src.session());
    s.tamper = |children, _, r| {
        if let Ok(b) = r
            && !children
            && b[0] == 0
        {
            b[40] ^= 1; // a record without parents: its state
        }
    };
    let r = t.fetch(&mut s, &src.folder.head);
    assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
    t.assert_unchanged();
}

#[test]
fn children_with_a_wrong_total_are_corrupt_for_that_node() {
    let src = Source::new(4);
    let t = Target::new();
    let mut s = Wrap::new(src.session());
    let nodes = s.nodes.clone();
    s.tamper = |children, _, r| {
        if let Ok(b) = r
            && children
        {
            b.push(0);
        }
    };
    let r = t.fetch(&mut s, &src.folder.head);
    let first = nodes.lock().unwrap()[0];
    assert!(matches!(r, Err(Error::Corrupt(x)) if x == first), "{r:?}");
    t.assert_unchanged();
}

#[test]
fn a_missing_object_is_not_found_and_keeps_staging() {
    let src = Source::new(4);
    let gone = load::record(&*src.folder.store, &src.folder.index.0["d1/f1"].version)
        .unwrap()
        .root;
    src.folder.store.remove(&gone).unwrap();
    let t = Target::new();
    let r = t.fetch(&mut *src.session(), &src.folder.head);
    assert!(matches!(r, Err(Error::NotFound(x)) if x == gone), "{r:?}");
    t.assert_unchanged();
    assert!(
        !t.staging.ids().unwrap().is_empty(),
        "staged objects remain"
    );
}

/// A manifest of one path whose record is a merge of two edits of `p`.
struct Merged {
    store: Arc<MemStore>,
    root: Hash,
    leaves: [Hash; 2],
    p: Hash,
}

fn merged(merge_parent: bool) -> Merged {
    let s = Arc::new(MemStore::new());
    let content = |d: &[u8]| write_content(d, &mut |id, b| s.put_known(id, b)).unwrap();
    let record = |parents: Vec<Hash>, data: &[u8]| {
        let mut parents = parents;
        parents.sort();
        let r = Record {
            change: Vec::new(),
            parents,
            root: content(data).id,
            state: FileKind::Regular.state(),
        };
        s.put(&r.encode().unwrap()).unwrap()
    };
    let p = record(vec![], b"first");
    let leaves = [record(vec![p], b"left"), record(vec![p], b"right")];
    let mut m = record(leaves.to_vec(), b"merged");
    if merge_parent {
        let other = record(vec![], b"other");
        m = record(vec![m, other], b"merged twice");
    }
    let entry = PathEntry {
        path: "f.bin".into(),
        version: m,
        size: if merge_parent { 12 } else { 6 },
        mtime: 1,
    };
    let root = build_manifest(&[entry], &mut |id, b| s.put_known(id, b))
        .unwrap()
        .id;
    Merged {
        store: s,
        root,
        leaves,
        p,
    }
}

fn session_on(store: Arc<MemStore>, root: Hash) -> Box<dyn Session> {
    let t = InProcess::default();
    t.register("m", Served::new(store, move || root));
    let mut s = t.connect("m").unwrap();
    s.hello(&Hash::ZERO).unwrap();
    s
}

#[test]
fn merge_leaves_come_with_their_content() {
    let m = merged(false);
    let t = Target::new();
    t.fetch(&mut *session_on(m.store.clone(), m.root), &m.root)
        .unwrap();
    t.assert_holds(&m.root);
    for leaf in m.leaves {
        let r = load::record(&t.store, &leaf).unwrap();
        assert!(t.store.has(&r.root).unwrap(), "a leaf's content");
    }
    let p = load::record(&t.store, &m.p).unwrap();
    assert!(
        !t.store.has(&p.root).unwrap(),
        "the leaves' parent is history"
    );
}

#[test]
fn a_merge_below_a_merge_is_invalid() {
    let m = merged(true);
    let t = Target::new();
    let r = t.fetch(&mut *session_on(m.store.clone(), m.root), &m.root);
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    t.assert_unchanged();
}

/// A tree of plain objects (`walk::Plain`): a root linking to two objects
/// that share a third, which names a record with content.
fn plain_tree() -> (Arc<MemStore>, Hash, Vec<Hash>) {
    let s = Arc::new(MemStore::new());
    let content = write_content(b"leaf data", &mut |id, b| s.put_known(id, b)).unwrap();
    let r = Record {
        change: Vec::new(),
        parents: vec![],
        root: content.id,
        state: vec![0],
    };
    let rec = s.put(&r.encode().unwrap()).unwrap();
    let put = |links: &[(Want, Hash)]| s.put(&Plain::object(links)).unwrap();
    let shared = put(&[(Want::Record(Content::Bytes), rec)]);
    let a = put(&[(Want::Object, shared)]);
    let b = put(&[(Want::Object, shared), (Want::Object, a)]);
    let root = put(&[(Want::Object, a), (Want::Object, b)]);
    (s, root, vec![root, a, b, shared, rec, content.id])
}

#[test]
fn a_profile_of_plain_objects_is_fetched() {
    let (store, root, ids) = plain_tree();
    let t = Target::new();
    let mut s = session_on(store, root);
    let r = fetch(&mut *s, &t.store, &t.staging, &Plain, &root, &[]).unwrap();
    assert_eq!(r.committed, t.store.ids().unwrap().len());
    for id in ids {
        assert!(t.store.has(&id).unwrap(), "{id}");
    }
    assert!(t.staging.ids().unwrap().is_empty());
}

/// A history of checkpoints (`Content::Objects`): `old`, its children
/// `left` and `right`, and `new`, their merge. The root names `new`, labelled
/// `0`, so each checkpoint's own object is at path `0/0`. Returns the store,
/// the root and the (record, object) pairs old, left, right, new.
fn checkpoints() -> (Arc<MemStore>, Hash, [(Hash, Hash); 4]) {
    let s = Arc::new(MemStore::new());
    let c = |parents, data: &[u8]| Plain::checkpoint(&*s, parents, data).unwrap();
    let old = c(vec![], b"old");
    let (left, right) = (c(vec![old.0], b"left"), c(vec![old.0], b"right"));
    let new = c(vec![left.0, right.0], b"new");
    let root = s
        .put(&Plain::object(&[(Want::Record(Content::Objects), new.0)]))
        .unwrap();
    (s, root, [old, left, right, new])
}

#[test]
fn a_checkpoint_comes_with_its_objects_and_a_merge_with_its_leaves() {
    let (store, root, [old, left, right, new]) = checkpoints();
    let t = Target::new();
    let mut s = session_on(store, root);
    fetch(&mut *s, &t.store, &t.staging, &Plain, &root, &[]).unwrap();
    for (_, object) in [left, right, new] {
        assert!(t.store.has(&object).unwrap(), "{object}");
    }
    assert!(t.store.has(&old.0).unwrap(), "history");
    assert!(!t.store.has(&old.1).unwrap(), "history only");
}

#[test]
fn a_partial_walk_filters_inside_a_checkpoint() {
    // Paths: the root names `new` at 0, a checkpoint's object is at 0/0 and
    // the content that object links to at 0/0/0.
    let (store, root, [_, left, _, new]) = checkpoints();
    let below = |object: &Hash| Plain.children(&store.get(object).unwrap()).unwrap()[0].id;
    let (left_below, new_below) = (below(&left.1), below(&new.1));
    let served = Served::new(store, move || root);
    let t = Target::new();
    let filter = |p: &str| p != "0/0/0";
    let mut w = Walk::partial(&t.store, &t.staging, &Plain, &root, &filter);
    drive(&mut w, &served, usize::MAX);
    w.commit().unwrap();
    assert!(t.store.has(&new.1).unwrap() && t.store.has(&left.1).unwrap());
    assert!(!t.store.has(&new_below).unwrap() && !t.store.has(&left_below).unwrap());
}

/// Answers a request from `served`, as a session would.
fn answer(served: &Served, r: &Request) -> Vec<Result<Vec<u8>>> {
    let (ids, children) = match r {
        Request::Get(ids) => (ids, false),
        Request::GetChildren(ids) => (ids, true),
        Request::Fetch { .. } => panic!("these walks send no Fetch"),
    };
    let one = |id: &Hash| match children {
        true => served.children(id),
        false => served.object(id),
    };
    ids.iter()
        .map(|id| one(id).ok_or(Error::NotFound(*id)))
        .collect()
}

#[test]
fn a_walk_driven_by_hand_hands_out_a_layer_at_once() {
    let s = Arc::new(MemStore::new());
    let n = MAX_GET + 10;
    let links: Vec<_> = (0..n)
        .map(|i| {
            let c = write_content(&i.to_le_bytes(), &mut |id, b| s.put_known(id, b));
            (Want::Content, c.unwrap().id)
        })
        .collect();
    let root = s.put(&Plain::object(&links)).unwrap();
    let served = Served::new(s.clone(), move || root);
    let t = Target::new();
    let mut w = Walk::new(&t.store, &t.staging, &Plain, &root);
    let mut layers = Vec::new();
    loop {
        let mut out = Vec::new();
        while let Some(r) = w.request().unwrap() {
            out.push(r);
        }
        if out.is_empty() {
            break;
        }
        assert!(!w.is_done());
        let sizes = out.iter().map(|r| match r {
            Request::Get(ids) => ('g', ids.len()),
            Request::GetChildren(ids) => ('c', ids.len()),
            Request::Fetch { have, .. } => ('f', have.len()),
        });
        layers.push(sizes.collect::<Vec<_>>());
        for r in &out {
            w.answer(answer(&served, r)).unwrap();
        }
    }
    let gets = vec![('g', MAX_GET), ('g', 10)];
    let children = vec![('c', MAX_GET), ('c', 10)];
    assert_eq!(layers, [vec![('g', 1)], gets, children]);
    assert!(w.is_done());
    let r = w.commit().unwrap();
    assert_eq!(r.committed, s.ids().unwrap().len());
    assert_eq!(r.nodes, n);
}

#[test]
fn a_walk_refuses_a_stray_answer_and_an_early_commit() {
    let (store, root, _) = plain_tree();
    let t = Target::new();
    let mut w = Walk::new(&t.store, &t.staging, &Plain, &root);
    assert!(matches!(w.answer(vec![]), Err(Error::Invalid(_))));
    let Some(r) = w.request().unwrap() else {
        panic!("the root is requested");
    };
    let served = Served::new(store, move || root);
    w.answer(answer(&served, &r)).unwrap();
    assert!(matches!(w.commit(), Err(Error::Invalid(_))));
    t.assert_unchanged();
}

/// Drives `w` over `served` to the end, or until `left` answers are taken.
fn drive(w: &mut Walk, served: &Served, mut left: usize) {
    while left > 0
        && let Some(r) = w.request().unwrap()
    {
        w.answer(answer(served, &r)).unwrap();
        left -= 1;
    }
}

#[test]
fn a_partial_walk_descends_only_where_its_filter_accepts() {
    // root -0-> a -0-> shared -0-> record; root -1-> b -> shared, a.
    let (store, root, ids) = plain_tree();
    let [_, a, b, shared, rec, content] = ids[..] else {
        panic!("six objects");
    };
    let served = Served::new(store, move || root);
    let t = Target::new();
    let asked = Mutex::new(Vec::new());
    let filter = |p: &str| {
        asked.lock().unwrap().push(p.to_string());
        !p.starts_with('1')
    };
    let mut w = Walk::partial(&t.store, &t.staging, &Plain, &root, &filter);
    drive(&mut w, &served, usize::MAX);
    assert!(w.is_done());
    w.commit().unwrap();
    for id in [root, a, shared, rec, content] {
        assert!(t.store.has(&id).unwrap(), "{id}");
    }
    assert!(!t.store.has(&b).unwrap(), "path 1 is filtered out");
    let mut asked = asked.into_inner().unwrap();
    asked.sort();
    assert_eq!(asked, ["0", "0/0", "0/0/0", "1"], "once per labelled link");
}

#[test]
fn a_partial_walk_leaves_absent_objects_and_fills_them_later() {
    let (store, root, ids) = plain_tree();
    let shared = ids[3];
    let bytes = store.get(&shared).unwrap();
    store.remove(&shared).unwrap();
    let served = Served::new(store.clone(), move || root);
    let t = Target::new();
    let all = |_: &str| true;
    let mut w = Walk::partial(&t.store, &t.staging, &Plain, &root, &all);
    drive(&mut w, &served, usize::MAX);
    let r = w.commit().unwrap();
    assert_eq!((r.absent, r.committed), (1, 3), "{r:?}");
    assert!(!t.store.has(&shared).unwrap());

    store.put(&bytes).unwrap();
    let mut w = Walk::partial(&t.store, &t.staging, &Plain, &root, &all);
    drive(&mut w, &served, usize::MAX);
    let r = w.commit().unwrap();
    assert_eq!(r.committed, 4, "below stored objects, a chunk too: {r:?}");
    for id in ids {
        assert!(t.store.has(&id).unwrap(), "{id}");
    }

    let mut strict = Walk::new(&t.store, &t.staging, &Plain, &root);
    assert!(
        strict.request().unwrap().is_none(),
        "a stored root is whole"
    );
}

#[test]
fn a_partial_walk_may_stop_early() {
    let (store, root, ids) = plain_tree();
    let served = Served::new(store, move || root);
    let t = Target::new();
    let all = |_: &str| true;
    let mut w = Walk::partial(&t.store, &t.staging, &Plain, &root, &all);
    drive(&mut w, &served, 2);
    assert!(!w.is_done());
    let r = w.commit().unwrap();
    assert_eq!(r.committed, 3, "the root and its children: {r:?}");
    assert!(t.store.has(&ids[1]).unwrap() && !t.store.has(&ids[3]).unwrap());
    assert!(t.staging.ids().unwrap().is_empty());
}

/// A server of `store` at `root` that lets a peer read a path if `may` says
/// so, over the plain profile.
fn reading(
    store: Arc<dyn ObjectStore>,
    root: Hash,
    profile: Arc<dyn Profile>,
    may: impl Fn(Option<&PeerKey>, &str) -> bool + Send + Sync + 'static,
) -> Served {
    Served {
        profile: Some(profile),
        reads: Some(Arc::new(may)),
        ..Served::new(store, move || root)
    }
}

fn connect(served: Served) -> Box<dyn Session> {
    let t = InProcess::default();
    t.register("r", served);
    let mut s = t.connect("r").unwrap();
    s.hello(&Hash::ZERO).unwrap();
    s
}

/// Drives a walk over a session to the end and commits it.
fn run(mut w: Walk, s: &mut dyn Session) -> FetchReport {
    while let Some(r) = w.request().unwrap() {
        match &r {
            Request::Get(ids) => w.answer(s.get(ids).unwrap()).unwrap(),
            Request::GetChildren(ids) => w.answer(s.get_children(ids).unwrap()).unwrap(),
            Request::Fetch { want, have, scope } => {
                w.pushed(s.fetch(want, have, scope).unwrap()).unwrap()
            }
        }
    }
    w.commit().unwrap()
}

#[test]
fn a_peer_is_served_only_what_it_may_read() {
    let (store, root, ids) = plain_tree();
    let [_, a, b, shared, ..] = ids[..] else {
        panic!("six objects");
    };
    let unlinked = store.put(b"stored, linked from nowhere").unwrap();
    let served = reading(store, root, Arc::new(Plain), |_, p| !p.starts_with('1'));
    let mut s = connect(served);
    let t = Target::new();
    let all = |_: &str| true;
    let w = Walk::partial(&t.store, &t.staging, &Plain, &root, &all);
    let r = run(w, &mut *s);
    assert_eq!(r.absent, 1, "b, at path 1: {r:?}");
    assert!(t.store.has(&a).unwrap() && t.store.has(&shared).unwrap());
    assert!(!t.store.has(&b).unwrap());
    let got = s.get(&[b, unlinked, a]).unwrap();
    assert!(
        matches!(got[0], Err(Error::NotFound(x)) if x == b),
        "unreadable"
    );
    assert!(matches!(got[1], Err(Error::NotFound(_))), "never reached");
    assert!(got[2].is_ok(), "granted earlier");
}

#[test]
fn grants_reach_below_objects_the_client_already_holds() {
    let (store, root, ids) = plain_tree();
    let served = reading(store, root, Arc::new(Plain), |_, _| true);
    let t = Target::new();
    let top = |p: &str| p == "0";
    let w = Walk::partial(&t.store, &t.staging, &Plain, &root, &top);
    run(w, &mut *connect(served.clone()));
    assert!(
        !t.store.has(&ids[3]).unwrap(),
        "shared, at 0/0, is left out"
    );
    let all = |_: &str| true;
    let w = Walk::partial(&t.store, &t.staging, &Plain, &root, &all);
    let r = run(w, &mut *connect(served));
    assert_eq!(r.absent, 0, "{r:?}");
    for id in ids {
        assert!(t.store.has(&id).unwrap(), "{id}");
    }
}

#[test]
fn read_rights_follow_folder_paths() {
    let src = Source::new(6);
    let head = src.folder.head;
    let store = src.folder.store.clone();
    let everything = reading(store.clone(), head, Arc::new(Manifests), |_, _| true);
    let t = Target::new();
    let w = Walk::new(&t.store, &t.staging, &Manifests, &head);
    let r = run(w, &mut *connect(everything.clone()));
    assert!(r.nodes > 0, "GetChildren under grants: {r:?}");
    t.assert_holds(&head);
    let t = Target::new();
    let r = t.fetch(&mut *connect(everything), &head).unwrap();
    assert_eq!((r.requested, r.nodes), (0, 0), "a push under grants: {r:?}");
    t.assert_holds(&head);

    let d1 = reading(store, head, Arc::new(Manifests), |_, p| {
        !p.starts_with("d1/")
    });
    let t = Target::new();
    let r = t.fetch(&mut *connect(d1), &head);
    assert!(
        matches!(r, Err(Error::NotFound(_))),
        "a whole walk needs d1: {r:?}"
    );
}

#[test]
fn the_peer_is_the_device_the_connection_comes_from() {
    let (store, root, ids) = plain_tree();
    let device: PeerKey = [7; 32];
    let served = reading(store, root, Arc::new(Plain), move |peer, p| {
        peer == Some(&device) || !p.starts_with('1')
    });
    let at = |key| {
        let local = vec![(key, "127.0.0.1".parse().unwrap())];
        Server::start("127.0.0.1:0".parse().unwrap(), local, served.clone()).unwrap()
    };
    for (key, reads_b) in [(device, true), ([8; 32], false)] {
        let server = at(key);
        let mut s = Tcp::default().connect(&server.addr().to_string()).unwrap();
        s.hello(&Hash::ZERO).unwrap();
        let got = s.get(&[root]).unwrap();
        assert!(got[0].is_ok());
        let got = s.get(&[ids[2]]).unwrap();
        assert_eq!(got[0].is_ok(), reads_b, "{key:?}");
    }
}

/// The source served with its profile, so that it pushes.
fn pushing(src: &Source) -> Served {
    Served {
        profile: Some(Arc::new(Manifests)),
        ..src.served()
    }
}

#[test]
fn a_fetch_pushes_a_folder_in_one_round_trip() {
    let mut src = Source::new(8);
    let t = Target::new();
    let r = fetch(
        &mut *connect(pushing(&src)),
        &t.store,
        &t.staging,
        &Manifests,
        &src.folder.head,
        &[],
    )
    .unwrap();
    t.assert_holds(&src.folder.head);
    assert_eq!((r.requested, r.nodes), (0, 0), "nothing left to Get: {r:?}");
    assert_eq!(r.pushed, r.committed);

    let old = src.folder.head;
    src.write("d0/f0", b"edited");
    src.scan();
    let head = src.folder.head;
    let r = fetch(
        &mut *connect(pushing(&src)),
        &t.store,
        &t.staging,
        &Manifests,
        &head,
        &[old],
    )
    .unwrap();
    t.assert_holds(&head);
    assert_eq!(r.requested, 0, "{r:?}");
    assert!(
        r.pushed < src.folder.store.ids().unwrap().len() / 2,
        "only the change: {r:?}"
    );
}

#[test]
fn a_scope_narrows_the_push_and_the_walk_fetches_the_rest() {
    let (store, root, ids) = plain_tree();
    let served = Served {
        profile: Some(Arc::new(Plain)),
        scope: Some(Arc::new(|scope: &[u8], path: &str| {
            path.as_bytes().starts_with(scope)
        })),
        ..Served::new(store, move || root)
    };
    let t = Target::new();
    let w = Walk::new(&t.store, &t.staging, &Plain, &root).with_fetch(vec![], b"0".to_vec());
    let r = run(w, &mut *connect(served));
    assert!(r.pushed > 0 && r.requested > 0, "{r:?}");
    assert!(t.store.has(&ids[2]).unwrap(), "b, at path 1, came by Get");
}

#[test]
fn a_pushed_object_under_another_id_is_corrupt() {
    let (_, root, _) = plain_tree();
    let t = Target::new();
    let mut w = Walk::new(&t.store, &t.staging, &Plain, &root).with_fetch(vec![], vec![]);
    let r = w.request().unwrap();
    assert!(matches!(r, Some(Request::Fetch { .. })), "{r:?}");
    let r = w.pushed(vec![(root, b"not the root".to_vec())]);
    assert!(matches!(r, Err(Error::Corrupt(x)) if x == root), "{r:?}");
}
