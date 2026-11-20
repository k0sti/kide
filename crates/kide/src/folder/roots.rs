//! `.kide/roots` (§5.4, §5.7): retained earlier roots, one line each, the
//! unix time it was replaced and the root's hex. A torn last line (one
//! without its newline) is ignored, and cut off before the next append so
//! that it never merges with a whole line.

use crate::error::{Error, Result, invalid};
use crate::file::{read_or_empty, write_atomic};
use crate::hash::Hash;
use std::fs::File;
use std::path::Path;

/// Longer than any line: 20 digits, a space, 64 hex digits and a newline.
const TAIL: u64 = 128;

/// The whole lines of the file; an unparsable whole line is `Invalid`.
pub fn read(path: &Path) -> Result<Vec<(u64, Hash)>> {
    let bytes = read_or_empty(path)?;
    let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let Ok(text) = std::str::from_utf8(&bytes[..whole]) else {
        return invalid("roots is not UTF-8");
    };
    let line = |l: &str| {
        let (t, h) = l.split_once(' ')?;
        Some((t.parse().ok()?, Hash::from_hex(h).ok()?))
    };
    let bad = |l: &str| Error::Invalid(format!("roots: bad line {l:?}"));
    text.lines()
        .map(|l| line(l).ok_or_else(|| bad(l)))
        .collect()
}

/// Appends one root, first cutting off a torn last line.
pub fn append(path: &Path, at: u64, root: &Hash, fsync: bool) -> Result<()> {
    let f = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(TAIL);
    let mut tail = vec![0; (len - start) as usize];
    crate::os::read_exact_at(&f, &mut tail, start)?;
    let end = match tail.iter().rposition(|b| *b == b'\n') {
        Some(i) => start + i as u64 + 1,
        None if start == 0 => 0,
        None => len, // no line is this long: left for `read` to reject
    };
    if end < len {
        f.set_len(end)?;
    }
    crate::os::write_all_at(&f, format!("{at} {root}\n").as_bytes(), end)?;
    if fsync {
        f.sync_all()?;
    }
    Ok(())
}

/// Rewrites the file whole without the roots retained before `horizon`;
/// returns those kept and the number dropped, a torn line not counted.
pub fn prune(path: &Path, horizon: u64, fsync: bool) -> Result<(Vec<Hash>, usize)> {
    let all = read(path)?;
    let (kept, dropped): (Vec<_>, Vec<_>) = all.into_iter().partition(|r| r.0 >= horizon);
    let text: String = kept.iter().map(|(t, r)| format!("{t} {r}\n")).collect();
    write_atomic(path, text.as_bytes(), fsync)?;
    Ok((kept.into_iter().map(|r| r.1).collect(), dropped.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::TempDir;
    use std::fs;

    #[test]
    fn a_torn_last_line_is_ignored_and_cut_before_appending() {
        let tmp = TempDir::new("roots");
        let p = tmp.path().join("roots");
        assert!(read(&p).unwrap().is_empty(), "no file");
        let (a, b, c) = (Hash([1; 32]), Hash([2; 32]), Hash([3; 32]));
        append(&p, 10, &a, false).unwrap();
        let torn = format!("20 {b}");
        fs::write(&p, format!("10 {a}\n{}", &torn[..30])).unwrap();
        assert_eq!(read(&p).unwrap(), [(10, a)]);
        append(&p, 30, &c, false).unwrap();
        assert_eq!(read(&p).unwrap(), [(10, a), (30, c)]);
        fs::write(&p, &torn[..5]).unwrap();
        append(&p, 40, &c, false).unwrap();
        assert_eq!(read(&p).unwrap(), [(40, c)], "a torn only line");
        fs::write(&p, "nonsense\n").unwrap();
        assert!(matches!(read(&p), Err(Error::Invalid(_))));
    }

    #[test]
    fn pruning_rewrites_the_file_whole() {
        let tmp = TempDir::new("roots");
        let p = tmp.path().join("roots");
        let roots: Vec<Hash> = (1..=4u8).map(|i| Hash([i; 32])).collect();
        let mut text: String = (0..4)
            .map(|i| format!("{} {}\n", 100 * i, roots[i]))
            .collect();
        text.push_str("400 abc"); // torn
        fs::write(&p, text).unwrap();
        let (kept, dropped) = prune(&p, 200, false).unwrap();
        assert_eq!((kept, dropped), (roots[2..].to_vec(), 2));
        let want = format!("200 {}\n300 {}\n", roots[2], roots[3]);
        assert_eq!(fs::read_to_string(&p).unwrap(), want);
        let names = fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(names, 1, "no temporary file remains");
    }
}
