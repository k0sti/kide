//! Version records (§3.1).

use super::{S, W};
use crate::error::{Result, invalid};
use crate::hash::{Hash, h};

/// A version record: parents, content root and profile state. A zero root
/// makes it a tombstone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Version-record ids, strictly ascending, at most W.
    pub parents: Vec<Hash>,
    pub root: Hash,
    /// Opaque to the core; at most S bytes.
    pub state: Vec<u8>,
    /// Opaque operation metadata, not part of the value; at most MAX + 8 bytes.
    pub change: Vec<u8>,
}

impl Record {
    /// A tombstone after `parents`, which must be sorted.
    pub fn tombstone(parents: Vec<Hash>) -> Record {
        Record {
            change: Vec::new(),
            parents,
            root: Hash::ZERO,
            state: Vec::new(),
        }
    }

    pub fn is_tombstone(&self) -> bool {
        self.root == Hash::ZERO
    }

    /// A record with two or more parents (§3.2).
    pub fn is_merge(&self) -> bool {
        self.parents.len() >= 2
    }

    /// The pair (root, state); operation metadata affects the id, not the value.
    pub fn value(&self) -> (Hash, &[u8]) {
        (self.root, &self.state)
    }

    /// Checks the rules of §3.1 that need no other object.
    pub fn validate(&self) -> Result<()> {
        if self.parents.len() > W {
            return invalid(format!("{} parents", self.parents.len()));
        }
        if self.parents.windows(2).any(|p| p[0] >= p[1]) {
            return invalid("parents not strictly ascending");
        }
        if self.is_merge() && !self.change.is_empty() {
            return invalid("merge records have no operation metadata");
        }
        if self.change.len() > super::MAX + 8 {
            return invalid("change metadata exceeds MAX + 8");
        }
        if self.state.len() > S {
            return invalid(format!("state of {} bytes", self.state.len()));
        }
        if self.is_tombstone() && (!self.state.is_empty() || self.parents.is_empty()) {
            return invalid("a tombstone has an empty state and a parent");
        }
        Ok(())
    }

    /// Validates, then encodes.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut out =
            Vec::with_capacity(48 + 32 * self.parents.len() + self.change.len() + self.state.len());
        out.extend((self.parents.len() as u64).to_le_bytes());
        for p in &self.parents {
            out.extend(p.0);
        }
        out.extend(self.root.0);
        out.extend((self.change.len() as u64).to_le_bytes());
        out.extend(&self.change);
        out.extend(&self.state);
        Ok(out)
    }

    pub fn id(&self) -> Result<Hash> {
        Ok(h(&self.encode()?))
    }

    /// Decodes and validates (§3.1). Whether a merge record's parents are
    /// merge records needs the parents, so the store checks it.
    pub fn decode(b: &[u8]) -> Result<Record> {
        let Some(count) = b.first_chunk::<8>().map(|c| u64::from_le_bytes(*c)) else {
            return invalid("version record shorter than 8 bytes");
        };
        if count > W as u64 {
            return invalid(format!("{count} parents"));
        }
        let n = count as usize;
        let change_at = 48 + 32 * n;
        let length_at = 40 + 32 * n;
        if b.len() < change_at {
            return invalid("version record lacks change length");
        }
        let length = u64::from_le_bytes(
            b[length_at..change_at]
                .try_into()
                .map_err(|_| crate::error::Error::Invalid("change length".into()))?,
        );
        if length > (super::MAX + 8) as u64 {
            return invalid("change metadata exceeds MAX + 8");
        }
        let state_at = change_at + length as usize;
        if b.len() < state_at {
            return invalid(format!(
                "version record of {} bytes with {n} parents",
                b.len()
            ));
        }
        let r = Record {
            parents: (0..n).map(|i| Hash::at(&b[8 + 32 * i..])).collect(),
            root: Hash::at(&b[8 + 32 * n..]),
            change: b[change_at..state_at].to_vec(),
            state: b[state_at..].to_vec(),
        };
        r.validate()?;
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(i: u8) -> Hash {
        h(&[i])
    }

    fn sorted(mut v: Vec<Hash>) -> Vec<Hash> {
        v.sort();
        v
    }

    /// Encodes without validating, to build malformed records.
    fn raw(count: u64, parents: &[Hash], root: Hash, state: &[u8]) -> Vec<u8> {
        let mut b = count.to_le_bytes().to_vec();
        parents.iter().for_each(|p| b.extend(p.0));
        b.extend(root.0);
        b.extend(0u64.to_le_bytes());
        b.extend(state);
        b
    }

    #[test]
    fn change_metadata_round_trips_without_changing_the_value() {
        let mut r = Record {
            parents: vec![],
            root: id(1),
            state: vec![0],
            change: Vec::new(),
        };
        let before = r.id().unwrap();
        let value = (r.root, r.state.clone());
        r.change = b"explicit change".to_vec();
        assert_eq!(Record::decode(&r.encode().unwrap()).unwrap(), r);
        assert_ne!(r.id().unwrap(), before);
        assert_eq!((r.root, r.state.clone()), value);
    }

    #[test]
    fn round_trip() {
        let records = [
            Record {
                change: Vec::new(),
                parents: vec![],
                root: id(1),
                state: vec![0],
            },
            Record {
                change: Vec::new(),
                parents: sorted(vec![id(2), id(3), id(4)]),
                root: id(1),
                state: vec![7; S],
            },
            Record::tombstone(vec![id(2)]),
            Record {
                change: Vec::new(),
                parents: sorted((0..W as u8).map(id).collect()),
                root: id(1),
                state: vec![],
            },
        ];
        for r in records {
            let enc = r.encode().unwrap();
            assert_eq!(
                enc.len(),
                48 + 32 * r.parents.len() + r.change.len() + r.state.len()
            );
            assert_eq!(Record::decode(&enc).unwrap(), r);
        }
        assert!(Record::tombstone(vec![id(1)]).is_tombstone());
        assert!(Record::tombstone(sorted(vec![id(1), id(2)])).is_merge());
    }

    #[test]
    fn decode_rejects_invalid_records() {
        let [a, b] = [id(1), id(2)];
        let [lo, hi] = if a < b { [a, b] } else { [b, a] };
        let many = sorted((0..=W as u8).map(id).collect());
        let bad: [(&str, Vec<u8>); 10] = [
            ("empty", vec![]),
            ("short count", vec![0; 7]),
            ("no root", raw(0, &[], a, b"")[..39].to_vec()),
            ("count beyond length", raw(2, &[lo], a, b"")),
            ("huge count", raw(u64::MAX, &[], a, b"")),
            ("unsorted", raw(2, &[hi, lo], a, b"\0")),
            ("duplicate", raw(2, &[lo, lo], a, b"\0")),
            ("more than W parents", raw(W as u64 + 1, &many, a, b"")),
            ("tombstone with state", raw(1, &[a], Hash::ZERO, b"\0")),
            ("tombstone without parent", raw(0, &[], Hash::ZERO, b"")),
        ];
        for (name, enc) in bad {
            assert!(Record::decode(&enc).is_err(), "{name}");
        }
        assert!(
            Record::decode(&raw(0, &[], a, &[0; S + 1])).is_err(),
            "state beyond S"
        );
        assert!(
            Record::decode(&raw(0, &[], a, &[0; S])).is_ok(),
            "state of S"
        );
    }

    #[test]
    fn encode_validates() {
        let unsorted = Record {
            change: Vec::new(),
            parents: vec![id(1), id(1)],
            root: id(3),
            state: vec![],
        };
        assert!(unsorted.encode().is_err());
        assert!(Record::tombstone(vec![]).encode().is_err());
    }
}
