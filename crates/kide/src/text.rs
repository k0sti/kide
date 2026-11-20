//! The three-way line merge of §5.6 for text files. Each side's changes
//! are hunks against the base, found by a longest common subsequence of
//! lines (Myers' greedy algorithm). Hunks of one side that touch no base
//! line of the other's apply as they are, adjacent ones too; overlapping
//! hunks of both sides take the first side's version of those lines, then
//! the second's, or one version if both made the same change.

/// The longest edit distance, in lines, a diff explores before giving up.
pub(crate) const MAX_EDITS: usize = 4096;

/// Whether `bytes` is text for the merge (§5.6): valid UTF-8 without a
/// zero byte.
pub fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// Merges the changes `a` and `b` made to `base`; `None` if a diff gives up.
/// A missing final newline is the state of the side that changed it.
pub fn merge3(base: &[u8], a: &[u8], b: &[u8]) -> Option<Vec<u8>> {
    if a == base || a == b {
        return Some(b.to_vec());
    }
    if b == base {
        return Some(a.to_vec());
    }
    let ends = |t: &[u8]| t.is_empty() || t.ends_with(b"\n");
    let newline = |t: &[u8]| match ends(t) {
        true => t.to_vec(),
        false => [t, b"\n"].concat(),
    };
    let (o, x, y) = (newline(base), newline(a), newline(b));
    let mut out = merge_lines(&lines(&o), &lines(&x), &lines(&y))?;
    let keep_end = match ends(a) != ends(base) {
        true => ends(a),
        false => ends(b),
    };
    if !keep_end && out.ends_with(b"\n") {
        out.pop();
    }
    Some(out)
}

/// Lines, each with its newline.
pub(crate) fn lines(t: &[u8]) -> Vec<&[u8]> {
    t.split_inclusive(|&c| c == b'\n').collect()
}

/// A change one side made: base lines `o.0..o.1` became its lines
/// `x.0..x.1` (an insertion when the base range is empty).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hunk {
    o: (usize, usize),
    x: (usize, usize),
}

/// The hunks turning `o` into `x`, in order.
fn hunks(o: &[&[u8]], x: &[&[u8]]) -> Option<Vec<Hunk>> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    let end = (o.len(), x.len());
    for (oi, xi) in common(o, x, MAX_EDITS)?.into_iter().chain([end]) {
        if oi > i || xi > j {
            out.push(Hunk {
                o: (i, oi),
                x: (j, xi),
            });
        }
        (i, j) = (oi + 1, xi + 1);
    }
    Some(out)
}

/// Whether two hunks touch the same base lines, or start at the same one.
fn overlap(h: &Hunk, g: &Hunk) -> bool {
    h.o.0 == g.o.0 || (h.o.0 < g.o.1 && g.o.0 < h.o.1)
}

/// The base lines `from..to` with one side's hunks among them applied.
fn apply(o: &[&[u8]], x: &[&[u8]], hs: &[Hunk], (from, to): (usize, usize)) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = from;
    for h in hs {
        out.extend(o[at..h.o.0].concat());
        out.extend(x[h.x.0..h.x.1].concat());
        at = h.o.1;
    }
    out.extend(o[at..to].concat());
    out
}

/// Applies both sides' hunks to the base. Hunks that overlap form a group
/// over the union of their base lines; a group takes the first side's
/// version of those lines, then the second's, or one of them if equal.
fn merge_lines(o: &[&[u8]], a: &[&[u8]], b: &[&[u8]]) -> Option<Vec<u8>> {
    let (ha, hb) = (hunks(o, a)?, hunks(o, b)?);
    let (mut i, mut j, mut at) = (0, 0, 0);
    let mut out = Vec::new();
    while i < ha.len() || j < hb.len() {
        let (h, g) = (ha.get(i), hb.get(j));
        if let (Some(h), Some(g)) = (h, g)
            && overlap(h, g)
        {
            // Grow the group while either side has a hunk overlapping it.
            let mut group = Hunk {
                o: (h.o.0.min(g.o.0), h.o.1.max(g.o.1)),
                x: (0, 0),
            };
            let (i0, j0) = (i, j);
            loop {
                let grows = |k: &Hunk| overlap(k, &group) || k.o.0 < group.o.1;
                if let Some(k) = ha.get(i).filter(|k| grows(k)) {
                    group.o.1 = group.o.1.max(k.o.1);
                    i += 1;
                } else if let Some(k) = hb.get(j).filter(|k| grows(k)) {
                    group.o.1 = group.o.1.max(k.o.1);
                    j += 1;
                } else {
                    break;
                }
            }
            out.extend(o[at..group.o.0].concat());
            let va = apply(o, a, &ha[i0..i], group.o);
            let vb = apply(o, b, &hb[j0..j], group.o);
            if va != vb {
                out.extend(va);
            }
            out.extend(vb);
            at = group.o.1;
            continue;
        }
        // The hunk that starts first; it overlaps nothing of the other side.
        let (k, x) = match (h, g) {
            (Some(h), Some(g)) if g.o.0 < h.o.0 => {
                j += 1;
                (g, b)
            }
            (Some(h), _) => {
                i += 1;
                (h, a)
            }
            (None, Some(g)) => {
                j += 1;
                (g, b)
            }
            (None, None) => break,
        };
        out.extend(o[at..k.o.0].concat());
        out.extend(x[k.x.0..k.x.1].concat());
        at = k.o.1;
    }
    out.extend(o[at..].concat());
    Some(out)
}

/// The matched pairs of a longest common subsequence of `a` and `b`,
/// ascending; `None` if they differ by more than `max` lines.
pub(crate) fn common(a: &[&[u8]], b: &[&[u8]], max: usize) -> Option<Vec<(usize, usize)>> {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let (ra, rb) = (&a[pre..], &b[pre..]);
    let suf = ra
        .iter()
        .rev()
        .zip(rb.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (ma, mb) = (&ra[..ra.len() - suf], &rb[..rb.len() - suf]);
    let mut pairs: Vec<(usize, usize)> = (0..pre).map(|i| (i, i)).collect();
    pairs.extend(
        myers(ma, mb, max)?
            .into_iter()
            .map(|(x, y)| (x + pre, y + pre)),
    );
    let (ea, eb) = (a.len() - suf, b.len() - suf);
    pairs.extend((0..suf).map(|i| (ea + i, eb + i)));
    Some(pairs)
}

/// Myers' greedy diff: the matched pairs of a shortest edit script.
fn myers(a: &[&[u8]], b: &[&[u8]], max: usize) -> Option<Vec<(usize, usize)>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let limit = (n + m).min(max as isize);
    // Diagonals reach -limit-1..=limit+1, so memory follows the edit limit,
    // not the number of lines.
    let off = limit + 1;
    let mut v = vec![0isize; (2 * off + 1) as usize];
    let at = |k: isize| (k + off) as usize;
    // trace[d] holds v[-d..=d] as it was before step d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=limit {
        trace.push(v[at(-d)..=at(d)].to_vec());
        for k in (-d..=d).step_by(2) {
            let down = k == -d || (k != d && v[at(k - 1)] < v[at(k + 1)]);
            let mut x = if down { v[at(k + 1)] } else { v[at(k - 1)] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                (x, y) = (x + 1, y + 1);
            }
            v[at(k)] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m));
            }
        }
    }
    None
}

/// Walks the trace back from (n, m), collecting the diagonal moves.
fn backtrack(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<(usize, usize)> {
    let (mut x, mut y) = (n, m);
    let mut pairs = Vec::new();
    for (d, v) in trace.iter().enumerate().rev() {
        let d = d as isize;
        let get = |k: isize| v[(k + d) as usize];
        let k = x - y;
        let (px, py) = if d == 0 {
            (0, 0)
        } else {
            let down = k == -d || (k != d && get(k - 1) < get(k + 1));
            let pk = if down { k + 1 } else { k - 1 };
            (get(pk), get(pk) - pk)
        };
        while x > px && y > py {
            (x, y) = (x - 1, y - 1);
            pairs.push((x as usize, y as usize));
        }
        (x, y) = (px, py);
    }
    pairs.reverse();
    pairs
}

#[cfg(test)]
mod tests;
