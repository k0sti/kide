//! `Plain`, a profile of plain objects for tests of walks.

use crate::error::{Result, invalid};
use crate::format::content::write_content;
use crate::format::record::Record;
use crate::hash::Hash;
use crate::iface::ObjectStore;
use crate::iface::{Content, Link, Profile, Want};

/// A profile of plain objects, for tests: each object is a list of links,
/// one tag byte (0 object, 1 record with content, 2 a version's content,
/// 3 record whose content is plain objects) and
/// an id each, labelled by their position.
pub struct Plain;

impl Plain {
    /// A checkpoint in `store`: a version record with `parents` whose
    /// content is a plain object linking one object of its own, which links
    /// to `data` as a version's content. Returns the record and that object.
    pub fn checkpoint(
        store: &dyn ObjectStore,
        mut parents: Vec<Hash>,
        data: &[u8],
    ) -> Result<(Hash, Hash)> {
        let mut put = |id: &Hash, b: &[u8]| store.put_known(id, b);
        let leaf = write_content(data, &mut put)?.id;
        let object = store.put(&Plain::object(&[(Want::Content, leaf)]))?;
        let top = Plain::object(&[(Want::Object, object)]);
        parents.sort();
        let r = Record {
            change: Vec::new(),
            parents,
            root: write_content(&top, &mut put)?.id,
            state: vec![0],
        };
        Ok((store.put(&r.encode()?)?, object))
    }

    /// The object of `links`.
    pub fn object(links: &[(Want, Hash)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (want, id) in links {
            out.push(match want {
                Want::Object => 0,
                Want::Record(Content::Bytes) => 1,
                Want::Content => 2,
                Want::Record(Content::Objects) => 3,
                w => panic!("a plain object links to no {w:?}"),
            });
            out.extend(id.0);
        }
        out
    }
}

impl Profile for Plain {
    fn root(&self) -> Want {
        Want::Object
    }

    fn children(&self, bytes: &[u8]) -> Result<Vec<Link>> {
        let (links, rest) = bytes.as_chunks::<33>();
        if !rest.is_empty() {
            return invalid("a plain object of partial links");
        }
        let link = |(i, l): (usize, &[u8; 33])| {
            let want = match l[0] {
                0 => Want::Object,
                1 => Want::Record(Content::Bytes),
                2 => Want::Content,
                3 => Want::Record(Content::Objects),
                t => return invalid(format!("link tag {t}")),
            };
            let id = Hash(l[1..].try_into().expect("32 bytes follow the tag"));
            let label = Some(i.to_string());
            Ok(Link { id, want, label })
        };
        links.iter().enumerate().map(link).collect()
    }
}
