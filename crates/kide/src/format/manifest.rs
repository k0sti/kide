//! The folder profile's manifest (§5.1) and version state (§5.2).

use super::node::{Root, build_with, chunk_entry_with};
use super::path::{MAX_PATH, check_path, is_dir_path};
use super::record::Record;
use super::{MAX, Params, Sink, gear};
use crate::error::{Result, invalid};
use crate::hash::Hash;
use std::collections::HashSet;

/// The bytes of an entry after its path: `0x00`, version, size, mtime.
const TAIL: usize = 1 + 32 + 8 + 8;
/// The longest entry: 4,145 bytes.
pub(crate) const MAX_ENTRY: usize = MAX_PATH + TAIL;
/// The longest manifest chunk: a decided cut moves to the end of its entry.
pub const MAX_MANIFEST_CHUNK: usize = MAX + MAX_ENTRY - 1;

/// The kind of a live record, its one-byte state (§5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Regular = 0,
    Executable = 1,
    Symlink = 2,
    Directory = 3,
}

impl Kind {
    pub fn state(self) -> Vec<u8> {
        vec![self as u8]
    }

    /// The kind of `r`, or `None` for a tombstone.
    pub fn of(r: &Record) -> Result<Option<Kind>> {
        if super::change::Change::decode(&r.change)?.is_some() && r.parents.len() != 1 {
            return invalid("a folder operation record has exactly one parent");
        }
        if r.is_tombstone() {
            return Ok(None);
        }
        Ok(Some(match r.state[..] {
            [0] => Kind::Regular,
            [1] => Kind::Executable,
            [2] => Kind::Symlink,
            [3] => Kind::Directory,
            _ => return invalid("folder state is one byte, 0 to 3"),
        }))
    }
}

/// One manifest entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathEntry {
    pub path: String,
    pub version: Hash,
    pub size: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime: i64,
}

impl PathEntry {
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend(self.path.as_bytes());
        out.push(0);
        out.extend(self.version.0);
        out.extend(self.size.to_le_bytes());
        out.extend(self.mtime.to_le_bytes());
    }

    /// Decodes the entry at the start of `b`; returns it and its length.
    /// The path is checked for UTF-8 only.
    fn decode(b: &[u8]) -> Result<(PathEntry, usize)> {
        let Some(z) = b.iter().position(|&c| c == 0) else {
            return invalid("partial manifest entry");
        };
        let Some(tail) = b[z..].first_chunk::<TAIL>() else {
            return invalid("partial manifest entry");
        };
        let Ok(path) = String::from_utf8(b[..z].to_vec()) else {
            return invalid("path is not UTF-8");
        };
        let word = |i: usize| {
            let mut w = [0; 8];
            w.copy_from_slice(&tail[i..i + 8]);
            w
        };
        let e = PathEntry {
            path,
            version: Hash::at(&tail[1..]),
            size: u64::from_le_bytes(word(33)),
            mtime: i64::from_le_bytes(word(41)),
        };
        Ok((e, z + TAIL))
    }
}

/// Decodes and validates one manifest chunk on its own (§5.1): complete
/// entries with valid, strictly ascending paths.
pub fn decode_chunk(b: &[u8]) -> Result<Vec<PathEntry>> {
    if b.len() > MAX_MANIFEST_CHUNK {
        return invalid(format!("manifest chunk of {} bytes", b.len()));
    }
    let (mut entries, mut rest) = (Vec::new(), b);
    while !rest.is_empty() {
        let (e, n) = PathEntry::decode(rest)?;
        check_path(&e.path)?;
        entries.push(e);
        rest = &rest[n..];
    }
    check_ascending(&entries)?;
    Ok(entries)
}

fn check_ascending(entries: &[PathEntry]) -> Result<()> {
    match entries.windows(2).find(|w| w[0].path >= w[1].path) {
        Some(w) => invalid(format!("{:?} does not precede {:?}", w[0].path, w[1].path)),
        None => Ok(()),
    }
}

/// The manifest's cut points (§5.1, the spec's function): a cut decided after
/// byte i moves to the end of the entry holding byte i. `entry_ends` are the
/// end offsets of the entries, the last one `data.len()`.
#[cfg(test)]
pub(crate) fn manifest_cut_points(data: &[u8], entry_ends: &[usize]) -> Vec<usize> {
    manifest_cut_points_with(data, entry_ends, &Params::SPEC)
}

/// `manifest_cut_points` under other parameters (§9).
pub(crate) fn manifest_cut_points_with(
    data: &[u8],
    entry_ends: &[usize],
    p: &Params,
) -> Vec<usize> {
    let g = gear();
    let (mut cuts, mut start, mut h, mut e) = (Vec::new(), 0usize, 0u64, 0usize);
    for (i, &b) in data.iter().enumerate() {
        h = (h << 1).wrapping_add(g[b as usize]);
        if i < start {
            continue; // rest of an entry already cut after
        }
        let size = i + 1 - start;
        if size >= p.min && (p.natural(h) || size == p.max) {
            while entry_ends[e] <= i {
                e += 1; // the last end is data.len() > i
            }
            cuts.push(entry_ends[e]);
            start = entry_ends[e];
        }
    }
    if start < data.len() {
        cuts.push(data.len());
    }
    cuts
}

/// The structural rules of a whole manifest that the entries alone decide:
/// valid paths, strictly ascending; an empty-directory path (live or
/// tombstone) has size and mtime 0.
pub(crate) fn check_entries(entries: &[PathEntry]) -> Result<()> {
    for e in entries {
        check_path(&e.path)?;
        if is_dir_path(&e.path) && (e.size, e.mtime) != (0, 0) {
            return invalid(format!("{:?} has a size or mtime", e.path));
        }
    }
    check_ascending(entries)
}

/// What a whole-manifest check needs to know of an entry's record: `None`
/// for a tombstone, else its kind and the length of its content.
pub type LiveKind = Option<(Kind, u64)>;

/// Validates a whole manifest (§5.1): also the rules that need records
/// (kinds, sizes, a link's zero mtime, nesting). `live` resolves an entry's version
/// record, failing with `NotFound` if the record or its root is missing.
pub fn validate(
    entries: &[PathEntry],
    live: &mut dyn FnMut(&Hash) -> Result<LiveKind>,
) -> Result<()> {
    check_entries(entries)?;
    let (mut files, mut dirs) = (HashSet::new(), HashSet::new());
    for e in entries {
        let Some((kind, len)) = live(&e.version)? else {
            if (e.size, e.mtime) != (0, 0) {
                return invalid(format!("tombstone {:?} has a size or mtime", e.path));
            }
            continue;
        };
        let dir = kind == Kind::Directory;
        if dir != is_dir_path(&e.path) {
            return invalid(format!("{:?} disagrees with its record's kind", e.path));
        }
        if e.size != len {
            return invalid(format!("{:?}: size {} of content {len}", e.path, e.size));
        }
        if kind == Kind::Symlink && e.mtime != 0 {
            return invalid(format!("symbolic link {:?} has an mtime", e.path));
        }
        if dir { &mut dirs } else { &mut files }.insert(e.path.as_str());
    }
    for p in files.iter().chain(&dirs) {
        for (j, _) in p.match_indices('/') {
            let below_file = files.contains(&p[..j]);
            let below_dir = j + 1 < p.len() && dirs.contains(&p[..=j]);
            if below_file || below_dir {
                return invalid(format!("{p:?} lies below a live file or empty directory"));
            }
        }
    }
    Ok(())
}

/// Stores a manifest's content (§5.1): checks the entries, which must be
/// sorted, and passes every chunk and node to `sink`.
pub fn build_manifest(entries: &[PathEntry], sink: &mut Sink) -> Result<Root> {
    build_manifest_with(entries, &Params::SPEC, sink)
}

/// `build_manifest` under other parameters (§9).
pub fn build_manifest_with(entries: &[PathEntry], p: &Params, sink: &mut Sink) -> Result<Root> {
    check_entries(entries)?;
    let (mut data, mut ends) = (Vec::new(), Vec::with_capacity(entries.len()));
    for e in entries {
        e.encode_into(&mut data);
        ends.push(data.len());
    }
    let mut start = 0;
    let mut chunks = Vec::new();
    for end in manifest_cut_points_with(&data, &ends, p) {
        let chunk = &data[start..end];
        let e = chunk_entry_with(chunk, p);
        sink(&e.id, chunk)?;
        chunks.push(e);
        start = end;
    }
    build_with(chunks, p, sink)
}

#[cfg(test)]
mod tests;
