//! Fetching a closure (§4.3): everything a root needs that the store lacks,
//! layer by layer, validated by kind on receipt (§2.3, §3.1, and the
//! profile's rules) and staged; once all of it is staged, it moves into the
//! store children first, so the invariants of §4.1 hold. Any error ends the
//! fetch: staged objects remain, the store is unchanged.
//!
//! `Walk` is the fetch as a state machine: it hands out requests and takes
//! their answers, in order, and never touches the network. `fetch` drives it
//! over a blocking `Session`.

use crate::error::{Error, Result, invalid};
use crate::format::node::Node;
use crate::hash::{Hash, h};
use crate::iface::Profile;
use crate::iface::{MAX_GET, ObjectStore, Session};
use crate::load::{self, split_children};
use crate::walk::{Child, Data, Kind, children, join};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchReport {
    /// Ids sent in Get requests.
    pub requested: usize,
    /// Level-1 nodes whose chunks came in one Children answer each.
    pub nodes: usize,
    /// Bytes of the objects and Children answers received.
    pub bytes: u64,
    /// Objects moved from staging into the store.
    pub committed: usize,
    /// Ids the server lacked, left absent by a partial walk.
    pub absent: usize,
    /// Objects the server pushed in answer to Fetch.
    pub pushed: usize,
    /// Requests answered: Fetch, Get and GetChildren, each a round trip
    /// unless pipelined.
    pub rounds: usize,
}

/// A request of a walk, of 1 to 4,096 ids (§4.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Get(Vec<Hash>),
    /// The chunks of level-1 nodes.
    GetChildren(Vec<Hash>),
    /// What `want` reaches and the `have`s do not, within `scope` (§4.3).
    Fetch {
        want: Hash,
        have: Vec<Hash>,
        scope: Vec<u8>,
    },
}

/// Fetches the closure of the profile root `root` from `session` into
/// `store`, through `staging`: first a Fetch of what the roots in `have`,
/// whose closures are stored, do not reach, then whatever the push left out.
/// Objects staged by an interrupted fetch are used without fetching them
/// again, while their children are still walked.
pub fn fetch(
    session: &mut dyn Session,
    store: &dyn ObjectStore,
    staging: &dyn ObjectStore,
    profile: &dyn Profile,
    root: &Hash,
    have: &[Hash],
) -> Result<FetchReport> {
    let mut w = Walk::new(store, staging, profile, root).with_fetch(have.to_vec(), Vec::new());
    while let Some(r) = w.request()? {
        match &r {
            Request::Get(ids) => w.answer(session.get(ids)?)?,
            Request::GetChildren(nodes) => w.answer(session.get_children(nodes)?)?,
            Request::Fetch { want, have, scope } => w.pushed(session.fetch(want, have, scope)?)?,
        }
    }
    w.commit()
}

/// The fetch of one root, driven by the caller: `request` hands out requests,
/// `answer` (or `pushed`, for a Fetch) takes the answer to the oldest one
/// handed out, and `commit` moves what was fetched into the store once the
/// walk is done. A walk may begin with a Fetch (`with_fetch`): it stages
/// what the server pushes, and walks it before it asks for anything else. The
/// requests of a layer's Gets are handed out together, so they may be
/// pipelined; the layer's GetChildren follow once those are answered.
///
/// A partial walk (§4.1) fetches a partial tree: it descends into stored
/// objects too, only into paths its filter accepts, leaves an object the
/// server lacks absent, and may commit before it is done.
pub struct Walk<'a> {
    store: &'a dyn ObjectStore,
    staging: &'a dyn ObjectStore,
    profile: &'a dyn Profile,
    root: Hash,
    /// Set for a partial walk: whether to descend into a path.
    filter: Option<&'a dyn Fn(&str) -> bool>,
    /// Every object walked or queued, with its path in a partial walk;
    /// each is walked once.
    seen: HashSet<Item>,
    /// The children of every walked object, for the commit order.
    edges: HashMap<Hash, Vec<Hash>>,
    /// Walked objects that are staged, to commit.
    staged: HashSet<Hash>,
    /// What the server lacked, in a partial walk; never asked for again.
    absent: HashSet<Hash>,
    /// The next layer.
    next: Vec<Item>,
    /// How each id requested by Get in this layer is wanted, and where.
    wanted: HashMap<Hash, Vec<(Kind, String)>>,
    /// Level-1 nodes of this layer whose chunks come by GetChildren.
    whole: Vec<(Hash, Node, Data)>,
    /// The chunks of those nodes.
    claimed: HashSet<Hash>,
    /// Requests of this layer not yet handed out.
    queue: VecDeque<Request>,
    /// Requests handed out and not yet answered, with the nodes of each
    /// GetChildren.
    open: VecDeque<Open>,
    /// Whether this layer's GetChildren are queued.
    children_queued: bool,
    report: FetchReport,
}

/// An object to walk: its id, its kind and, in a partial walk, its path.
type Item = (Hash, Kind, String);

enum Open {
    Get(Vec<Hash>),
    Children(Vec<(Hash, Node, Data)>),
    Fetch,
}

impl<'a> Walk<'a> {
    /// A walk from `root`, wanted as `profile` says.
    pub fn new(
        store: &'a dyn ObjectStore,
        staging: &'a dyn ObjectStore,
        profile: &'a dyn Profile,
        root: &Hash,
    ) -> Walk<'a> {
        Walk::with(store, staging, profile, root, None)
    }

    /// A partial walk from `root` that descends only into the paths
    /// `filter` accepts; it is asked once per labelled link.
    pub fn partial(
        store: &'a dyn ObjectStore,
        staging: &'a dyn ObjectStore,
        profile: &'a dyn Profile,
        root: &Hash,
        filter: &'a dyn Fn(&str) -> bool,
    ) -> Walk<'a> {
        Walk::with(store, staging, profile, root, Some(filter))
    }

    /// Begins the walk with a Fetch of what the roots in `have` do not
    /// reach, within `scope`.
    pub fn with_fetch(mut self, have: Vec<Hash>, scope: Vec<u8>) -> Walk<'a> {
        let want = self.root;
        self.queue.push_back(Request::Fetch { want, have, scope });
        self
    }

    fn with(
        store: &'a dyn ObjectStore,
        staging: &'a dyn ObjectStore,
        profile: &'a dyn Profile,
        root: &Hash,
        filter: Option<&'a dyn Fn(&str) -> bool>,
    ) -> Walk<'a> {
        let first = (*root, Kind::from(profile.root()), String::new());
        Walk {
            store,
            staging,
            profile,
            root: *root,
            filter,
            seen: HashSet::from([first.clone()]),
            edges: HashMap::new(),
            staged: HashSet::new(),
            absent: HashSet::new(),
            next: vec![first],
            wanted: HashMap::new(),
            whole: Vec::new(),
            claimed: HashSet::new(),
            queue: VecDeque::new(),
            open: VecDeque::new(),
            children_queued: true,
            report: FetchReport::default(),
        }
    }

    /// The next request to send, or `None` while answers are awaited or
    /// once the walk is done.
    pub fn request(&mut self) -> Result<Option<Request>> {
        self.advance()?;
        let Some(r) = self.queue.pop_front() else {
            return Ok(None);
        };
        self.open.push_back(match &r {
            Request::Get(ids) => Open::Get(ids.clone()),
            Request::GetChildren(_) => {
                let n = self.whole.len().min(MAX_GET);
                Open::Children(self.whole.drain(..n).collect())
            }
            Request::Fetch { .. } => Open::Fetch,
        });
        Ok(Some(r))
    }

    /// The answer to the oldest open request: one result per id, in request
    /// order, as `Session` returns them. Fails with the first failure in it,
    /// after staging what arrived intact.
    pub fn answer(&mut self, results: Vec<Result<Vec<u8>>>) -> Result<()> {
        self.report.rounds += 1;
        match self.open.pop_front() {
            Some(Open::Get(ids)) => self.got(&ids, results),
            Some(Open::Children(nodes)) => self.got_children(&nodes, results),
            Some(Open::Fetch) => invalid("a Fetch is answered by what was pushed"),
            None => invalid("an answer to no request"),
        }
    }

    /// The answer to the oldest open request, a Fetch: stages every pushed
    /// object whose bytes match its id, to be validated when it is walked.
    /// An object under another id is `Corrupt`.
    pub fn pushed(&mut self, objects: Vec<(Hash, Vec<u8>)>) -> Result<()> {
        if !matches!(self.open.pop_front(), Some(Open::Fetch)) {
            return invalid("pushed objects answer no Fetch");
        }
        self.report.rounds += 1;
        for (id, bytes) in objects {
            if h(&bytes) != id {
                return Err(Error::Corrupt(id));
            }
            self.staging.put_known(&id, &bytes)?;
            self.report.pushed += 1;
            self.report.bytes += bytes.len() as u64;
        }
        Ok(())
    }

    /// Whether every request has been answered and nothing is left to walk.
    pub fn is_done(&self) -> bool {
        self.queue.is_empty()
            && self.open.is_empty()
            && self.children_queued
            && self.whole.is_empty()
            && self.next.is_empty()
    }

    /// Moves the walked, staged objects into the store, every object after
    /// its children (a post-order over the walked graph), then syncs the
    /// store and clears staging. `Invalid` before a whole walk is done; a
    /// partial walk may stop at any time.
    pub fn commit(mut self) -> Result<FetchReport> {
        if self.filter.is_none() && !self.is_done() {
            return invalid("the walk is not done");
        }
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        let mut stack = vec![(self.root, false)];
        while let Some((id, children_done)) = stack.pop() {
            if children_done {
                if self.staged.contains(&id) {
                    order.push(id);
                }
                continue;
            }
            if !visited.insert(id) {
                continue;
            }
            stack.push((id, true));
            let children = self.edges.get(&id).into_iter().flatten();
            stack.extend(
                children
                    .filter(|c| !visited.contains(*c))
                    .map(|c| (*c, false)),
            );
        }
        for id in &order {
            self.store.put_known(id, &self.staging.get(id)?)?;
        }
        self.report.committed = order.len();
        self.store.sync()?;
        self.staging.clear()?;
        Ok(self.report)
    }

    /// Queues what comes next once every open request is answered: this
    /// layer's GetChildren, then the next layer.
    fn advance(&mut self) -> Result<()> {
        while self.queue.is_empty() && self.open.is_empty() {
            if !self.children_queued {
                self.claimed.clear();
                let batches = self.whole.chunks(MAX_GET);
                let ids = batches.map(|b| Request::GetChildren(b.iter().map(|w| w.0).collect()));
                self.queue.extend(ids.collect::<Vec<_>>());
                self.children_queued = true;
            } else if self.next.is_empty() {
                return Ok(());
            } else {
                let todo = std::mem::take(&mut self.next);
                self.layer(todo)?;
                self.children_queued = false;
            }
        }
        Ok(())
    }

    /// Starts a layer: walks what is staged and queues Gets for what is
    /// missing. A whole walk skips what is stored with its closure; a
    /// partial one walks stored objects, whose children may be absent.
    fn layer(&mut self, todo: Vec<Item>) -> Result<()> {
        self.wanted.clear();
        let mut request = Vec::new();
        for (id, kind, path) in todo {
            if self.filter.is_none() && !self.needed(&id, kind)? || self.absent.contains(&id) {
                continue;
            }
            if self.staging.has(&id)? {
                self.walk(id, kind, &path, &self.staging.get(&id)?)?;
                self.staged.insert(id);
            } else if self.store.has(&id)? {
                // Partial, or a stored record wanted with content that is
                // not complete.
                self.walk(id, kind, &path, &self.store.get(&id)?)?;
            } else {
                let kinds = self.wanted.entry(id).or_default();
                if kinds.is_empty() {
                    request.push(id);
                }
                kinds.push((kind, path));
            }
        }
        let gets = request.chunks(MAX_GET).map(|b| Request::Get(b.to_vec()));
        self.queue.extend(gets);
        Ok(())
    }

    /// Whether the store lacks the object, or, for a record wanted with
    /// content, the record is not complete (§4.1).
    fn needed(&self, id: &Hash, kind: Kind) -> Result<bool> {
        match kind {
            Kind::Record {
                content: Some(_),
                deep,
                ..
            } => Ok(!self.complete(id, deep)?),
            _ => Ok(!self.store.has(id)?),
        }
    }

    /// Whether the record and its content are stored, with a merge's
    /// leaves, or with `deep` every record it descends from (§4.1).
    fn complete(&self, id: &Hash, deep: bool) -> Result<bool> {
        let (mut todo, mut seen) = (vec![*id], HashSet::from([*id]));
        while let Some(x) = todo.pop() {
            if !self.store.has(&x)? {
                return Ok(false);
            }
            let r = load::record(self.store, &x)?;
            if !r.is_tombstone() && !self.store.has(&r.root)? {
                return Ok(false);
            }
            // A stored merge's parents are stored records, never merges.
            if deep || (x == *id && r.is_merge()) {
                todo.extend(r.parents.iter().filter(|p| seen.insert(**p)));
            }
        }
        Ok(true)
    }

    /// Checks, validates, walks and stages every object of a Get answer
    /// that arrived, then fails with the first failure, if any. A partial
    /// walk leaves what the server lacks absent.
    fn got(&mut self, batch: &[Hash], results: Vec<Result<Vec<u8>>>) -> Result<()> {
        self.report.requested += batch.len();
        answers(batch.len(), results.len())?;
        let mut failure = None;
        for (id, r) in batch.iter().zip(results) {
            let bytes = match r {
                Ok(b) => b,
                Err(Error::NotFound(_)) if self.filter.is_some() => {
                    self.absent.insert(*id);
                    self.report.absent += 1;
                    continue;
                }
                Err(e) => {
                    failure.get_or_insert(e);
                    continue;
                }
            };
            if h(&bytes) != *id {
                return Err(Error::Corrupt(*id));
            }
            let kinds = self.wanted.remove(id).unwrap_or_default();
            for (kind, path) in kinds {
                self.walk(*id, kind, &path, &bytes)?;
            }
            self.staging.put_known(id, &bytes)?;
            self.staged.insert(*id);
            self.report.bytes += bytes.len() as u64;
        }
        failure.map_or(Ok(()), Err)
    }

    /// Stages each node's chunks of a Children answer once all of them
    /// check out. A node the server lacks is left to Get.
    fn got_children(
        &mut self,
        batch: &[(Hash, Node, Data)],
        results: Vec<Result<Vec<u8>>>,
    ) -> Result<()> {
        answers(batch.len(), results.len())?;
        let mut failure = None;
        for ((id, node, data), r) in batch.iter().zip(results) {
            let payload = match r {
                Ok(p) => p,
                Err(Error::NotFound(_)) => continue,
                Err(e) => {
                    failure.get_or_insert(e);
                    continue;
                }
            };
            let chunks = split_children(id, node, &payload)?;
            for (c, chunk) in node.children.iter().zip(&chunks) {
                let kind = Kind::Chunk {
                    data: *data,
                    len: c.bytes,
                };
                children(self.profile, &c.id, kind, chunk)?;
            }
            for (c, chunk) in node.children.iter().zip(chunks) {
                self.staging.put_known(&c.id, chunk)?;
                self.staged.insert(c.id); // committed even if not yet walked
            }
            self.report.nodes += 1;
            self.report.bytes += payload.len() as u64;
        }
        failure.map_or(Ok(()), Err)
    }

    /// Validates an object of `kind` at `path` and queues its children,
    /// typed by context (§4.3); a level-1 node may be whole.
    fn walk(&mut self, id: Hash, kind: Kind, path: &str, bytes: &[u8]) -> Result<()> {
        let children = children(self.profile, &id, kind, bytes)?;
        if let Kind::Node { data, .. } = kind {
            let n = Node::decode(bytes)?;
            if self.is_whole(&n)? {
                self.claimed.extend(n.children.iter().map(|c| c.id));
                self.whole.push((id, n, data));
            }
        }
        let edges = self.edges.entry(id).or_default();
        for Child { id, kind, label } in children {
            let path = match (self.filter, label) {
                (Some(accepts), Some(label)) => {
                    let path = join(path, &label);
                    if !accepts(&path) {
                        continue;
                    }
                    path
                }
                (Some(_), None) => path.to_string(),
                (None, _) => String::new(),
            };
            edges.push(id);
            if self.seen.insert((id, kind, path.clone())) {
                self.next.push((id, kind, path));
            }
        }
        Ok(())
    }

    /// A level-1 node with chunks, distinct, none stored, staged or claimed
    /// by another whole node of this layer.
    fn is_whole(&self, n: &Node) -> Result<bool> {
        let ids: HashSet<Hash> = n.children.iter().map(|c| c.id).collect();
        if n.level != 1 || ids.is_empty() || ids.len() != n.children.len() {
            return Ok(false);
        }
        for id in &ids {
            if self.claimed.contains(id) || self.store.has(id)? || self.staging.has(id)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn answers(asked: usize, got: usize) -> Result<()> {
    match asked == got {
        true => Ok(()),
        false => Err(Error::Protocol(format!("{got} answers to {asked} ids"))),
    }
}

#[cfg(test)]
mod tests;
