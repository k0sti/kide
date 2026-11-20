//! Pulling from the browser (§4.3): the fetch walk of `kide::fetch`, driven
//! over a WebTransport session, whole or partial (§4.1). The walk's requests of a layer go out
//! together, so they are pipelined; what arrives is staged in memory and
//! moves into the store once the walk is done.

use crate::session;
use kide::auth::Secret;
use kide::error::{Error, Result};
use kide::fetch::{FetchReport, Request, Walk};
use kide::hash::Hash;
use kide::iface::ObjectStore;
use kide::iface::Profile;
use kide::protocol::Event;
use std::sync::Arc;

/// Fetches the closure of the root of the server at `url` (trusting its
/// certificate and answering a challenge with `key` as `session::connect`
/// does) into `store`, wanted as `profile` says: first a Fetch of what the
/// roots in `have`, whose closures are stored, do not reach, then whatever
/// the push left out. Returns the root.
pub async fn pull(
    url: &str,
    cert_hash: Option<&[u8]>,
    key: Option<Arc<Secret>>,
    store: &dyn ObjectStore,
    profile: &dyn Profile,
    have: &[Hash],
) -> Result<(Hash, FetchReport)> {
    let from = Start {
        at: None,
        have,
        part: None,
    };
    pull_from(url, cert_hash, key, store, profile, from).await
}

/// The part of a tree a partial pull wants: the paths `filter` accepts, and
/// the `scope` bytes the server's scope rule reads (§4.3), which should
/// describe the same paths so that the push carries what the walk wants.
pub struct Part<'a> {
    pub filter: &'a dyn Fn(&str) -> bool,
    pub scope: Vec<u8>,
}

/// As `pull`, only into the part of the tree `part` names (`Walk::partial`):
/// an object the server lacks below the root is left absent, so a client may
/// hold the part of a large tree it looks at.
pub async fn pull_partial(
    url: &str,
    cert_hash: Option<&[u8]>,
    key: Option<Arc<Secret>>,
    store: &dyn ObjectStore,
    profile: &dyn Profile,
    have: &[Hash],
    part: Part<'_>,
) -> Result<(Hash, FetchReport)> {
    let from = Start {
        at: None,
        have,
        part: Some(part),
    };
    pull_from(url, cert_hash, key, store, profile, from).await
}

/// As `pull`, from `at` instead of the server's root: for an object the
/// server holds below it, such as an older version and its content, which
/// the profile does not want from the root.
pub async fn pull_at(
    url: &str,
    cert_hash: Option<&[u8]>,
    key: Option<Arc<Secret>>,
    store: &dyn ObjectStore,
    profile: &dyn Profile,
    at: &Hash,
) -> Result<FetchReport> {
    let from = Start {
        at: Some(*at),
        have: &[],
        part: None,
    };
    let pulled = pull_from(url, cert_hash, key, store, profile, from).await;
    Ok(pulled?.1)
}

/// Where a pull starts: `at`, or else the server's root, with what the
/// client has and the part it wants.
struct Start<'a> {
    at: Option<Hash>,
    have: &'a [Hash],
    part: Option<Part<'a>>,
}

async fn pull_from(
    url: &str,
    cert_hash: Option<&[u8]>,
    key: Option<Arc<Secret>>,
    store: &dyn ObjectStore,
    profile: &dyn Profile,
    from: Start<'_>,
) -> Result<(Hash, FetchReport)> {
    let (mut s, root) = session::connect(url, cert_hash, key).await?;
    let root = from.at.unwrap_or(root);
    let staging = kide::store::MemStore::new();
    let have = from.have.to_vec();
    let mut w = match from.part {
        None => Walk::new(store, &staging, profile, &root).with_fetch(have, Vec::new()),
        Some(p) => {
            Walk::partial(store, &staging, profile, &root, p.filter).with_fetch(have, p.scope)
        }
    };
    let walked = walk(&mut s, &mut w).await;
    s.bye().await;
    walked?;
    Ok((root, w.commit()?))
}

/// Sends every request the walk hands out and answers them in order, until
/// it has none left.
async fn walk(s: &mut session::Stream, w: &mut Walk<'_>) -> Result<()> {
    let mut open = 0;
    loop {
        while let Some(r) = w.request()? {
            match &r {
                Request::Get(ids) => s.client.request(ids, false)?,
                Request::GetChildren(nodes) => s.client.request(nodes, true)?,
                Request::Fetch { want, have, scope } => s.client.fetch(want, have, scope)?,
            }
            open += 1;
        }
        s.flush().await?;
        if open == 0 {
            return Ok(());
        }
        match s.event().await? {
            Event::Answer(a) => w.answer(a)?,
            Event::Pushed(p) => w.pushed(p)?,
            _ => return Err(Error::Protocol("a message out of turn in a pull".into())),
        }
        open -= 1;
    }
}
