//! Conformance suite for `Resolver` (§3.2, §5.6): rows 1 to 3, the frontier
//! rule, the mtime table, conflict copies (only with a new merge, never of
//! an element that lost an earlier merge in L or R), error pass-through,
//! symmetry of the result and the copies, and one record for any pairing of
//! three concurrent edits.
//!
//! Each case names its frontier by hand; the suite derives the merge, the
//! mtime and the copies from it by the rules of §5.6.

use super::Rng;
use crate::error::{Error, Result};
use crate::format::content::write_content;
use crate::format::manifest::Kind;
use crate::format::record::Record;
use crate::hash::{Hash, to_hex};
use crate::iface::{ConflictCopy, Decision, Outcome, Resolver, Side};
use std::collections::{BTreeSet, HashMap};

/// Runs every check on one resolver from `make`.
pub fn resolver(make: &dyn Fn() -> Box<dyn Resolver>) {
    let r = make();
    let r = &*r;
    for c in cases() {
        eprintln!("resolver: {}", c.name);
        check_case(r, &c);
    }
    eprintln!("resolver: history errors pass through");
    errors_pass_through(r);
    eprintln!("resolver: any pairing of three concurrent edits gives one record");
    canonical_merges(r);
    eprintln!("resolver: random symmetry");
    random_symmetry(r);
    eprintln!("resolver: concurrent edits of a note merge");
    text_merges(r);
}

const R1: Hash = Hash([0xa1; 32]);
const R2: Hash = Hash([0xa2; 32]);
const R3: Hash = Hash([0xa3; 32]);

/// Version records by id: the history a resolver may read.
#[derive(Clone, Default)]
struct World(HashMap<Hash, Record>);

impl World {
    fn insert(&mut self, r: Record) -> Hash {
        let id = r.id().expect("a valid record");
        self.0.insert(id, r);
        id
    }

    fn live(&mut self, root: Hash, kind: Kind, parents: &[Hash]) -> Hash {
        let mut parents = parents.to_vec();
        parents.sort();
        self.insert(Record {
            change: Vec::new(),
            parents,
            root,
            state: kind.state(),
        })
    }

    fn tombstone(&mut self, parent: Hash) -> Hash {
        self.insert(Record::tombstone(vec![parent]))
    }

    fn merge_of(&mut self, leaves: &[Hash]) -> Hash {
        let m = self.merge(leaves);
        self.insert(m)
    }

    fn side(&self, id: Hash, mtime: i64) -> Side {
        let record = self.0[&id].clone();
        let mtime = if record.is_tombstone() { 0 } else { mtime };
        Side { id, record, mtime }
    }

    /// The element whose value a merge takes: the live one with the largest
    /// id, or the largest id if all are tombstones (§5.6).
    fn winner(&self, f: &[Hash]) -> Hash {
        let key = |x: &&Hash| (!self.0[*x].is_tombstone(), **x);
        *f.iter().max_by_key(key).expect("a frontier")
    }

    fn merge(&self, f: &[Hash]) -> Record {
        let w = &self.0[&self.winner(f)];
        let mut parents = f.to_vec();
        parents.sort();
        Record {
            change: Vec::new(),
            parents,
            root: w.root,
            state: w.state.clone(),
        }
    }

    /// The decision of §5.6 for a frontier `f` of two or more elements:
    /// copies only with a new merge, never of an element that lost an
    /// earlier merge in L or R, each with the mtime of the side(s) holding
    /// its value; mtimes only for regular files.
    fn decision(&self, path: &str, l: &Side, r: &Side, f: &[Hash]) -> Decision {
        let m = self.merge(f);
        let by_value = |v: (Hash, &[u8])| match (l.record.value() == v, r.record.value() == v) {
            (true, false) => l.mtime,
            (false, true) => r.mtime,
            _ => l.mtime.max(r.mtime),
        };
        if m == l.record {
            return plain(Outcome::Keep, l.mtime);
        }
        if m == r.record {
            return plain(Outcome::Adopt(r.id), r.mtime);
        }
        let lost = |s: &Side, x: &Hash, xr: &Record| {
            s.record.is_merge() && s.record.parents.contains(x) && s.record.value() != xr.value()
        };
        let copies = f
            .iter()
            .filter(|x| {
                let xr = &self.0[*x];
                !xr.is_tombstone() && xr.root != m.root && !lost(l, x, xr) && !lost(r, x, xr)
            })
            .map(|x| {
                let xr = &self.0[x];
                ConflictCopy {
                    path: conflict_path(path, x),
                    record: Record {
                        change: Vec::new(),
                        parents: vec![*x],
                        ..xr.clone()
                    },
                    mtime: regular_only(xr, by_value(xr.value())),
                }
            })
            .collect();
        Decision {
            mtime: regular_only(&m, by_value(m.value())),
            outcome: Outcome::Merge(m),
            copies,
            content: None,
        }
    }
}

fn plain(outcome: Outcome, mtime: i64) -> Decision {
    Decision {
        outcome,
        mtime,
        copies: vec![],
        content: None,
    }
}

/// A note's records with real content: base, then two concurrent edits.
fn notes(base: &str, a: &str, b: &str) -> (World, HashMap<Hash, Vec<u8>>, [Hash; 3]) {
    let (mut w, mut content) = (World::default(), HashMap::new());
    let mut put = |w: &mut World, text: &str, parents: &[Hash]| {
        // Only the root is computed; nothing is stored.
        let root = write_content(text.as_bytes(), &mut |_, _| Ok(()))
            .expect("content")
            .id;
        content.insert(root, text.as_bytes().to_vec());
        w.live(root, Kind::Regular, parents)
    };
    let o = put(&mut w, base, &[]);
    let (x, y) = (put(&mut w, a, &[o]), put(&mut w, b, &[o]));
    (w, content, [o, x, y])
}

/// Concurrent edits of a text file in different places merge into one
/// text, with no conflict copy, from either side alike, whatever the
/// file's name; binary contents keep the copies; errors of `content` pass
/// through.
fn text_merges(r: &dyn Resolver) {
    let (w, content, [_, x, y]) = notes(
        "# T\none\ntwo\nthree\n",
        "# T\nONE\ntwo\nthree\n",
        "# T\none\ntwo\nTHREE\n",
    );
    let run = |path: &str, l: Hash, rm: Hash, content: &mut crate::iface::ReadContent| {
        let mut history = |ids: &[Hash]| -> Result<Vec<Record>> {
            ids.iter()
                .map(|i| w.0.get(i).cloned().ok_or(Error::NotFound(*i)))
                .collect()
        };
        r.decide(
            path,
            Some(&w.side(l, 1)),
            Some(&w.side(rm, 2)),
            &mut history,
            content,
        )
    };
    let mut read = |root: &Hash, _| {
        content
            .get(root)
            .cloned()
            .map(Some)
            .ok_or(Error::NotFound(*root))
    };
    let d = run("n/plan.md", x, y, &mut read).expect("a decision");
    assert_eq!(
        d.content.as_deref(),
        Some(&b"# T\nONE\ntwo\nTHREE\n"[..]),
        "{d:?}"
    );
    assert!(d.copies.is_empty(), "a merged note needs no copy: {d:?}");
    let Outcome::Merge(m) = &d.outcome else {
        panic!("a merge: {d:?}")
    };
    let mut sorted = [x, y];
    sorted.sort();
    assert_eq!(m.parents, sorted);
    assert_eq!(d.mtime, 2, "neither side's value: the larger mtime");
    let other = run("n/plan.md", y, x, &mut read).expect("a decision");
    assert_eq!(
        (other.outcome, other.content),
        (d.outcome, d.content),
        "symmetric"
    );
    let txt = run("n/plan.txt", x, y, &mut read).expect("a decision");
    assert_eq!(
        txt.content.as_deref(),
        Some(&b"# T\nONE\ntwo\nTHREE\n"[..]),
        "any text file merges"
    );
    let mut binary = |root: &Hash, _| {
        let mut c = content.get(root).cloned().ok_or(Error::NotFound(*root))?;
        c.push(0);
        Ok(Some(c))
    };
    let d = run("n/plan.md", x, y, &mut binary).expect("a decision");
    assert_eq!(
        (d.copies.len(), d.content),
        (1, None),
        "a zero byte: not text"
    );
    let mut large = |_: &Hash, max: u64| -> Result<Option<Vec<u8>>> {
        assert!(max >= 4 << 20, "up to 4 MiB merges (§5.6)");
        Ok(None)
    };
    let d = run("n/plan.md", x, y, &mut large).expect("a decision");
    assert_eq!(
        (d.copies.len(), d.content),
        (1, None),
        "over the size limit: not read, not merged"
    );
    let failing = |root: &Hash, _| -> Result<Option<Vec<u8>>> { Err(Error::Corrupt(*root)) };
    let got = run("n/plan.md", x, y, &mut { failing });
    assert!(
        matches!(got, Err(Error::Corrupt(_))),
        "content errors pass through: {got:?}"
    );
}

/// Only regular files carry an mtime; links, directories and tombstones 0.
fn regular_only(r: &Record, mtime: i64) -> i64 {
    match (r.is_tombstone(), &r.state[..]) {
        (false, [0] | [1]) => mtime,
        _ => 0,
    }
}

/// `<stem>.kide-conflict-<first 12 hex of x><.ext>` on the last component,
/// the stem cut, between characters, until the name has at most 255 bytes.
fn conflict_path(path: &str, x: &Hash) -> String {
    let (dir, name) = path.rsplit_once('/').map_or(("", path), |(d, n)| (d, n));
    let tag = format!("kide-conflict-{}", &to_hex(&x.0)[..12]);
    let (mut stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), &name[i..]),
        _ => (name.to_string(), ""),
    };
    while !stem.is_empty() && stem.len() + 1 + tag.len() + ext.len() > 255 {
        stem.pop();
    }
    let name = format!("{stem}.{tag}{ext}");
    match dir {
        "" => name,
        _ => format!("{dir}/{name}"),
    }
}

/// What a case expects: an explicit decision, or the merge rule over the
/// frontier given.
enum Want {
    Is(Decision),
    Frontier(Vec<Hash>),
}

struct Case {
    name: &'static str,
    path: &'static str,
    world: World,
    local: Option<Side>,
    remote: Option<Side>,
    /// Whether the decision may read history (not for rows 1 to 3).
    reads_history: bool,
    want: Want,
}

fn keep(mtime: i64) -> Want {
    Want::Is(plain(Outcome::Keep, mtime))
}

fn adopt(id: Hash, mtime: i64) -> Want {
    Want::Is(plain(Outcome::Adopt(id), mtime))
}

/// Two ids, the smaller first.
fn ordered(a: Hash, b: Hash) -> (Hash, Hash) {
    if a < b { (a, b) } else { (b, a) }
}

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    let mut w = World::default();
    let reg = Kind::Regular;
    let a = w.live(R1, reg, &[]);
    let b = w.live(R2, reg, &[a]); // an edit of a
    let same = w.live(R1, Kind::Executable, &[a]); // a mode change of a
    let t = w.tombstone(a); // a deletion of a
    let re = w.live(R3, reg, &[t]); // a re-creation after it
    let (x1, x2) = (
        w.live(R2, Kind::Symlink, &[]),
        w.live(R3, Kind::Symlink, &[]),
    );
    let (tl, tr) = (w.tombstone(x1), w.tombstone(x2)); // unrelated deletions
    let (lo, hi) = ordered(w.live(R1, reg, &[x1]), w.live(R2, reg, &[x2]));
    let (elo, ehi) = ordered(w.live(R1, reg, &[x2]), w.live(R1, Kind::Executable, &[x1]));
    let mut case = |name, path, l: Option<Side>, r: Option<Side>, reads, want| {
        out.push(Case {
            name,
            path,
            world: w.clone(),
            local: l,
            remote: r,
            reads_history: reads,
            want,
        })
    };
    let s = |id, m| Some(w.side(id, m));
    case(
        "row 1: R = L, mtime max",
        "f",
        s(a, 300),
        s(a, 900),
        false,
        keep(900),
    );
    case("row 2: remote none", "f", s(a, 300), None, false, keep(300));
    case(
        "row 2: a local tombstone",
        "f",
        s(tl, 0),
        None,
        false,
        keep(0),
    );
    case(
        "row 3: local none",
        "f",
        None,
        s(b, 700),
        false,
        adopt(b, 700),
    );
    case(
        "row 3: a remote tombstone",
        "f",
        None,
        s(t, 0),
        false,
        adopt(t, 0),
    );
    case(
        "fast-forward",
        "f",
        s(a, 300),
        s(b, 700),
        true,
        adopt(b, 700),
    );
    case(
        "fast-forward to a deletion",
        "f",
        s(a, 300),
        s(t, 0),
        true,
        adopt(t, 0),
    );
    case(
        "fast-forward to a re-creation",
        "f",
        s(t, 0),
        s(re, 50),
        true,
        adopt(re, 50),
    );
    case(
        "fast-forward past a deletion",
        "f",
        s(a, 9),
        s(re, 50),
        true,
        adopt(re, 50),
    );
    case("local ahead", "f", s(b, 700), s(a, 300), true, keep(700));
    case(
        "a mode change",
        "f",
        s(a, 300),
        s(same, 700),
        true,
        adopt(same, 700),
    );
    let f = Want::Frontier;
    case(
        "two deletions",
        "f",
        s(tl, 0),
        s(tr, 0),
        true,
        f(vec![tl, tr]),
    );
    case(
        "content beats a remote deletion",
        "f",
        s(lo, 400),
        s(tr, 0),
        true,
        f(vec![lo, tr]),
    );
    case(
        "content beats a local deletion",
        "f",
        s(tl, 0),
        s(hi, 800),
        true,
        f(vec![tl, hi]),
    );
    let long: &'static str = format!("d/{}.md", "ü".repeat(126)).leak();
    for path in [
        "notes.md",
        ".bashrc",
        "archive.tar.gz",
        "src/Makefile",
        "a.d/notes",
        long,
    ] {
        let name = "conflict: the larger id wins, the other is copied";
        case(name, path, s(lo, 400), s(hi, 800), true, f(vec![lo, hi]));
    }
    case(
        "conflict: the local side wins",
        "n.md",
        s(hi, 800),
        s(lo, 400),
        true,
        f(vec![lo, hi]),
    );
    case(
        "conflict of equal content: no copy",
        "f",
        s(elo, 4),
        s(ehi, 8),
        true,
        f(vec![elo, ehi]),
    );

    // Merges of merges: x, y, z live with different content.
    let mut w = World::default();
    let [x, y, z] = [R1, R2, R3].map(|root| w.live(root, reg, &[]));
    let mxy = w.merge_of(&[x, y]);
    let myz = w.merge_of(&[y, z]);
    let edit = w.live(R3, reg, &[mxy]);
    let mut case = |name, l: Option<Side>, r: Option<Side>, want| {
        out.push(Case {
            name,
            path: "m.md",
            world: w.clone(),
            local: l,
            remote: r,
            reads_history: true,
            want,
        })
    };
    let s = |id, m| Some(w.side(id, m));
    let xyz = || Want::Frontier(vec![x, y, z]);
    case("a merge meets a third edit", s(mxy, 500), s(z, 600), xyz());
    case("a third edit meets a merge", s(z, 600), s(mxy, 500), xyz());
    case("two overlapping merges", s(mxy, 500), s(myz, 600), xyz());
    case(
        "a merge against one of its leaves",
        s(mxy, 500),
        s(x, 100),
        Want::Frontier(vec![x, y]),
    );
    case(
        "a leaf against a merge of it",
        s(x, 100),
        s(mxy, 500),
        Want::Frontier(vec![x, y]),
    );
    case(
        "an edit of a merge supersedes its leaves",
        s(edit, 900),
        s(y, 200),
        keep(900),
    );
    out.extend(ordered_cases());
    out
}

/// The first value `make(w, b)` returns for b = 1, 2, …, with the records it
/// added: for cases that need ids in a given order.
fn find<T>(w: &mut World, make: impl Fn(&mut World, u8) -> Option<T>) -> T {
    for b in 1..=255 {
        let mut t = w.clone();
        if let Some(v) = make(&mut t, b) {
            *w = t;
            return v;
        }
    }
    panic!("no records with ids in the order a case needs")
}

/// Cases whose ids must fall in a given order.
fn ordered_cases() -> Vec<Case> {
    let reg = Kind::Regular;
    let root = |b: u8, k: u8| crate::hash::h(&[b, k]);
    let case = |name, w: &World, l: Side, r: Side, f: Vec<Hash>| Case {
        name,
        path: "o.txt",
        world: w.clone(),
        local: Some(l),
        remote: Some(r),
        reads_history: true,
        want: Want::Frontier(f),
    };
    let mut out = Vec::new();
    // x re-creates g's value after deleting g; R merges g and z and shows
    // g's value: both sides hold x's value, so its copy takes the larger mtime.
    let mut w = World::default();
    let (g, x) = find(&mut w, |w, b| {
        let g = w.live(root(b, 0), reg, &[]);
        let t = w.tombstone(g);
        let x = w.live(root(b, 0), reg, &[t]);
        (g > x).then_some((g, x))
    });
    let z = find(&mut w, |w, b| {
        let z = w.live(root(b, 1), reg, &[]);
        (g > z && z > x).then_some(z)
    });
    let gz = w.merge_of(&[g, z]);
    let name = "a copy of a value both sides hold takes the larger mtime";
    out.push(case(name, &w, w.side(x, 300), w.side(gz, 700), vec![x, z]));
    // L merged a link a with b and shows b; R edited b: the merge shows the
    // link, the value of neither side, and a link carries no mtime.
    let mut w = World::default();
    let (a, b, c) = find(&mut w, |w, i| {
        let a = w.live(root(i, 2), Kind::Symlink, &[]);
        let b = w.live(root(i, 3), reg, &[]);
        let c = w.live(root(i, 4), reg, &[b]);
        (b > a && a > c).then_some((a, b, c))
    });
    let ab = w.merge_of(&[a, b]);
    let name = "a link of neither side wins: mtime 0";
    out.push(case(name, &w, w.side(ab, 500), w.side(c, 900), vec![a, c]));
    out
}

/// What a decision results in, whichever side is local: the record and the
/// mtime. Copies may differ between the sides.
#[derive(Debug, PartialEq)]
enum Final {
    Existing(Hash),
    New(Record),
}

fn outcome(w: &World, d: &Decision, local: Option<&Side>, what: &str) -> (Final, i64) {
    let rec = match &d.outcome {
        Outcome::Keep => {
            Final::Existing(local.unwrap_or_else(|| panic!("{what}: Keep of none")).id)
        }
        Outcome::Adopt(id) => Final::Existing(*id),
        Outcome::Merge(m) => {
            let id = m
                .id()
                .unwrap_or_else(|e| panic!("{what}: an invalid merge: {e}"));
            match w.0.contains_key(&id) {
                true => Final::Existing(id),
                false => Final::New(m.clone()),
            }
        }
    };
    (rec, d.mtime)
}

fn decide(
    r: &dyn Resolver,
    w: &World,
    path: &str,
    l: Option<&Side>,
    rm: Option<&Side>,
    reads: bool,
    what: &str,
) -> Result<Decision> {
    let mut history = |ids: &[Hash]| -> Result<Vec<Record>> {
        assert!(reads, "{what}: rows 1 to 3 are decided without history");
        ids.iter()
            .map(|x| w.0.get(x).cloned().ok_or(Error::NotFound(*x)))
            .collect()
    };
    // These records' contents are binary, so no text merge applies (§5.6).
    let mut content = |_: &Hash, _| -> Result<Option<Vec<u8>>> { Ok(Some(vec![0])) };
    r.decide(path, l, rm, &mut history, &mut content)
}

/// Copies in a fixed order: their order is not part of the contract.
fn normalise(mut d: Decision) -> Decision {
    d.copies.sort_by(|a, b| a.path.cmp(&b.path));
    d
}

fn check_case(r: &dyn Resolver, c: &Case) {
    let (l, rm) = (c.local.as_ref(), c.remote.as_ref());
    let want = match &c.want {
        Want::Is(d) => d.clone(),
        Want::Frontier(f) => {
            let (Some(l), Some(rm)) = (l, rm) else {
                panic!("{}: a frontier needs both sides", c.name)
            };
            c.world.decision(c.path, l, rm, f)
        }
    };
    let got = decide(r, &c.world, c.path, l, rm, c.reads_history, c.name)
        .unwrap_or_else(|e| panic!("{}: {e}", c.name));
    assert_eq!(
        normalise(got),
        normalise(want.clone()),
        "{}: wrong decision",
        c.name
    );
    let swapped = decide(r, &c.world, c.path, rm, l, c.reads_history, c.name)
        .unwrap_or_else(|e| panic!("{} (swapped): {e}", c.name));
    assert_eq!(
        outcome(&c.world, &swapped, rm, c.name),
        outcome(&c.world, &want, l, c.name),
        "{}: swapping local and remote changed the result",
        c.name
    );
    assert_eq!(
        normalise(swapped).copies,
        normalise(want).copies,
        "{}: swapping local and remote changed the copies",
        c.name
    );
}

fn errors_pass_through(r: &dyn Resolver) {
    // Two edits of one base: deciding them needs the base's history.
    let mut w = World::default();
    let base = w.live(R3, Kind::Regular, &[]);
    let (a, b) = (
        w.live(R1, Kind::Regular, &[base]),
        w.live(R2, Kind::Regular, &[base]),
    );
    let (a, b) = (w.side(a, 1), w.side(b, 2));
    let errors: [fn() -> Error; 3] = [
        || Error::NotFound(Hash([7; 32])),
        || Error::Unavailable("store unreadable".into()),
        || Error::Corrupt(Hash([8; 32])),
    ];
    for make_err in errors {
        let mut history = |_: &[Hash]| -> Result<Vec<Record>> { Err(make_err()) };
        let got = r.decide("f", Some(&a), Some(&b), &mut history, &mut |_, _| Ok(None));
        let same = match (&got, make_err()) {
            (Err(Error::NotFound(x)), Error::NotFound(y))
            | (Err(Error::Corrupt(x)), Error::Corrupt(y)) => *x == y,
            (Err(Error::Unavailable(_)), Error::Unavailable(_)) => true,
            _ => false,
        };
        assert!(
            same,
            "a history error {:?} came back as {got:?}",
            make_err()
        );
    }
}

/// A random record after `base`: a deletion, or content of a random kind.
fn random_record(w: &mut World, rng: &mut Rng, parents: &[Hash]) -> Hash {
    match (rng.below(4), parents.first()) {
        (0, Some(p)) => w.tombstone(*p),
        (k, _) => {
            let kind = [Kind::Regular, Kind::Executable][k as usize % 2];
            w.live([R1, R2, R3][rng.below(3) as usize], kind, parents)
        }
    }
}

/// Three devices holding three concurrent edits merge them in different
/// pairings; every pairing ends in the same record (§3.2).
fn canonical_merges(r: &dyn Resolver) {
    let mut rng = Rng::new(25);
    for round in 0..30 {
        let mut w = World::default();
        let base = w.live(R1, Kind::Regular, &[]);
        let ids: Vec<Hash> = (0..3)
            .map(|_| random_record(&mut w, &mut rng, &[base]))
            .collect();
        let mut finals = BTreeSet::new();
        for (p, q, s) in [(0, 1, 2), (1, 2, 0), (0, 2, 1)] {
            let what = format!("pairing round {round}");
            let side = |w: &World, x: Hash, m: i64| Some(w.side(x, m));
            let first = decide(
                r,
                &w,
                "f",
                side(&w, ids[p], 1).as_ref(),
                side(&w, ids[q], 2).as_ref(),
                true,
                &what,
            )
            .unwrap_or_else(|e| panic!("{what}: {e}"));
            let mut w2 = w.clone();
            let m = match first.outcome {
                Outcome::Keep => ids[p],
                Outcome::Adopt(id) => id,
                Outcome::Merge(rec) => w2.insert(rec),
            };
            let second = decide(
                r,
                &w2,
                "f",
                side(&w2, m, 3).as_ref(),
                side(&w2, ids[s], 4).as_ref(),
                true,
                &what,
            )
            .unwrap_or_else(|e| panic!("{what}: {e}"));
            let last = match second.outcome {
                Outcome::Keep => m,
                Outcome::Adopt(id) => id,
                Outcome::Merge(rec) => rec.id().expect("a valid merge"),
            };
            finals.insert(last);
        }
        assert_eq!(
            finals.len(),
            1,
            "round {round}: pairings disagree: {finals:?}"
        );
    }
}

fn random_symmetry(r: &dyn Resolver) {
    let mut rng = Rng::new(8);
    for i in 0..500 {
        let mut w = World::default();
        let mut pool = vec![w.live(R1, Kind::Regular, &[])];
        for _ in 0..6 {
            let parents = match rng.below(3) {
                0 => vec![],
                _ => vec![pool[rng.below(pool.len() as u64) as usize]],
            };
            pool.push(random_record(&mut w, &mut rng, &parents));
        }
        let mut pick = || match rng.below(8) {
            0 => None,
            _ => Some(w.side(
                pool[rng.below(pool.len() as u64) as usize],
                rng.below(4) as i64,
            )),
        };
        let (l, rm) = (pick(), pick());
        if l.is_none() && rm.is_none() {
            continue;
        }
        let what = format!("random case {i}: {l:?} / {rm:?}");
        let a = decide(r, &w, "dir/file.txt", l.as_ref(), rm.as_ref(), true, &what)
            .unwrap_or_else(|e| panic!("{what}: {e}"));
        let b = decide(r, &w, "dir/file.txt", rm.as_ref(), l.as_ref(), true, &what)
            .unwrap_or_else(|e| panic!("{what} (swapped): {e}"));
        assert_eq!(
            outcome(&w, &a, l.as_ref(), &what),
            outcome(&w, &b, rm.as_ref(), &what),
            "{what}: not symmetric"
        );
        assert_eq!(
            normalise(a).copies,
            normalise(b).copies,
            "{what}: copies not symmetric"
        );
    }
}

#[cfg(test)]
mod tests;
