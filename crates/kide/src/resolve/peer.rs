//! Resolvers that let the peer win (§5.6, "Joining and resetting"), for a
//! device that has lost its history or wants the peer's state. Unlike
//! `DecisionTable`, they depend on which side is local, so a device uses
//! them only for a pull it asks for, never as its everyday rule.

use super::{DecisionTable, ancestors, plain, regular_only};
use crate::error::Result;
use crate::format::path::conflict_path;
use crate::format::record::Record;
use crate::iface::{ConflictCopy, Decision, History, Outcome, ReadContent, Resolver, Side};
use std::collections::HashMap;

/// A device without history joins: where the sides have diverged, the
/// remote record wins, and the local content is kept as a conflict copy
/// unless the remote record's history already holds it (then it was only
/// stale). Everywhere else, the rule of `DecisionTable`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Join;

impl Resolver for Join {
    fn decide(
        &self,
        path: &str,
        local: Option<&Side>,
        remote: Option<&Side>,
        history: &mut History,
        content: &mut ReadContent,
    ) -> Result<Decision> {
        let d = DecisionTable.decide(path, local, remote, history, content)?;
        let (Some(l), Some(r), Outcome::Merge(_)) = (local, remote, &d.outcome) else {
            return Ok(d);
        };
        let mut known = HashMap::new();
        let seen = ancestors(r.id, &mut known, history)?;
        let stale = seen.iter().any(|x| known[x].value() == l.record.value());
        let mut adopt = plain(Outcome::Adopt(r.id), r.mtime);
        if !stale && !l.record.is_tombstone() {
            adopt.copies.push(ConflictCopy {
                path: conflict_path(path, &l.id),
                record: Record {
                    change: Vec::new(),
                    parents: vec![l.id],
                    root: l.record.root,
                    state: l.record.state.clone(),
                },
                mtime: regular_only(&l.record, l.mtime),
            });
        }
        Ok(adopt)
    }
}

/// A device resets to the peer's state: every path takes the remote entry,
/// and a live path only this device has becomes a tombstone. Its files go
/// to the trash; its versions stay in the store until garbage collection.
#[derive(Clone, Copy, Debug, Default)]
pub struct Reset;

impl Resolver for Reset {
    fn decide(
        &self,
        path: &str,
        local: Option<&Side>,
        remote: Option<&Side>,
        history: &mut History,
        content: &mut ReadContent,
    ) -> Result<Decision> {
        match (local, remote) {
            (Some(l), Some(r)) if l.id != r.id => Ok(plain(Outcome::Adopt(r.id), r.mtime)),
            (Some(l), None) if !l.record.is_tombstone() => {
                let gone = Record::tombstone(vec![l.id]);
                Ok(plain(Outcome::Merge(gone), 0))
            }
            _ => DecisionTable.decide(path, local, remote, history, content),
        }
    }
}
