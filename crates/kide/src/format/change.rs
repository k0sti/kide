//! Explicit folder operations (§5.2), carried by a record's opaque metadata.
use super::path::check_path;
use crate::error::{Result, invalid};
use crate::hash::Hash;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Move { from: String, to: String },
    Restore(Hash),
}

impl Change {
    pub fn encode(&self) -> Result<Vec<u8>> {
        match self {
            Self::Move { from, to } => {
                check_path(from)?;
                check_path(to)?;
                if from == to || from.ends_with('/') != to.ends_with('/') {
                    return invalid("move operands must differ and have the same path kind");
                }
                let mut out = vec![1];
                for p in [from, to] {
                    out.extend((p.len() as u16).to_le_bytes());
                    out.extend(p.as_bytes());
                }
                Ok(out)
            }
            Self::Restore(tree) => {
                let mut out = vec![2];
                out.extend(tree.0);
                Ok(out)
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Option<Self>> {
        match bytes {
            [] => Ok(None),
            [2, tree @ ..] if tree.len() == 32 => Ok(Some(Self::Restore(Hash::at(tree)))),
            [1, rest @ ..] => {
                let mut rest = rest;
                let mut path = || -> Result<String> {
                    let Some(n) = rest.first_chunk::<2>() else {
                        return invalid("short move path length");
                    };
                    let n = u16::from_le_bytes(*n) as usize;
                    let Some(p) = rest.get(2..2 + n) else {
                        return invalid("short move path");
                    };
                    let p = std::str::from_utf8(p)
                        .map_err(|_| crate::error::Error::Invalid("move path is not UTF-8".into()))?
                        .to_string();
                    rest = &rest[2 + n..];
                    Ok(p)
                };
                let change = Self::Move {
                    from: path()?,
                    to: path()?,
                };
                if !rest.is_empty() {
                    return invalid("trailing move bytes");
                }
                change.encode()?; // validate operands, including path lengths
                Ok(Some(change))
            }
            _ => invalid("unknown or malformed folder change metadata"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn folder_operations_require_one_parent() {
        let mut r = super::super::record::Record {
            parents: Vec::new(),
            root: Hash([7; 32]),
            state: vec![0],
            change: Change::Move {
                from: "a".into(),
                to: "b".into(),
            }
            .encode()
            .unwrap(),
        };
        assert!(super::super::manifest::Kind::of(&r).is_err());
        r.parents = vec![Hash([1; 32])];
        assert!(super::super::manifest::Kind::of(&r).is_ok());
        r.parents.push(Hash([2; 32]));
        assert!(super::super::manifest::Kind::of(&r).is_err());
    }

    #[test]
    fn operations_are_canonical_and_bounded() {
        for c in [
            Change::Move {
                from: "old/".into(),
                to: "new/".into(),
            },
            Change::Restore(Hash([7; 32])),
        ] {
            assert_eq!(Change::decode(&c.encode().unwrap()).unwrap(), Some(c));
        }
        for bad in [&[3][..], &[2, 0], &[1], &[1, 255, 255]] {
            assert!(Change::decode(bad).is_err());
        }
        assert!(
            Change::Move {
                from: "../x".into(),
                to: "y".into()
            }
            .encode()
            .is_err()
        );
        assert!(
            Change::Move {
                from: "x/".into(),
                to: "y".into()
            }
            .encode()
            .is_err()
        );
    }
}
