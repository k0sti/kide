//! The pack store's files. A segment `<n:08>.seg` is a sequence of records
//! `id: [u8; 32] | len: u32 | bytes`, where `len = u32::MAX` marks a removal
//! and has no bytes. `index` holds one 48-byte entry per record:
//! `id | segment: u32 | offset: u64 | len: u32`. `synced` holds the newest
//! segment and its length at the last sync: `segment: u32 | length: u64`.

use crate::error::Result;
use crate::hash::{Hash, h};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

/// Id and length field.
pub const HEADER: usize = 36;
/// The length field of a removal record.
pub const REMOVAL: u32 = u32::MAX;
/// One index entry.
pub const ENTRY: usize = 48;

/// Where a record lies, with its length field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub seg: u32,
    pub off: u64,
    pub len: u32,
}

impl Loc {
    pub fn removal(&self) -> bool {
        self.len == REMOVAL
    }

    pub fn size(&self) -> u64 {
        HEADER as u64 + if self.removal() { 0 } else { self.len as u64 }
    }

    pub fn end(&self) -> u64 {
        self.off + self.size()
    }
}

pub fn encode_entry(id: &Hash, loc: Loc) -> [u8; ENTRY] {
    let mut e = [0; ENTRY];
    e[..32].copy_from_slice(&id.0);
    e[32..36].copy_from_slice(&loc.seg.to_le_bytes());
    e[36..44].copy_from_slice(&loc.off.to_le_bytes());
    e[44..].copy_from_slice(&loc.len.to_le_bytes());
    e
}

pub fn decode_entry(e: &[u8; ENTRY]) -> (Hash, Loc) {
    let (mut seg, mut off, mut len) = ([0; 4], [0; 8], [0; 4]);
    seg.copy_from_slice(&e[32..36]);
    off.copy_from_slice(&e[36..44]);
    len.copy_from_slice(&e[44..]);
    let loc = Loc {
        seg: u32::from_le_bytes(seg),
        off: u64::from_le_bytes(off),
        len: u32::from_le_bytes(len),
    };
    (Hash::at(e), loc)
}

/// The record at the start of `b`: (id, length field, record size), or `None`
/// if it is incomplete or its bytes do not hash to its id.
pub fn parse_record(b: &[u8]) -> Option<(Hash, u32, usize)> {
    let len = u32::from_le_bytes(b.get(32..HEADER)?.try_into().ok()?);
    let id = Hash::at(b);
    if len == REMOVAL {
        return Some((id, len, HEADER));
    }
    let end = HEADER + len as usize;
    (h(b.get(HEADER..end)?) == id).then_some((id, len, end))
}

/// A record's bytes: header and object, or header alone for a removal.
pub fn encode_record(id: &Hash, bytes: Option<&[u8]>) -> Vec<u8> {
    let len = bytes.map_or(REMOVAL, |b| b.len() as u32);
    let mut rec = Vec::with_capacity(HEADER + bytes.map_or(0, <[u8]>::len));
    rec.extend(id.0);
    rec.extend(len.to_le_bytes());
    rec.extend(bytes.unwrap_or_default());
    rec
}

pub fn seg_name(n: u32) -> String {
    format!("{n:08}.seg")
}

fn seg_number(name: &str) -> Option<u32> {
    let digits = name.strip_suffix(".seg")?;
    if digits.len() != 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The synced point recorded in `b`, or (0, 0).
pub fn decode_point(b: &[u8]) -> (u32, u64) {
    let seg = b.get(..4).unwrap_or_default().try_into();
    let len = b.get(4..).unwrap_or_default().try_into();
    match (seg, len) {
        (Ok(seg), Ok(len)) => (u32::from_le_bytes(seg), u64::from_le_bytes(len)),
        _ => (0, 0),
    }
}

pub fn encode_point(point: (u32, u64)) -> [u8; 12] {
    let mut b = [0; 12];
    b[..4].copy_from_slice(&point.0.to_le_bytes());
    b[4..].copy_from_slice(&point.1.to_le_bytes());
    b
}

pub struct Segment {
    pub file: Arc<File>,
    pub size: u64,
    /// Bytes of the records of present objects.
    pub live: u64,
}

impl Segment {
    pub fn new(file: File, size: u64) -> Segment {
        Segment {
            file: Arc::new(file),
            size,
            live: 0,
        }
    }
}

/// The segment files in `dir`, by number. A read-only store accepts a
/// missing directory as an empty store.
pub fn segments(dir: &Path, writable: bool) -> Result<BTreeMap<u32, Segment>> {
    let mut segs = BTreeMap::new();
    let entries = match fs::read_dir(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound && !writable => return Ok(segs),
        r => r?,
    };
    for e in entries {
        let name = e?.file_name();
        let Some(n) = name.to_str().and_then(seg_number) else {
            continue;
        };
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open(dir.join(&name))?;
        let size = file.metadata()?.len();
        segs.insert(n, Segment::new(file, size));
    }
    Ok(segs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_entries_round_trip() {
        let id = h(b"object");
        let rec = encode_record(&id, Some(b"object"));
        assert_eq!(parse_record(&rec), Some((id, 6, HEADER + 6)));
        assert_eq!(parse_record(&rec[..rec.len() - 1]), None, "incomplete");
        let mut bad = rec.clone();
        bad[HEADER] ^= 1;
        assert_eq!(parse_record(&bad), None, "wrong bytes");
        let rm = encode_record(&id, None);
        assert_eq!(parse_record(&rm), Some((id, REMOVAL, HEADER)));
        let loc = Loc {
            seg: 3,
            off: 1 << 40,
            len: 7,
        };
        assert_eq!(decode_entry(&encode_entry(&id, loc)), (id, loc));
        assert_eq!(decode_point(&encode_point((5, 99))), (5, 99));
        assert_eq!(decode_point(b"short"), (0, 0));
        assert_eq!(seg_number(&seg_name(42)), Some(42));
        assert_eq!(seg_number("1.seg"), None);
    }
}
