//! What lies below a profile root (§4.1, §4.3): the core knows content
//! nodes, their chunks and version records; a profile knows its own
//! objects and what they link to. Every walk (fetching, garbage
//! collection) asks `children` for the objects one level down.

use crate::error::{Result, invalid};
use crate::format::chunk::check_chunk;
use crate::format::node::Node;
use crate::format::record::Record;
use crate::hash::Hash;
use crate::iface::{Content, Link, Profile, Want};

/// Whose content a version record's content is, as `Content` says.
fn data(c: Content) -> Option<Data> {
    match c {
        Content::History => None,
        Content::Bytes | Content::Full => Some(Data::Version),
        Content::Objects => Some(Data::Profile),
    }
}

/// Whose content a node or chunk is: the profile's, or a version's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Data {
    Profile,
    Version,
}

/// An object's kind, known from its referrer (A8), with what the referrer
/// says about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Object,
    /// A node; below another node, its level and content length are known.
    Node {
        data: Data,
        expect: Option<(u8, u64)>,
    },
    Chunk {
        data: Data,
        len: u64,
    },
    /// `content`: the data its content is, if wanted with content, else
    /// `None` for history. `below_merge`: a parent of a merge record, which
    /// may not be a merge itself. `deep`: its parents are wanted with their
    /// content too, all the way down (`Content::Full`).
    Record {
        content: Option<Data>,
        below_merge: bool,
        deep: bool,
    },
}

impl From<Want> for Kind {
    fn from(w: Want) -> Kind {
        let node = |data| Kind::Node { data, expect: None };
        match w {
            Want::Object => Kind::Object,
            Want::Objects => node(Data::Profile),
            Want::Content => node(Data::Version),
            Want::Record(c) => Kind::Record {
                content: data(c),
                below_merge: false,
                deep: c == Content::Full,
            },
        }
    }
}

impl Kind {
    /// A chunk of a version's content: it links to nothing, so a walk that
    /// only marks need not read it.
    pub(crate) fn is_leaf(self) -> bool {
        matches!(
            self,
            Kind::Chunk {
                data: Data::Version,
                ..
            }
        )
    }
}

/// A child typed for the walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Child {
    pub id: Hash,
    pub kind: Kind,
    pub label: Option<String>,
}

impl Child {
    fn new(id: Hash, kind: Kind) -> Child {
        Child {
            id,
            kind,
            label: None,
        }
    }
}

/// The path of a link labelled `label` below `parent` (§4.1).
pub(crate) fn join(parent: &str, label: &str) -> String {
    match parent.is_empty() {
        true => label.to_string(),
        false => format!("{parent}/{label}"),
    }
}

/// Validates the object `id` of `kind` and returns its children, typed by
/// context (§4.3).
pub(crate) fn children(
    profile: &dyn Profile,
    id: &Hash,
    kind: Kind,
    bytes: &[u8],
) -> Result<Vec<Child>> {
    match kind {
        Kind::Object => links(profile, bytes),
        Kind::Node { data, expect } => node(id, data, expect, bytes),
        Kind::Chunk { data, len } => {
            if bytes.len() as u64 != len {
                return invalid(format!("chunk {id} of {} bytes, not {len}", bytes.len()));
            }
            match data {
                Data::Profile => links(profile, bytes),
                Data::Version => check_chunk(bytes).map(|()| Vec::new()),
            }
        }
        Kind::Record {
            content,
            below_merge,
            deep,
        } => record(id, content, below_merge, deep, bytes),
    }
}

fn links(profile: &dyn Profile, bytes: &[u8]) -> Result<Vec<Child>> {
    let links = profile.children(bytes)?.into_iter();
    Ok(links
        .map(|l| Child {
            id: l.id,
            kind: l.want.into(),
            label: l.label,
        })
        .collect())
}

fn node(id: &Hash, data: Data, expect: Option<(u8, u64)>, bytes: &[u8]) -> Result<Vec<Child>> {
    let n = Node::decode(bytes)?;
    if let Some((level, len)) = expect
        && (n.level, n.bytes()) != (level, len)
    {
        return invalid(format!(
            "node {id}: level {} of {} bytes where level {level} of {len} belongs",
            n.level,
            n.bytes()
        ));
    }
    let kind = |bytes| match n.level {
        1 => Kind::Chunk { data, len: bytes },
        l => Kind::Node {
            data,
            expect: Some((l - 1, bytes)),
        },
    };
    Ok(n.children
        .iter()
        .map(|c| Child::new(c.id, kind(c.bytes)))
        .collect())
}

/// A record's children (§3.1, §4.3): its parents, as history, or, if it is a
/// merge wanted with content or a record of a whole history, with the same
/// content; its root if it is wanted with content, as the data its content
/// is.
fn record(
    id: &Hash,
    content: Option<Data>,
    below_merge: bool,
    deep: bool,
    bytes: &[u8],
) -> Result<Vec<Child>> {
    let r = Record::decode(bytes)?;
    if below_merge && r.is_merge() {
        return invalid(format!("merge record {id} is the parent of a merge record"));
    }
    let parent = Kind::Record {
        content: content.filter(|_| r.is_merge() || deep),
        below_merge: r.is_merge(),
        deep,
    };
    let mut out: Vec<_> = r.parents.iter().map(|p| Child::new(*p, parent)).collect();
    if let Some(data) = content
        && !r.is_tombstone()
    {
        let root = Kind::Node { data, expect: None };
        out.push(Child::new(r.root, root));
    }
    Ok(out)
}

/// The profile of one chain of versions: the root is a version record
/// wanted with its content, and the profile has no objects of its own. An
/// application whose data is one document's history uses it as it is.
pub struct Versions;

impl Profile for Versions {
    fn root(&self) -> Want {
        Want::Record(Content::Bytes)
    }

    fn children(&self, _: &[u8]) -> Result<Vec<Link>> {
        invalid("Versions has no objects of its own")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::Plain;
    use crate::error::Error;

    #[test]
    fn the_profiles_here_pass_the_suite() {
        use crate::conformance::profile;
        let plain = Plain::object(&[
            (Want::Object, Hash([1; 32])),
            (Want::Content, Hash([2; 32])),
        ]);
        profile(&Plain, &[plain, Plain::object(&[])]);
        profile(&Versions, &[]);
    }

    #[test]
    fn a_plain_object_links_typed_and_labelled() {
        let [a, b] = [1, 2].map(|i| Hash([i; 32]));
        let o = Plain::object(&[(Want::Object, a), (Want::Record(Content::Bytes), b)]);
        let c = children(&Plain, &crate::hash::h(&o), Kind::Object, &o).unwrap();
        let rec = Kind::Record {
            content: Some(Data::Version),
            below_merge: false,
            deep: false,
        };
        let want = [(a, Kind::Object, "0"), (b, rec, "1")];
        let got: Vec<_> = c
            .iter()
            .map(|c| (c.id, c.kind, c.label.as_deref().unwrap()))
            .collect();
        assert_eq!(got, want);
        let r = children(&Plain, &Hash::ZERO, Kind::Object, &o[1..]);
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    }

    #[test]
    fn a_chunk_has_the_length_its_node_states() {
        let k = Kind::Chunk {
            data: Data::Version,
            len: 3,
        };
        assert!(children(&Plain, &Hash::ZERO, k, b"abc").unwrap().is_empty());
        assert!(children(&Plain, &Hash::ZERO, k, b"ab").is_err());
        assert!(k.is_leaf() && !Kind::from(Want::Content).is_leaf());
    }
}
