//! The protocol of §4.2 without I/O: frames, and both ends of a session as
//! state machines that take received bytes and give bytes to send. Drivers
//! move the bytes (the blocking ones in `transport`); every driver shares
//! the same format and logic.

mod client;
pub(crate) mod frame;
mod grants;
mod live;
mod push;
mod responder;

pub use client::{Client, Event};
pub use frame::Greeting;
pub use responder::Responder;

use crate::auth::PeerKey;
use crate::error::{Error, Result};
use crate::format::FORMAT;
use crate::hash::Hash;
use crate::iface::Profile;
use crate::iface::{LiveEvents, MAX_FRAME, ObjectStore};
use crate::load::children;
use crate::log;
use std::sync::Arc;

/// The highest protocol version this implementation speaks (§4.2).
pub const PROTOCOL: u8 = 8;

/// The lowest protocol version this implementation speaks.
pub(crate) const LOWEST: u8 = 8;

/// The optional features this implementation supports: none defined yet.
pub(crate) const FEATURES: u64 = 0;

impl Greeting {
    /// This implementation's Hello for `root`, of folder format `format`.
    pub fn new(root: Hash, format: u16, challenge: [u8; 32]) -> Greeting {
        Greeting {
            low: LOWEST,
            high: PROTOCOL,
            features: FEATURES,
            format,
            root,
            challenge,
        }
    }
}

/// The protocol version and features of a session between `mine` and
/// `theirs` (§4.2): the highest version both speak and the features both
/// set; `Incompatible` if they share no version or their folder formats
/// differ.
pub fn agree(mine: &Greeting, theirs: &Greeting) -> Result<(u8, u64)> {
    let range = |g: &Greeting| match g.low == g.high {
        true => format!("{}", g.high),
        false => format!("{} to {}", g.low, g.high),
    };
    if mine.low.max(theirs.low) > mine.high.min(theirs.high) {
        let who = if mine.high < theirs.low {
            "this device"
        } else {
            "the peer"
        };
        return Err(Error::Incompatible(format!(
            "this device speaks protocol {}, the peer {}: update {who}",
            range(mine),
            range(theirs)
        )));
    }
    if mine.format != theirs.format {
        return Err(Error::Incompatible(format!(
            "the peer's folder is format {}, this device's {}",
            theirs.format, mine.format
        )));
    }
    Ok((mine.high.min(theirs.high), mine.features & theirs.features))
}

/// What a server offers: any object of its store, and the current root.
#[derive(Clone)]
pub struct Served {
    pub store: Arc<dyn ObjectStore>,
    pub root: Arc<dyn Fn() -> Hash + Send + Sync>,
    /// The protocol versions said in Hello, lowest and highest; others than
    /// `LOWEST` and `PROTOCOL` only in tests.
    pub protocols: (u8, u8),
    /// The folder format of the root, said in Hello (§5.4).
    pub format: u16,
    /// Set where the transport does not identify peers: each session is then
    /// challenged for a key (§4.2).
    pub admission: Option<Arc<Admission>>,
    /// The profile of the root, whose links a Fetch and read rights follow;
    /// without it, a Fetch pushes nothing and read rights grant only the root.
    pub profile: Option<Arc<dyn Profile>>,
    /// Set to serve each peer only what it may read (§4.2); unset, a server
    /// serves any object of its store.
    pub reads: Option<Arc<ReadRule>>,
    /// Whether a path is within a Fetch's scope; unset, or for an empty
    /// scope, every path is.
    pub scope: Option<Arc<ScopeRule>>,
    /// Where the application takes the events of live sessions (§4.2);
    /// unset, every live stream a client opens is refused.
    pub live: Option<LiveEvents>,
}

/// Whom a challenging server serves.
pub struct Admission {
    /// The names clients may know this server by, as they sign them.
    pub names: Vec<String>,
    /// Whether the holder of a key is served.
    pub admits: Box<dyn Fn(&PeerKey) -> bool + Send + Sync>,
}

/// Whether a peer may read the object at a path (§4.2). The peer is `None`
/// when neither the transport nor a challenge identified it.
pub type ReadRule = dyn Fn(Option<&PeerKey>, &str) -> bool + Send + Sync;

/// Whether a path is within a Fetch's scope, an application's bytes (§4.3).
pub type ScopeRule = dyn Fn(&[u8], &str) -> bool + Send + Sync;

impl Served {
    pub fn new(
        store: Arc<dyn ObjectStore>,
        root: impl Fn() -> Hash + Send + Sync + 'static,
    ) -> Self {
        Served {
            store,
            root: Arc::new(root),
            protocols: (LOWEST, PROTOCOL),
            format: FORMAT,
            admission: None,
            profile: None,
            reads: None,
            scope: None,
            live: None,
        }
    }

    pub fn root(&self) -> Hash {
        (self.root)()
    }

    /// The answer to one id of a Get: the bytes, or `None` for Missing. An
    /// object the server cannot read (corrupt, I/O) is missing too.
    pub fn object(&self, id: &Hash) -> Option<Vec<u8>> {
        fits(id, self.store.get(id))
    }

    /// The answer to one id of a GetChildren: the chunks of a held, valid
    /// level-1 node whose chunks are all held, or `None` for Missing.
    pub fn children(&self, id: &Hash) -> Option<Vec<u8>> {
        fits(
            id,
            children(&*self.store, id).and_then(|c| c.ok_or(Error::NotFound(*id))),
        )
    }
}

/// The bytes, if they fit one answer frame; failures are logged.
fn fits(id: &Hash, r: crate::Result<Vec<u8>>) -> Option<Vec<u8>> {
    match r {
        Ok(b) if 1 + 32 + b.len() <= MAX_FRAME => Some(b),
        Ok(b) => {
            log!("serving {id}: {} bytes do not fit a frame", b.len());
            None
        }
        Err(Error::NotFound(_)) => None,
        Err(e) => {
            log!("serving {id}: {e}");
            None
        }
    }
}

/// A server of exactly `objects` at `root`, speaking only `protocol`.
#[cfg(test)]
pub(crate) fn sample_served(objects: &[Vec<u8>], root: Hash, protocol: u8) -> Served {
    let store = crate::store::MemStore::new();
    for o in objects {
        store.put(o).expect("a memory store accepts every object");
    }
    Served {
        protocols: (protocol, protocol),
        profile: Some(Arc::new(crate::walk::Versions)),
        live: Some(crate::conformance::echo()),
        ..Served::new(Arc::new(store), move || root)
    }
}
