//! The interfaces engines use. Implementations depend only on the format;
//! every trait is blocking and standard-library only. Errors follow §4.4.

use crate::auth::PeerKey;
use crate::error::{Error, Result};
use crate::format::manifest::{Kind, MAX_MANIFEST_CHUNK};
use crate::format::record::Record;
use crate::format::{K, W};
use crate::hash::Hash;
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::mpsc::SyncSender;

/// Content-addressed storage (§4.1). Objects carry no type (A8): a store never
/// parses them; it only checks that `H(bytes)` is the id.
pub trait ObjectStore: Send + Sync {
    /// Whether the object is stored. `Io` if the store cannot be read.
    fn has(&self, id: &Hash) -> Result<bool>;
    /// The object's bytes. `NotFound` if absent; `Corrupt` if the stored bytes
    /// do not hash to `id`; `Io` if the store cannot be read.
    fn get(&self, id: &Hash) -> Result<Vec<u8>>;
    /// Stores `bytes` and returns `H(bytes)`. Storing the same bytes again
    /// changes nothing. `Invalid` for a read-only store.
    fn put(&self, bytes: &[u8]) -> Result<Hash>;
    /// Stores bytes received under `id`. `Corrupt(id)` if `H(bytes)` differs,
    /// and then nothing is stored.
    fn put_known(&self, id: &Hash, bytes: &[u8]) -> Result<()>;
    /// Removes an object (garbage collection); removing an absent one is not an
    /// error. `Invalid` for a read-only store.
    fn remove(&self, id: &Hash) -> Result<()>;
    /// Every stored id, once each, in no particular order.
    fn ids(&self) -> Result<Vec<Hash>>;
    /// Makes everything stored so far durable; a profile calls it before it
    /// writes a root (§4.1).
    fn sync(&self) -> Result<()> {
        Ok(())
    }
    /// Reclaims the space of removed objects; changes no content.
    fn compact(&self) -> Result<()> {
        Ok(())
    }
    /// Removes every object: staging is cleared so once a pull is committed
    /// (§4.3). The store stays usable.
    fn clear(&self) -> Result<()> {
        self.ids()?.iter().try_for_each(|id| self.remove(id))
    }
}

/// The most ids in one Get or GetChildren, and haves in one Fetch (§4.2).
pub const MAX_GET: usize = 4096;
/// The most object bytes a server pushes for one Fetch (§4.3).
pub const MAX_PUSH: u64 = 16 << 20;
/// The largest frame, its type byte and payload counted: 2^(2k+2) bytes,
/// 4 MiB at k = 10 (§4.2).
pub const MAX_FRAME: usize = 1 << (2 * K + 2);
// The largest answer, a Children of W manifest chunks, is 394,785 bytes and
// fits one frame (§4.3).
const _: () = assert!(1 + 32 + W * MAX_MANIFEST_CHUNK == 394_785);
const _: () = assert!(1 + 32 + W * MAX_MANIFEST_CHUNK <= MAX_FRAME);

/// Opens sessions to the user's other devices (§4.2).
pub trait Transport: Send + Sync {
    /// A new session for one pull. `Unavailable` if the peer cannot be reached;
    /// `Invalid` for a malformed address.
    fn connect(&self, peer: &str) -> Result<Box<dyn Session>>;
    /// A new live session (§4.2), whose events arrive on `events`, each with
    /// the session's handle. `Unavailable` if the peer cannot be reached;
    /// `Invalid` for a malformed address; `Incompatible` if the peer shares
    /// no protocol version or folder format.
    fn live(&self, peer: &str, events: LiveEvents) -> Result<Arc<dyn Live>>;
}

/// One end of a live session (§4.2). Calls never wait: each queues a frame,
/// and one that would queue more than `LIVE_QUEUE` bytes ends the session
/// with `Unavailable`. After the end, every call fails as the session ended.
/// A call that breaks a stream's state is `Protocol`.
pub trait Live: Send + Sync {
    /// Opens `name` at `version` (client), or accepts it (server).
    fn open(&self, name: &str, version: &[u8]) -> Result<()>;
    /// Sends an update on the open stream `name`.
    fn update(&self, name: &str, bytes: &[u8]) -> Result<()>;
    /// Closes the open stream `name`, or refuses it (server).
    fn close(&self, name: &str) -> Result<()>;
    /// Ends the session once what is queued has been sent.
    fn bye(&self) -> Result<()>;
    /// The peer's key, where known: at a server, the client's.
    fn peer(&self) -> Option<PeerKey>;
    /// The bytes queued and not yet taken for sending, at most `LIVE_QUEUE`.
    fn queued(&self) -> usize;
}

/// What a live session received, or its end.
#[derive(Clone, Debug)]
pub enum LiveEvent {
    /// At the server, the client opens `name`; at the client, the server
    /// accepted it. `version` is the sender's, as the application wrote it.
    Open { name: String, version: Vec<u8> },
    /// An update on an open stream.
    Update { name: String, bytes: Vec<u8> },
    /// The stream is closed: by the peer, by a refusal, or as the answer to
    /// this side's close.
    Close { name: String },
    /// The session is over, and why; always its last event.
    Ended(Error),
}

/// Where an application takes the events of its live sessions; bounded, so a
/// slow application stops the sessions' readers.
pub type LiveEvents = SyncSender<(Arc<dyn Live>, LiveEvent)>;

/// The most bytes of frames a live session queues unsent (§4.2).
pub const LIVE_QUEUE: usize = 4 << 20;

/// One pull's conversation with a server (§4.2). Any call fails with
/// `Unavailable` if the connection breaks or a read waits too long, and with
/// `Protocol` for a malformed frame or an unexpected message.
pub trait Session: Send {
    /// Exchanges Hello: sends `root`, returns the server's root. If the
    /// sides share no protocol version or folder format (§4.2), sends Bye and
    /// fails with `Incompatible`.
    fn hello(&mut self, root: &Hash) -> Result<Hash>;
    /// One result per id, in request order: the bytes as sent (the caller
    /// checks them), or `NotFound(id)` for Missing. If the connection breaks
    /// during the answer, the results received are kept and every later id
    /// carries `Unavailable`. More than `MAX_GET` ids is `Invalid`; none sends
    /// nothing.
    fn get(&mut self, ids: &[Hash]) -> Result<Vec<Result<Vec<u8>>>>;
    /// GetChildren of level-1 nodes: one result per node, in request order,
    /// the node's chunks concatenated in node order as sent (the caller splits
    /// and checks them), or `NotFound(node)` for Missing. Otherwise as `get`.
    fn get_children(&mut self, nodes: &[Hash]) -> Result<Vec<Result<Vec<u8>>>>;
    /// Fetch: the objects the server pushes of what `want` reaches and the
    /// `have`s do not, within `scope`, each with the id it was sent under
    /// (the caller checks them); possibly none. If the connection breaks
    /// during the answer, the objects received are kept. More than `MAX_GET`
    /// haves is `Invalid`.
    fn fetch(&mut self, want: &Hash, have: &[Hash], scope: &[u8]) -> Result<Vec<(Hash, Vec<u8>)>>;
    /// Sends Bye and closes the session.
    fn bye(&mut self) -> Result<()>;
}

/// What the folder profile keeps of a path on disk (§5.2, §5.4). `size` is the
/// length of the content: a symbolic link's target, 0 for a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    pub size: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime: i64,
    pub inode: u64,
}

/// The user's folder. Paths are manifest paths (§5.1, §5.3): an empty
/// directory's ends in `/`. A workspace never follows a symbolic link, neither
/// as the last component nor as a parent. Every call fails with `Invalid` for
/// an invalid path and with `Io` on a file-system failure.
pub trait Workspace: Send {
    /// Every regular file, executable file, symbolic link and empty directory,
    /// skipping reserved names, special files and invalid names (§5.5 step 1).
    fn list(&self) -> Result<Vec<(String, Meta)>>;
    /// One path; `None` if nothing listable is there (a directory that holds
    /// something, a special file, nothing).
    fn stat(&self, path: &str) -> Result<Option<Meta>>;
    /// The content: a file's bytes, a link's target, nothing for a directory.
    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>>;
    /// Atomically replaces `path` with `content` of `kind` (§5.6 step 3):
    /// readers see the old or the new content, and no `*.kide-tmp` remains.
    /// Creates parent directories; an empty directory at `path` gives way.
    /// A path ends in `/` exactly for `Directory`, whose content is empty;
    /// anything else is `Invalid`. Sets the mtime of a regular file (a link's
    /// and a directory's are not synchronised, §5.1) and returns the metadata
    /// observed after.
    fn write(&self, path: &str, kind: Kind, content: &mut dyn Read, mtime: i64) -> Result<Meta>;
    /// `write`, only if what is at `path` is still `expect` (`None`:
    /// nothing) when it is replaced; otherwise nothing is written and the
    /// result is `None`. A replaced file or link is kept in the trash
    /// (§5.6, applying step 3). This default checks before writing; a
    /// workspace that can should check just before it replaces.
    fn write_if(
        &self,
        path: &str,
        kind: Kind,
        content: &mut dyn Read,
        mtime: i64,
        expect: Option<Meta>,
    ) -> Result<Option<Meta>> {
        if self.stat(path)? != expect {
            return Ok(None);
        }
        self.write(path, kind, content, mtime).map(Some)
    }
    /// Sets a file's mtime and returns the metadata observed after.
    fn set_mtime(&self, path: &str, mtime: i64) -> Result<Meta>;
    /// Moves a file, link or whole directory tree, creating target parents
    /// and removing source parents that became empty. Directory operands both
    /// end in `/`; refuse an existing target or overlapping operands.
    fn rename(&self, from: &str, to: &str) -> Result<()>;
    /// Moves a file or link to `.kide/trash/<unix-time>/<path>` (or
    /// `<unix-time>-<n>`), or removes an empty directory, then removes parents
    /// that became empty (§5.6 step 4).
    fn trash(&self, path: &str) -> Result<()>;
}

/// What the index recorded of a path (§5.4): its metadata and the time it
/// was checked. An entry checked less than 2 s after its mtime is racy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checked {
    pub meta: Meta,
    pub at: i64,
}

/// An entry checked less than this after its mtime is racy (§5.4).
pub(crate) const RACY_NS: i64 = 2_000_000_000;

impl Checked {
    /// Whether a same-size edit could have kept the mtime: the entry was
    /// checked less than 2 s after it (or before it). Never a cache key.
    pub fn is_racy(&self) -> bool {
        self.at.saturating_sub(self.meta.mtime) < RACY_NS
    }
}

/// Every path's index entry, as a change source reads it.
pub type CheckedIndex = BTreeMap<String, Checked>;

/// Reports the paths that may have changed since the index was written.
pub trait ChangeSource: Send {
    /// Every path created, edited, touched, retyped, renamed or removed since
    /// `index` was recorded appears at least once, as does every racy entry
    /// (§5.4). Unchanged paths may appear too.
    fn changed(&mut self, ws: &dyn Workspace, index: &CheckedIndex) -> Result<Vec<String>>;
}

/// One side of a differing path (§5.6): its version record and the mtime of
/// its manifest entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Side {
    pub id: Hash,
    pub record: Record,
    pub mtime: i64,
}

/// A conflict copy to create (§5.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictCopy {
    pub path: String,
    pub record: Record,
    pub mtime: i64,
}

/// The record a path ends with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The local record.
    Keep,
    /// An existing record: the remote one.
    Adopt(Hash),
    /// A new merge record.
    Merge(Record),
}

/// The decision for one differing path (§5.6). Name clashes are found by the
/// walk over both manifests, not by a resolver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub outcome: Outcome,
    pub mtime: i64,
    pub copies: Vec<ConflictCopy>,
    /// The content of a text merge's value (§5.6), for the caller to store
    /// before the merge record.
    pub content: Option<Vec<u8>>,
}

/// Reads version records, one per id in order; one call per generation of
/// the ancestor search.
pub type History<'a> = dyn FnMut(&[Hash]) -> Result<Vec<Record>> + 'a;

/// Reads the whole content below a root if it is at most `max` bytes, else
/// `None`, without reading it: a text merge reads its base and its leaves
/// (§5.6).
pub type ReadContent<'a> = dyn FnMut(&Hash, u64) -> Result<Option<Vec<u8>>> + 'a;

/// Decides one differing path (§5.6).
pub trait Resolver: Send + Sync {
    /// Applies the rule to the local and remote sides of `path`, at least one
    /// present. Rows 1 to 3 read no history; errors of `history` and
    /// `content` pass through unchanged. For the everyday rule
    /// (`DecisionTable`), the resulting record, its content and mtime depend
    /// only on the pair, not on which side is local, so devices converge;
    /// `Join` and `Reset` favour the remote side on purpose (§5.6).
    fn decide(
        &self,
        path: &str,
        local: Option<&Side>,
        remote: Option<&Side>,
        history: &mut History,
        content: &mut ReadContent,
    ) -> Result<Decision>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_limit_is_4_mib() {
        assert_eq!(MAX_FRAME, 4 << 20);
    }

    // Compiles only if every trait can be used as a trait object.
    #[allow(dead_code)]
    struct Objects(
        Box<dyn ObjectStore>,
        Box<dyn Transport>,
        Box<dyn Session>,
        Box<dyn Live>,
        Box<dyn Workspace>,
        Box<dyn ChangeSource>,
        Box<dyn Resolver>,
    );
}

// Profiles (§4.1, §4.3): what an application's tree links to.

/// What a profile tells walks about its own data.
pub trait Profile: Send + Sync {
    /// How the root is wanted.
    fn root(&self) -> Want;
    /// Validates one of the profile's own objects on its own and returns
    /// its links.
    fn children(&self, bytes: &[u8]) -> Result<Vec<Link>>;
}

/// How a profile wants an object it links to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Want {
    /// One of the profile's own objects, read by `Profile::children`.
    Object,
    /// A content node whose chunks are the profile's own objects.
    Objects,
    /// A content node of a version's content; its chunks link to nothing.
    Content,
    /// A version record, as history only or with its content (§4.3).
    Record(Content),
}

/// How much of a version record a walk wants, and what its content is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Content {
    /// The record and its history, without content.
    History,
    /// With its content, whose chunks link to nothing (`Want::Content`).
    Bytes,
    /// With its content, whose chunks are the profile's own objects
    /// (`Want::Objects`): a checkpoint of the profile's data.
    Objects,
    /// With its content, and every record it descends from with its content
    /// too: a whole history that can be read back, as a note's merge base
    /// is (§5.6).
    Full,
}

/// One link of a profile object, with the path segment it is reached by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub id: Hash,
    pub want: Want,
    pub label: Option<String>,
}
