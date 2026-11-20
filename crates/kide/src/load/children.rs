//! GetChildren (§4.2): the server's answer from its store, and the client's
//! split of it.

use super::node;
use crate::error::{Error, Result};
use crate::format::node::Node;
use crate::hash::{Hash, h};
use crate::iface::ObjectStore;

/// The chunks of the level-1 node `id`, concatenated in node order; `None`
/// (Missing) unless the store holds the node and all its chunks.
pub fn children(store: &dyn ObjectStore, id: &Hash) -> Result<Option<Vec<u8>>> {
    let n = match node(store, id) {
        Ok(n) if n.level == 1 => n,
        Ok(_) | Err(Error::NotFound(_) | Error::Invalid(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    // A level-1 node holds at most W chunks of at most MAX bytes; its claim
    // is checked against them only as they are read.
    let cap = n
        .bytes()
        .min((crate::format::W * crate::format::MAX) as u64);
    let mut out = Vec::with_capacity(cap as usize);
    for c in &n.children {
        match store.get(&c.id) {
            Ok(b) => out.extend(b),
            Err(Error::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    Ok(Some(out))
}

/// Splits a Children payload for `node`, which has id `id`, into its chunks
/// and checks each. A wrong total or one mismatch is `Corrupt(id)`.
pub fn split_children<'a>(id: &Hash, node: &Node, payload: &'a [u8]) -> Result<Vec<&'a [u8]>> {
    if node.level != 1 || node.bytes() != payload.len() as u64 {
        return Err(Error::Corrupt(*id));
    }
    let mut rest = payload;
    let mut chunks = Vec::with_capacity(node.children.len());
    for c in &node.children {
        let (chunk, tail) = rest.split_at(c.bytes as usize); // the total fits
        if h(chunk) != c.id {
            return Err(Error::Corrupt(*id));
        }
        chunks.push(chunk);
        rest = tail;
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::node::{chunk_entry, encode_node};
    use crate::store::MemStore;

    #[test]
    fn served_and_split() {
        let s = MemStore::new();
        let (a, b) = (b"first".to_vec(), vec![7; 3000]);
        s.put(&a).unwrap();
        s.put(&b).unwrap();
        let entries = [chunk_entry(&a), chunk_entry(&b), chunk_entry(&a)];
        let whole = s.put(&encode_node(1, &entries)).unwrap();
        let gap = s
            .put(&encode_node(1, &[chunk_entry(&a), chunk_entry(b"x")]))
            .unwrap();
        let upper = s.put(&encode_node(2, &[chunk_entry(&a)])).unwrap();
        let empty = s.put(&encode_node(1, &[])).unwrap();
        let payload = children(&s, &whole).unwrap().expect("held");
        assert_eq!(payload, [&a[..], &b, &a].concat());
        assert_eq!(children(&s, &empty).unwrap(), Some(vec![]));
        for id in [gap, upper, h(&a), h(b"unknown")] {
            assert_eq!(children(&s, &id).unwrap(), None, "{id}");
        }
        let n = Node::decode(&s.get(&whole).unwrap()).unwrap();
        let chunks = split_children(&whole, &n, &payload).unwrap();
        assert_eq!(chunks, [&a[..], &b, &a]);
        let mut bad = payload.clone();
        bad[10] ^= 1;
        for p in [&bad[..], &payload[1..], &[&payload[..], b"x"].concat()] {
            assert!(matches!(split_children(&whole, &n, p), Err(Error::Corrupt(x)) if x == whole));
        }
    }
}
