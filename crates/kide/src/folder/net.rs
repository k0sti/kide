//! The folder profile on the network: what a manifest root needs (§5.7).

use crate::error::Result;
use crate::format::manifest::decode_chunk;
use crate::format::path::may_be_text;
use crate::hash::Hash;
use crate::iface::ObjectStore;
use crate::iface::{Content, Link, Profile, Want};
use crate::protocol::Served;
use std::sync::Arc;

/// A manifest root is content whose chunks are manifest chunks (§5.1); every
/// version record they name is wanted with its content (§5.7), reached by
/// its path, and a note's with its whole history's content (§5.6).
pub struct Manifests;

impl Profile for Manifests {
    fn root(&self) -> Want {
        Want::Objects
    }

    fn children(&self, bytes: &[u8]) -> Result<Vec<Link>> {
        let entries = decode_chunk(bytes)?.into_iter();
        Ok(entries
            .map(|e| Link {
                id: e.version,
                want: Want::Record(match may_be_text(&e.path) {
                    true => Content::Full,
                    false => Content::Bytes,
                }),
                label: Some(e.path),
            })
            .collect())
    }
}

/// A folder served at the root `head` gives, pushing with its profile.
pub fn served(
    store: Arc<dyn ObjectStore>,
    head: impl Fn() -> Hash + Send + Sync + 'static,
) -> Served {
    Served {
        profile: Some(Arc::new(Manifests)),
        ..Served::new(store, head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_names_the_records_of_a_manifest_chunk_by_path() {
        use crate::format::manifest::PathEntry;
        let entries = [("a", 1u8), ("b/c.png", 2)].map(|(p, v)| PathEntry {
            path: p.into(),
            version: Hash([v; 32]),
            size: 0,
            mtime: 0,
        });
        let mut chunk = Vec::new();
        entries.iter().for_each(|e| e.encode_into(&mut chunk));
        let links = Manifests.children(&chunk).unwrap();
        let got: Vec<_> = links.iter().map(|l| (l.id, l.label.as_deref())).collect();
        assert_eq!(
            got,
            [(Hash([1; 32]), Some("a")), (Hash([2; 32]), Some("b/c.png"))]
        );
        let wants: Vec<_> = links.iter().map(|l| l.want).collect();
        let (text, binary) = (Content::Full, Content::Bytes);
        assert_eq!(wants, [Want::Record(text), Want::Record(binary)]);
        assert!(Manifests.children(b"no manifest").is_err());
    }

    #[test]
    fn manifests_pass_the_profile_suite() {
        let entry = |p: &str, v: u8| crate::format::manifest::PathEntry {
            path: p.into(),
            version: Hash([v; 32]),
            size: 3,
            mtime: 1,
        };
        let mut chunk = Vec::new();
        [entry("a", 1), entry("b/c.md", 2)]
            .iter()
            .for_each(|e| e.encode_into(&mut chunk));
        crate::conformance::profile(&Manifests, &[chunk]);
    }
}
