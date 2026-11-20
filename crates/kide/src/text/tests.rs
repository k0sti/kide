use super::*;

fn m(o: &str, a: &str, b: &str) -> Option<String> {
    merge3(o.as_bytes(), a.as_bytes(), b.as_bytes()).map(|t| String::from_utf8(t).unwrap())
}

#[test]
fn edits_in_different_places_both_apply() {
    let o = "# Title\none\ntwo\nthree\nfour\n";
    let a = "# Title\nONE\ntwo\nthree\nfour\n";
    let b = "# Title\none\ntwo\nthree\nFOUR\nfive\n";
    assert_eq!(
        m(o, a, b).unwrap(),
        "# Title\nONE\ntwo\nthree\nFOUR\nfive\n"
    );
}

#[test]
fn one_side_unchanged_gives_the_other() {
    let (o, a) = ("x\ny\n", "x\ny\nz\n");
    assert_eq!(m(o, a, o).unwrap(), a);
    assert_eq!(m(o, o, a).unwrap(), a);
}

#[test]
fn the_same_edit_on_both_sides_appears_once() {
    let (o, a) = ("x\ny\n", "x\nY\n");
    assert_eq!(m(o, a, a).unwrap(), a);
}

#[test]
fn overlapping_edits_keep_both_first_side_first() {
    let o = "a\nb\nc\n";
    let x = "a\nB from x\nc\n";
    let y = "a\nB from y\nc\n";
    assert_eq!(m(o, x, y).unwrap(), "a\nB from x\nB from y\nc\n");
}

#[test]
fn deletions_and_insertions_at_the_ends() {
    let o = "1\n2\n3\n4\n";
    let a = "0\n1\n2\n3\n4\n"; // insert at start
    let b = "1\n2\n3\n"; // delete the end
    assert_eq!(m(o, a, b).unwrap(), "0\n1\n2\n3\n");
}

#[test]
fn a_last_line_without_newline_is_a_line() {
    let o = "a\nb";
    let a = "A\nb";
    let b = "a\nb\nc";
    // b changed the last line ("b" became "b\n", then "c"): not a's region.
    assert_eq!(m(o, a, b).unwrap(), "A\nb\nc");
}

#[test]
fn empty_inputs() {
    assert_eq!(m("", "", "").unwrap(), "");
    assert_eq!(m("", "a\n", "").unwrap(), "a\n");
    assert_eq!(m("x\n", "", "x\n").unwrap(), "");
}

#[test]
fn diff_finds_a_longest_common_subsequence() {
    let l = |s: &'static str| lines(s.as_bytes());
    let (a, b) = (l("a\nb\nc\nd\ne\n"), l("b\nx\nd\ne\ny\n"));
    let pairs = common(&a, &b, 100).unwrap();
    assert_eq!(pairs, [(1, 0), (3, 2), (4, 3)]);
}

#[test]
fn too_different_inputs_give_up() {
    let a: String = (0..300).map(|i| format!("a{i}\n")).collect();
    let b: String = (0..300).map(|i| format!("b{i}\n")).collect();
    assert!(common(&lines(a.as_bytes()), &lines(b.as_bytes()), 100).is_none());
    assert!(
        m(&a, &b, &a).is_some(),
        "one side unchanged needs no diff limit"
    );
}

#[test]
fn many_small_edits_in_a_long_note() {
    let o: Vec<String> = (0..2000).map(|i| format!("line {i}\n")).collect();
    let (mut a, mut b) = (o.clone(), o.clone());
    for i in (0..2000).step_by(10) {
        a[i] = format!("a {i}\n");
        b[i + 5] = format!("b {i}\n");
    }
    let got = m(&o.concat(), &a.concat(), &b.concat()).unwrap();
    let mut want = o.clone();
    for i in (0..2000).step_by(10) {
        want[i] = format!("a {i}\n");
        want[i + 5] = format!("b {i}\n");
    }
    assert_eq!(got, want.concat());
}

#[test]
fn adjacent_edits_both_apply() {
    let o = "1\n2\n3\n4\n";
    assert_eq!(
        m(o, "1\n2a\n3\n4\n", "1\n2\n3b\n4\n").unwrap(),
        "1\n2a\n3b\n4\n"
    );
    // An insertion right after the other side's changed line.
    assert_eq!(
        m(o, "1\n2a\n3\n4\n", "1\n2\nnew\n3\n4\n").unwrap(),
        "1\n2a\nnew\n3\n4\n"
    );
}

#[test]
fn random_disjoint_line_edits_all_apply() {
    let mut rng = crate::conformance::Rng::new(11);
    for round in 0..300 {
        let n = 1 + rng.below(30) as usize;
        let o: Vec<String> = (0..n).map(|i| format!("o{i}\n")).collect();
        let (mut a, mut b, mut want) = (o.clone(), o.clone(), o.clone());
        for i in 0..n {
            match rng.below(3) {
                0 => {
                    a[i] = format!("a{round}.{i}\n");
                    want[i] = a[i].clone();
                }
                1 => {
                    b[i] = format!("b{round}.{i}\n");
                    want[i] = b[i].clone();
                }
                _ => {}
            }
        }
        let got = m(&o.concat(), &a.concat(), &b.concat()).unwrap();
        assert_eq!(got, want.concat(), "round {round}");
        let swapped = m(&o.concat(), &b.concat(), &a.concat()).unwrap();
        assert_eq!(swapped, want.concat(), "either order, round {round}");
    }
}

/// The length of a longest common subsequence, by dynamic programming.
fn lcs_len(a: &[&[u8]], b: &[&[u8]]) -> usize {
    let mut t = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            t[i][j] = match a[i] == b[j] {
                true => t[i + 1][j + 1] + 1,
                false => t[i + 1][j].max(t[i][j + 1]),
            };
        }
    }
    t[0][0]
}

#[test]
fn random_diffs_are_longest_common_subsequences() {
    let mut rng = crate::conformance::Rng::new(7);
    let alphabet: [&[u8]; 4] = [b"a\n", b"b\n", b"c\n", b"d\n"];
    let text = |rng: &mut crate::conformance::Rng| -> Vec<&[u8]> {
        let n = rng.below(12) as usize;
        (0..n).map(|_| alphabet[rng.below(4) as usize]).collect()
    };
    for _ in 0..500 {
        let (a, b) = (text(&mut rng), text(&mut rng));
        let pairs = common(&a, &b, MAX_EDITS).unwrap();
        assert_eq!(pairs.len(), lcs_len(&a, &b), "{a:?} {b:?}");
        for w in pairs.windows(2) {
            assert!(w[0].0 < w[1].0 && w[0].1 < w[1].1, "ascending");
        }
        assert!(
            pairs.iter().all(|&(x, y)| a[x] == b[y]),
            "matched lines are equal"
        );
    }
}
