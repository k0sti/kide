//! The history of one path (§3): every version record reachable from a head,
//! each before its parents, laid out in lanes for display.

use crate::error::Result;
use crate::format::record::Record;
use crate::hash::Hash;
use crate::iface::ObjectStore;
use crate::load;
use std::collections::HashMap;

/// How a record came about (§3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// No parents: a path seen for the first time.
    New,
    /// An explicit move with source/destination in `record.change`.
    Move,
    /// An explicit forward tree restore.
    Restore,
    /// One parent and a new value: an edit, a change of state or a
    /// re-creation after a deletion.
    Edit,
    /// One parent with the same value: a conflict copy.
    Copy,
    /// Two or more parents.
    Merge,
    /// A tombstone with one parent.
    Delete,
}

impl Event {
    pub fn name(self) -> &'static str {
        match self {
            Event::New => "new",
            Event::Move => "move",
            Event::Restore => "restore",
            Event::Edit => "edit",
            Event::Copy => "copy",
            Event::Merge => "merge",
            Event::Delete => "delete",
        }
    }
}

/// One version in a history, with its place in the lanes. Every edge from a
/// record to a parent runs in a lane of its own until the parent's row.
#[derive(Clone, Debug)]
pub struct Row {
    pub id: Hash,
    pub record: Record,
    pub event: Event,
    /// The lane of this version.
    pub lane: usize,
    /// Lanes that pass this row on their way to an older version.
    pub passing: Vec<usize>,
    /// Lanes that end here, right of `lane`: other edges into this version.
    pub joining: Vec<usize>,
}

/// The history below `head`, newest first: each record comes before its
/// parents. Fails with `NotFound` if a record is not in `store`.
pub fn history(store: &dyn ObjectStore, head: &Hash) -> Result<Vec<Row>> {
    let records = reach(store, head)?;
    let mut lanes = Lanes(Vec::new());
    let mut rows = Vec::with_capacity(records.len());
    for id in order(head, &records) {
        // `order` yields only ids that `reach` stored.
        let record = records[&id].clone();
        let event = event(&record, &records);
        let (lane, passing, joining) = lanes.place(&id, &record.parents);
        rows.push(Row {
            id,
            record,
            event,
            lane,
            passing,
            joining,
        });
    }
    Ok(rows)
}

/// Every record reachable from `head` over parents.
fn reach(store: &dyn ObjectStore, head: &Hash) -> Result<HashMap<Hash, Record>> {
    let (mut todo, mut records) = (vec![*head], HashMap::new());
    while let Some(id) = todo.pop() {
        if records.contains_key(&id) {
            continue;
        }
        let r = load::record(store, &id)?;
        todo.extend(&r.parents);
        records.insert(id, r);
    }
    Ok(records)
}

/// Each record after all its children, following one line of parents as far
/// as it goes: the largest ready parent first.
fn order(head: &Hash, records: &HashMap<Hash, Record>) -> Vec<Hash> {
    let mut children: HashMap<Hash, usize> = HashMap::new();
    for r in records.values() {
        for p in &r.parents {
            *children.entry(*p).or_default() += 1;
        }
    }
    let (mut ready, mut out) = (vec![*head], Vec::with_capacity(records.len()));
    while let Some(id) = ready.pop() {
        out.push(id);
        // `reach` stored every id that `order` yields, and every parent of
        // a stored record, so the lookups below cannot fail.
        for p in &records[&id].parents {
            let n = children.get_mut(p).expect("every parent was counted");
            *n -= 1;
            if *n == 0 {
                ready.push(*p);
            }
        }
    }
    out
}

/// `records` holds every parent of `r` (see `reach`).
fn event(r: &Record, records: &HashMap<Hash, Record>) -> Event {
    match crate::format::change::Change::decode(&r.change) {
        Ok(Some(crate::format::change::Change::Move { .. })) => return Event::Move,
        Ok(Some(crate::format::change::Change::Restore(_))) => return Event::Restore,
        _ => {} // metadata is opaque for non-folder profiles
    }
    match r.parents.as_slice() {
        [] => Event::New,
        [_] if r.is_tombstone() => Event::Delete,
        [p] if records[p].value() == r.value() => Event::Copy,
        [_] => Event::Edit,
        _ => Event::Merge,
    }
}

/// The lanes, each waiting for the version its edge leads to.
struct Lanes(Vec<Option<Hash>>);

impl Lanes {
    /// Places `id`: the leftmost lane waiting for it, or a free one for the
    /// head. Its edges then wait in their own lanes, the largest parent's
    /// in the version's lane, since that parent tends to come next.
    fn place(&mut self, id: &Hash, parents: &[Hash]) -> (usize, Vec<usize>, Vec<usize>) {
        let waiting: Vec<usize> = (0..self.0.len())
            .filter(|&i| self.0[i] == Some(*id))
            .collect();
        let lane = match waiting.first() {
            Some(&l) => l,
            None => self.free(),
        };
        let joining = waiting.get(1..).unwrap_or_default().to_vec();
        for &i in &waiting {
            self.0[i] = None;
        }
        let passing = (0..self.0.len()).filter(|&i| self.0[i].is_some()).collect();
        for p in parents.iter().rev() {
            let l = match self.0[lane] {
                None => lane,
                Some(_) => self.free(),
            };
            self.0[l] = Some(*p);
        }
        (lane, passing, joining)
    }

    fn free(&mut self) -> usize {
        match self.0.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.0.push(None);
                self.0.len() - 1
            }
        }
    }
}

/// One row's lanes as text: `*` for the version, `|` for a passing lane and
/// `/` for a lane that ends here. A version takes the leftmost lane waiting
/// for it, so other lanes always end from the right.
pub fn graph(row: &Row) -> String {
    let all = row.passing.iter().chain(&row.joining);
    let width = all.fold(row.lane, |w, &l| w.max(l)) + 1;
    let mark = |i: usize| match i {
        _ if i == row.lane => '*',
        _ if row.joining.contains(&i) => '/',
        _ if row.passing.contains(&i) => '|',
        _ => ' ',
    };
    let cols: Vec<String> = (0..width).map(|i| mark(i).to_string()).collect();
    cols.join(" ").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::store::MemStore;

    fn put(s: &MemStore, mut parents: Vec<Hash>, root: u8) -> Hash {
        parents.sort();
        let r = match root {
            0 => Record::tombstone(parents),
            n => Record {
                change: Vec::new(),
                parents,
                root: Hash([n; 32]),
                state: vec![0],
            },
        };
        s.put(&r.encode().unwrap()).unwrap()
    }

    fn rows(s: &MemStore, head: Hash) -> Vec<(Hash, Event, String)> {
        let h = history(s, &head).unwrap();
        h.iter().map(|r| (r.id, r.event, graph(r))).collect()
    }

    #[test]
    fn a_line_of_edits() {
        let s = MemStore::new();
        let a = put(&s, vec![], 1);
        let b = put(&s, vec![a], 2);
        let c = put(&s, vec![b], 0);
        let d = put(&s, vec![c], 3);
        let star = || "*".to_string();
        assert_eq!(
            rows(&s, d),
            [
                (d, Event::Edit, star()),
                (c, Event::Delete, star()),
                (b, Event::Edit, star()),
                (a, Event::New, star()),
            ]
        );
    }

    #[test]
    fn a_merge_and_a_copy() {
        let s = MemStore::new();
        let a = put(&s, vec![], 1);
        let (x, y) = (put(&s, vec![a], 2), put(&s, vec![a], 3));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        let m = put(&s, vec![x, y], 2);
        let g = |t: &str| t.to_string();
        let want = [
            (m, Event::Merge, g("*")),
            (hi, Event::Edit, g("* |")),
            (lo, Event::Edit, g("| *")),
            (a, Event::New, g("* /")),
        ];
        assert_eq!(rows(&s, m), want);
        let copy = put(&s, vec![lo], if lo == x { 2 } else { 3 });
        let h = rows(&s, copy);
        assert_eq!((h[0].1, h[1].0, h.len()), (Event::Copy, lo, 3));
    }

    #[test]
    fn every_record_comes_before_its_parents() {
        let s = MemStore::new();
        let a = put(&s, vec![], 1);
        let mut tips = vec![put(&s, vec![a], 2), put(&s, vec![a], 3)];
        for i in 4..40u8 {
            let j = i as usize;
            let parents = match i % 3 {
                0 => vec![tips[j % tips.len()], tips[(j + 1) % tips.len()]],
                _ => vec![tips[j % tips.len()]],
            };
            let mut parents = parents;
            parents.dedup();
            tips.push(put(&s, parents, i));
        }
        let head = put(&s, vec![tips[tips.len() - 1], tips[tips.len() - 2]], 99);
        let h = history(&s, &head).unwrap();
        let at = |id: &Hash| h.iter().position(|r| r.id == *id).unwrap();
        for (i, r) in h.iter().enumerate() {
            assert!(r.record.parents.iter().all(|p| at(p) > i));
        }
        let mut ids: Vec<Hash> = h.iter().map(|r| r.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), h.len(), "each record once");
    }

    #[test]
    fn a_missing_record_is_not_found() {
        let s = MemStore::new();
        let gone = Hash([7; 32]);
        let head = put(&s, vec![gone], 1);
        let r = history(&s, &head);
        assert!(matches!(r, Err(Error::NotFound(id)) if id == gone));
    }
}
