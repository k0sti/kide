//! `DecisionTable`: the rule of §5.6 over the frontier of §3.2, with the
//! text merge of notes, the conflict copies and the mtime table. Every
//! result depends only on the pair of sides, never on which one is local.

use crate::error::{Result, invalid};
use crate::format::content::write_content;
use crate::format::manifest::Kind;
use crate::format::path::{conflict_path, is_prose};
use crate::format::record::Record;
use crate::hash::Hash;
use crate::iface::{ConflictCopy, Decision, History, Outcome, ReadContent, Resolver, Side};
use crate::text::{is_text, merge3};
use std::collections::{HashMap, HashSet};

mod peer;
pub use peer::{Join, Reset};

#[derive(Clone, Copy, Debug, Default)]
pub struct DecisionTable;

impl Resolver for DecisionTable {
    fn decide(
        &self,
        path: &str,
        local: Option<&Side>,
        remote: Option<&Side>,
        history: &mut History,
        content: &mut ReadContent,
    ) -> Result<Decision> {
        let (l, r) = match (local, remote) {
            (Some(l), Some(r)) if l.id == r.id => {
                let mtime = regular_only(&l.record, l.mtime.max(r.mtime));
                return Ok(plain(Outcome::Keep, mtime)); // row 1
            }
            (Some(l), None) => return Ok(plain(Outcome::Keep, l.mtime)), // row 2
            (None, Some(r)) => return Ok(plain(Outcome::Adopt(r.id), r.mtime)), // row 3
            (Some(l), Some(r)) => (l, r),
            (None, None) => return invalid(format!("{path:?}: no side to decide")),
        };
        let mut known = HashMap::from([(l.id, l.record.clone()), (r.id, r.record.clone())]);
        let f = frontier(l, r, &mut known, history)?;
        let keep = plain(Outcome::Keep, l.mtime);
        let adopt = plain(Outcome::Adopt(r.id), r.mtime);
        match f[..] {
            [x] if x == l.id => return Ok(keep),
            [x] if x == r.id => return Ok(adopt),
            [x] => return invalid(format!("{path:?}: a frontier of {x}, neither side")),
            _ => {}
        }
        let m = merge(&f, &known);
        let text = text_merge(path, &f, &m, &mut known, history, content)?;
        let (m, text) = text.map_or((m, None), |(m, t)| (m, Some(t)));
        if m == l.record {
            return Ok(keep);
        }
        if m == r.record {
            return Ok(adopt);
        }
        if text.is_some() {
            // Every leaf's changes are in the merged text: no copies.
            return Ok(Decision {
                mtime: regular_only(&m, by_value(m.value(), l, r)),
                outcome: Outcome::Merge(m),
                copies: Vec::new(),
                content: text,
            });
        }
        let copies = f
            .iter()
            .filter(|x| wants_copy(x, &known[*x], &m, l, r))
            .map(|x| copy(path, x, &known[x], l, r))
            .collect();
        Ok(Decision {
            mtime: regular_only(&m, by_value(m.value(), l, r)),
            outcome: Outcome::Merge(m),
            copies,
            content: None,
        })
    }
}

fn plain(outcome: Outcome, mtime: i64) -> Decision {
    Decision {
        outcome,
        mtime,
        copies: Vec::new(),
        content: None,
    }
}

/// Only regular files carry a synchronised mtime; links, directories and
/// tombstones have 0 (§5.1).
fn regular_only(r: &Record, mtime: i64) -> i64 {
    match Kind::of(r) {
        Ok(Some(Kind::Regular | Kind::Executable)) => mtime,
        _ => 0,
    }
}

/// The mtime of the side whose record has value `v`; the larger one if both
/// or neither have it (§5.6).
fn by_value(v: (Hash, &[u8]), l: &Side, r: &Side) -> i64 {
    match (l.record.value() == v, r.record.value() == v) {
        (true, false) => l.mtime,
        (false, true) => r.mtime,
        _ => l.mtime.max(r.mtime),
    }
}

fn leaves(s: &Side) -> Vec<Hash> {
    match s.record.is_merge() {
        true => s.record.parents.clone(),
        false => vec![s.id],
    }
}

/// Reads the records of `ids` not yet known through `history`.
fn load(ids: &[Hash], known: &mut HashMap<Hash, Record>, history: &mut History) -> Result<()> {
    let missing: Vec<Hash> = ids
        .iter()
        .filter(|x| !known.contains_key(x))
        .copied()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let records = history(&missing)?;
    if records.len() != missing.len() {
        return invalid("history returned another number of records");
    }
    known.extend(missing.into_iter().zip(records));
    Ok(())
}

/// The frontier of L and R, sorted (§3.2): the leaves of either side that
/// are not an ancestor of another. One breadth-first search over parents
/// from all of them, a generation per `history` call; it stops once a
/// single leaf can remain.
fn frontier(
    l: &Side,
    r: &Side,
    known: &mut HashMap<Hash, Record>,
    history: &mut History,
) -> Result<Vec<Hash>> {
    let mut all = leaves(l);
    for x in leaves(r) {
        if !all.contains(&x) {
            all.push(x);
        }
    }
    load(&all, known, history)?;
    let mut seen = HashSet::new();
    let mut todo: Vec<Hash> = all
        .iter()
        .flat_map(|x| known[x].parents.iter().copied())
        .filter(|p| seen.insert(*p))
        .collect();
    let mut below = HashSet::new();
    while !todo.is_empty() {
        below.extend(todo.iter().filter(|x| all.contains(x)).copied());
        if below.len() + 1 >= all.len() {
            break; // records are acyclic: one leaf always remains
        }
        let records = history(&todo)?;
        if records.len() != todo.len() {
            return invalid("history returned another number of records");
        }
        todo = records
            .into_iter()
            .flat_map(|rec| rec.parents)
            .filter(|p| seen.insert(*p))
            .collect();
    }
    let mut f: Vec<Hash> = all.into_iter().filter(|x| !below.contains(x)).collect();
    f.sort();
    Ok(f)
}

/// The merge (value of w, F sorted), where w is the live element of F with
/// the largest id, or the largest id if all are tombstones (§5.6).
fn merge(f: &[Hash], known: &HashMap<Hash, Record>) -> Record {
    let key = |x: &&Hash| (!known[*x].is_tombstone(), **x);
    // A frontier here has two or more elements.
    let w = &known[f.iter().max_by_key(key).expect("a frontier")];
    Record {
        change: Vec::new(),
        parents: f.to_vec(),
        root: w.root,
        state: w.state.clone(),
    }
}

/// The largest note a text merge reads, per side.
const MAX_TEXT: usize = 4 << 20;

/// The text merge (§5.6), for text contents only (`is_text`): when the frontier `f` holds two or more
/// live regular files of different values with a live regular common base,
/// the merge `m` takes the three-way merge of their contents instead of w's
/// value, folded over the live elements in id order. `None` when it does
/// not apply, or a diff gives up.
fn text_merge(
    path: &str,
    f: &[Hash],
    m: &Record,
    known: &mut HashMap<Hash, Record>,
    history: &mut History,
    content: &mut ReadContent,
) -> Result<Option<(Record, Vec<u8>)>> {
    let regular = |r: &Record| matches!(Kind::of(r), Ok(Some(Kind::Regular | Kind::Executable)));
    let live: Vec<Hash> = f
        .iter()
        .filter(|x| !known[*x].is_tombstone())
        .copied()
        .collect();
    let differ = live.iter().any(|x| known[x].root != known[&live[0]].root);
    if !is_prose(path) || live.len() < 2 || !differ || !live.iter().all(|x| regular(&known[x])) {
        return Ok(None);
    }
    let Some(base) = base(&live, known, history)?.filter(|b| regular(&known[b])) else {
        return Ok(None);
    };
    let mut read = |x: &Hash| -> Result<Option<Vec<u8>>> {
        let t = content(&known[x].root, MAX_TEXT as u64)?;
        Ok(t.filter(|t| is_text(t)))
    };
    let (Some(o), Some(mut text)) = (read(&base)?, read(&live[0])?) else {
        return Ok(None);
    };
    for x in &live[1..] {
        let Some(t) = read(x)? else { return Ok(None) };
        let Some(merged) = merge3(&o, &text, &t) else {
            return Ok(None);
        };
        text = merged;
    }
    let root = write_content(&text, &mut |_, _| Ok(()))?.id;
    let merged = Record {
        change: Vec::new(),
        parents: m.parents.clone(),
        root,
        state: m.state.clone(),
    };
    Ok(Some((merged, text)))
}

/// The nearest common ancestor of `live`: of the records all of them reach
/// (themselves included), those that no other such record reaches, the
/// smallest id if several; `None` if they share none.
fn base(
    live: &[Hash],
    known: &mut HashMap<Hash, Record>,
    history: &mut History,
) -> Result<Option<Hash>> {
    let mut common: Option<HashSet<Hash>> = None;
    for x in live {
        let reach = ancestors(*x, known, history)?;
        common = Some(match common {
            None => reach,
            Some(c) => c.intersection(&reach).copied().collect(),
        });
    }
    let common = common.unwrap_or_default();
    let below: HashSet<Hash> = common
        .iter()
        .flat_map(|c| known[c].parents.iter().copied())
        .filter(|p| common.contains(p))
        .collect();
    Ok(common.difference(&below).min().copied())
}

/// `x` and every record it descends from, read a generation at a time.
fn ancestors(
    x: Hash,
    known: &mut HashMap<Hash, Record>,
    history: &mut History,
) -> Result<HashSet<Hash>> {
    let (mut seen, mut todo) = (HashSet::from([x]), vec![x]);
    while !todo.is_empty() {
        load(&todo, known, history)?;
        todo = todo
            .iter()
            .flat_map(|t| known[t].parents.clone())
            .filter(|p| seen.insert(*p))
            .collect();
    }
    Ok(seen)
}

/// Whether the new merge `m` copies `x`: live content other than m's,
/// unless x is a leaf of L or R where that record is a merge showing
/// another value; that merge's maker copied x then.
fn wants_copy(x: &Hash, xr: &Record, m: &Record, l: &Side, r: &Side) -> bool {
    let lost = |s: &Side| {
        s.record.is_merge() && s.record.parents.contains(x) && s.record.value() != xr.value()
    };
    !xr.is_tombstone() && xr.root != m.root && !lost(l) && !lost(r)
}

fn copy(path: &str, x: &Hash, xr: &Record, l: &Side, r: &Side) -> ConflictCopy {
    ConflictCopy {
        path: conflict_path(path, x),
        record: Record {
            change: Vec::new(),
            parents: vec![*x],
            root: xr.root,
            state: xr.state.clone(),
        },
        mtime: regular_only(xr, by_value(xr.value(), l, r)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn conformance() {
        crate::conformance::resolver(&|| Box::new(DecisionTable));
    }

    #[test]
    fn neither_side_is_invalid() {
        let r = DecisionTable.decide("x", None, None, &mut |_| Ok(vec![]), &mut |_, _| Ok(None));
        assert!(matches!(r, Err(Error::Invalid(_))));
    }

    #[test]
    fn a_fast_forward_stops_the_search_early() {
        // a ← b ← c ← d: L = a, R = d; only the generations down to a are read.
        let mut chain = vec![];
        let mut recs = HashMap::new();
        for i in 0..4u8 {
            let r = Record {
                change: Vec::new(),
                parents: chain.last().map(|p| vec![*p]).unwrap_or_default(),
                root: Hash([i + 1; 32]),
                state: vec![0],
            };
            let id = r.id().unwrap();
            recs.insert(id, r);
            chain.push(id);
        }
        let side = |id: Hash| Side {
            id,
            record: recs[&id].clone(),
            mtime: 1,
        };
        let mut calls = 0;
        let mut history = |ids: &[Hash]| {
            calls += 1;
            Ok(ids.iter().map(|x| recs[x].clone()).collect())
        };
        let (l, r) = (side(chain[0]), side(chain[3]));
        let d = DecisionTable
            .decide("f", Some(&l), Some(&r), &mut history, &mut |_, _| Ok(None))
            .unwrap();
        assert_eq!(d.outcome, Outcome::Adopt(chain[3]));
        assert_eq!(calls, 2, "generations c and b, then a is found");
    }

    /// Records whose roots are real content, and that content by root.
    #[derive(Default)]
    struct Notes {
        records: HashMap<Hash, Record>,
        content: HashMap<Hash, Vec<u8>>,
    }

    impl Notes {
        fn put(&mut self, text: &str, parents: &[Hash]) -> Hash {
            let root = crate::format::content::write_content(text.as_bytes(), &mut |_, _| Ok(()))
                .unwrap()
                .id;
            self.content.insert(root, text.as_bytes().to_vec());
            let r = Record {
                change: Vec::new(),
                parents: parents.to_vec(),
                root,
                state: vec![0],
            };
            let id = r.id().unwrap();
            self.records.insert(id, r);
            id
        }

        fn side(&self, id: Hash, mtime: i64) -> Side {
            Side {
                id,
                record: self.records[&id].clone(),
                mtime,
            }
        }

        fn decide(&self, path: &str, l: Hash, r: Hash) -> Decision {
            let (l, r) = (self.side(l, 1), self.side(r, 2));
            let mut history =
                |ids: &[Hash]| Ok(ids.iter().map(|x| self.records[x].clone()).collect());
            let mut content = |root: &Hash, max: u64| {
                let t = &self.content[root];
                Ok((t.len() as u64 <= max).then(|| t.clone()))
            };
            DecisionTable
                .decide(path, Some(&l), Some(&r), &mut history, &mut content)
                .unwrap()
        }
    }

    #[test]
    fn concurrent_edits_of_a_note_merge_without_copies() {
        let mut n = Notes::default();
        let base = n.put("# Plan\none\ntwo\nthree\n", &[]);
        let l = n.put("# Plan\nONE\ntwo\nthree\n", &[base]);
        let r = n.put("# Plan\none\ntwo\nTHREE\n", &[base]);
        let d = n.decide("notes/plan.md", l, r);
        let Outcome::Merge(m) = &d.outcome else {
            panic!("{d:?}")
        };
        assert_eq!(
            d.content.as_deref(),
            Some(&b"# Plan\nONE\ntwo\nTHREE\n"[..])
        );
        let mut sorted = vec![l, r];
        sorted.sort();
        assert_eq!(m.parents, sorted);
        assert!(d.copies.is_empty());
        assert_eq!(d.mtime, 2, "neither side's value: the larger mtime");
        // Either side decides the same merge.
        assert_eq!(n.decide("notes/plan.md", r, l).outcome, d.outcome);
    }

    #[test]
    fn binary_files_and_unrelated_texts_keep_conflict_copies() {
        let mut n = Notes::default();
        let base = n.put("a\nb\n", &[]);
        let (l, r) = (n.put("A\nb\n", &[base]), n.put("a\nB\n", &[base]));
        let d = n.decide("src/plan.txt", l, r);
        assert_eq!(
            d.content.as_deref(),
            Some(&b"A\nB\n"[..]),
            "any text file merges"
        );
        assert!(d.copies.is_empty());
        let (l, r) = (n.put("A\0\nb\n", &[base]), n.put("a\nB\n", &[base]));
        let d = n.decide("notes/plan.md", l, r);
        assert_eq!(
            (d.copies.len(), d.content),
            (1, None),
            "a zero byte: not text"
        );
        let (x, y) = (n.put("x\n", &[]), n.put("y\n", &[]));
        let d = n.decide("notes/new.md", x, y);
        assert_eq!((d.copies.len(), d.content), (1, None), "no common base");
    }

    #[test]
    fn a_merge_of_a_merge_and_a_later_edit_keeps_every_change() {
        let mut n = Notes::default();
        let base = n.put("1\n2\n3\n4\n", &[]);
        let a = n.put("1a\n2\n3\n4\n", &[base]);
        let b = n.put("1\n2\n3b\n4\n", &[base]);
        let first = n.decide("n.md", a, b);
        let Outcome::Merge(m) = first.outcome else {
            panic!()
        };
        let m = {
            let text = String::from_utf8(first.content.unwrap()).unwrap();
            let id = n.put(&text, &m.parents);
            assert_eq!(n.records[&id], m);
            id
        };
        // c edits a concurrently with the merge.
        let c = n.put("1a\n2c\n3\n4\n", &[a]);
        let d = n.decide("n.md", m, c);
        assert_eq!(d.content.as_deref(), Some(&b"1a\n2c\n3b\n4\n"[..]));
    }
}
