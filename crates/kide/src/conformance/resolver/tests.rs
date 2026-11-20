use super::*;
use crate::iface::{History, ReadContent};
use crate::resolve::DecisionTable;

#[test]
fn conflict_paths() {
    let x = Hash::from_hex(&format!("3fa2b1c90d4e{}", "0".repeat(52))).unwrap();
    let cases = [
        ("notes.md", "notes.kide-conflict-3fa2b1c90d4e.md"),
        (".bashrc", ".bashrc.kide-conflict-3fa2b1c90d4e"),
        (
            "archive.tar.gz",
            "archive.tar.kide-conflict-3fa2b1c90d4e.gz",
        ),
        ("Makefile", "Makefile.kide-conflict-3fa2b1c90d4e"),
        ("a.d/notes", "a.d/notes.kide-conflict-3fa2b1c90d4e"),
        ("a/.env.local", "a/.env.kide-conflict-3fa2b1c90d4e.local"),
    ];
    for (path, want) in cases {
        assert_eq!(conflict_path(path, &x), want);
    }
    let long = format!("{}.md", "ü".repeat(126));
    let want = format!("{}.kide-conflict-3fa2b1c90d4e.md", "ü".repeat(112));
    assert_eq!(
        conflict_path(&long, &x),
        want,
        "254 bytes, not half a ü more"
    );
}

#[test]
fn the_oracle_follows_the_mtime_table() {
    let mut w = World::default();
    let (a, b) = ordered(
        w.live(R1, Kind::Regular, &[]),
        w.live(R2, Kind::Regular, &[]),
    );
    let d = w.decision("f", &w.side(a, 4), &w.side(b, 9), &[a, b]);
    assert_eq!(d.mtime, 9, "the value of the remote side");
    assert_eq!(d.copies.len(), 1);
    assert_eq!(d.copies[0].mtime, 4, "a copy takes its side's mtime");
    let d = w.decision("f", &w.side(b, 9), &w.side(a, 4), &[a, b]);
    assert_eq!(d.mtime, 9, "the value of the local side");
    let [both, link] = &ordered_cases()[..] else {
        panic!("two ordered cases")
    };
    let decide = |c: &Case| {
        let (Some(l), Some(r), Want::Frontier(f)) = (&c.local, &c.remote, &c.want) else {
            panic!("{}: a frontier case", c.name)
        };
        c.world.decision(c.path, l, r, f)
    };
    let d = decide(both);
    assert_eq!((d.mtime, d.copies.len()), (700, 1), "{d:?}");
    assert_eq!(d.copies[0].mtime, 700, "both sides hold the copy's value");
    let d = decide(link);
    assert_eq!((d.mtime, d.copies.len()), (0, 1), "{d:?}");
    assert_eq!(d.copies[0].mtime, 900, "the copy's value is R's");
}

#[test]
fn copies_only_with_a_new_merge() {
    let mut w = World::default();
    let [x, y] = [R1, R2].map(|root| w.live(root, Kind::Regular, &[]));
    let m = w.merge_of(&[x, y]);
    let d = w.decision("f", &w.side(x, 1), &w.side(m, 2), &[x, y]);
    assert_eq!(d, plain(Outcome::Adopt(m), 2), "the merge's maker copied");
    let z = w.live(R3, Kind::Regular, &[]);
    let d = w.decision("f", &w.side(z, 1), &w.side(m, 2), &[x, y, z]);
    let loser = if w.winner(&[x, y]) == x { y } else { x };
    let copied: Vec<_> = d.copies.iter().map(|c| c.record.parents[0]).collect();
    assert!(!copied.contains(&loser), "{loser} lost the merge in R");
}

/// A throwaway resolver: the frontier by breadth-first search over
/// `history`, then the suite's own rule; notes go to `DecisionTable`, as the
/// suite checks their merges against fixed texts. `max_mtime` breaks the
/// table.
struct Reference {
    max_mtime: bool,
}

fn leaves(s: &Side) -> Vec<Hash> {
    match s.record.is_merge() {
        true => s.record.parents.clone(),
        false => vec![s.id],
    }
}

/// Whether `x` is reachable from `from` by following parents.
fn reachable(x: &Hash, from: &Hash, history: &mut History) -> Result<bool> {
    let (mut todo, mut seen) = (vec![*from], BTreeSet::new());
    while !todo.is_empty() {
        let mut next = Vec::new();
        for r in history(&todo)? {
            next.extend(r.parents.into_iter().filter(|p| seen.insert(*p)));
        }
        if next.contains(x) {
            return Ok(true);
        }
        todo = next;
    }
    Ok(false)
}

impl Resolver for Reference {
    fn decide(
        &self,
        path: &str,
        local: Option<&Side>,
        remote: Option<&Side>,
        history: &mut History,
        content: &mut ReadContent,
    ) -> Result<Decision> {
        // Text merges are the table's own rule (§5.6); this reference checks
        // the rest.
        let table = DecisionTable.decide(path, local, remote, history, content)?;
        if table.content.is_some() {
            return Ok(table);
        }
        let plain = |outcome, mtime| Decision {
            outcome,
            mtime,
            copies: vec![],
            content: None,
        };
        let (l, r) = match (local, remote) {
            (Some(l), Some(r)) if l.id == r.id => {
                return Ok(plain(Outcome::Keep, l.mtime.max(r.mtime)));
            }
            (Some(l), None) => return Ok(plain(Outcome::Keep, l.mtime)),
            (None, Some(r)) => return Ok(plain(Outcome::Adopt(r.id), r.mtime)),
            (Some(l), Some(r)) => (l, r),
            (None, None) => return Err(Error::Invalid("no side".into())),
        };
        let mut all = leaves(l);
        all.extend(leaves(r).into_iter().filter(|x| !leaves(l).contains(x)));
        let mut f = Vec::new();
        for x in &all {
            let mut below = false;
            for y in all.iter().filter(|y| *y != x) {
                below |= reachable(x, y, history)?;
            }
            if !below {
                f.push(*x);
            }
        }
        match f[..] {
            [x] if x == l.id => return Ok(plain(Outcome::Keep, l.mtime)),
            [x] if x == r.id => return Ok(plain(Outcome::Adopt(r.id), r.mtime)),
            _ => {}
        }
        let mut w = World::default();
        for (id, rec) in f.iter().zip(history(&f)?) {
            w.0.insert(*id, rec);
        }
        let mut d = w.decision(path, l, r, &f);
        if self.max_mtime {
            d.mtime = l.mtime.max(r.mtime);
        }
        Ok(d)
    }
}

#[test]
fn a_reference_resolver_passes() {
    resolver(&|| Box::new(Reference { max_mtime: false }));
}

#[test]
#[should_panic(expected = "wrong decision")]
fn a_resolver_that_ignores_the_mtime_table_fails() {
    resolver(&|| Box::new(Reference { max_mtime: true }));
}
