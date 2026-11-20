//! `.kide/index` (§5.4): every manifest entry with the stat cache of its
//! file, rewritten whole. One record per path:
//!
//! `state \t version \t size \t mtime \t disk-mtime \t inode \t checked \t path \0`
//!
//! `state` is `-` for a tombstone, else the kind's state byte (§5.2); the
//! three disk fields are `-` when nothing on disk is recorded.

use crate::error::{Error, Result, invalid};
use crate::file::{read_or_empty, write_atomic};
use crate::format::manifest::{Kind, PathEntry};
use crate::format::path::check_path;
use crate::hash::Hash;
use crate::iface::{self, Checked, Meta};
use std::collections::BTreeMap;
use std::path::Path;

/// One path: its manifest entry, and what was last seen of it on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The kind of the current record; `None` for a tombstone.
    pub kind: Option<Kind>,
    pub version: Hash,
    /// The length of the content.
    pub size: u64,
    /// The synchronised mtime: what the manifest carries.
    pub mtime: i64,
    /// The file as last checked; `None` if nothing on disk is recorded.
    pub disk: Option<Disk>,
}

/// The stat cache of a live entry; kind and size are the entry's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Disk {
    pub mtime: i64,
    pub inode: u64,
    /// When the metadata was read, before the content was.
    pub at: i64,
}

impl Entry {
    pub fn tombstone(version: Hash) -> Entry {
        Entry {
            kind: None,
            version,
            size: 0,
            mtime: 0,
            disk: None,
        }
    }

    /// A live entry for content observed on disk as `m` at time `at`.
    pub fn observed(version: Hash, m: &Meta, mtime: i64, at: i64) -> Entry {
        Entry {
            kind: Some(m.kind),
            version,
            size: m.size,
            mtime,
            disk: Some(Disk {
                mtime: m.mtime,
                inode: m.inode,
                at,
            }),
        }
    }

    pub fn is_live(&self) -> bool {
        self.kind.is_some()
    }

    /// The entry as a change source sees it.
    pub fn checked(&self) -> Option<Checked> {
        let (kind, d) = (self.kind?, self.disk?);
        let meta = Meta {
            kind,
            size: self.size,
            mtime: d.mtime,
            inode: d.inode,
        };
        Some(Checked { meta, at: d.at })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index(pub BTreeMap<String, Entry>);

impl Index {
    /// What a change source reads: every entry with a file on record.
    pub fn checked(&self) -> iface::CheckedIndex {
        let with_disk = self
            .0
            .iter()
            .filter_map(|(p, e)| Some((p.clone(), e.checked()?)));
        with_disk.collect()
    }

    /// The manifest entries, in path order (§5.1).
    pub fn manifest(&self) -> Vec<PathEntry> {
        let entry = |(path, e): (&String, &Entry)| PathEntry {
            path: path.clone(),
            version: e.version,
            size: e.size,
            mtime: e.mtime,
        };
        self.0.iter().map(entry).collect()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, e) in &self.0 {
            let state = e.kind.map_or("-".into(), |k| (k as u8).to_string());
            let disk = e.disk.map_or("-\t-\t-".into(), |d| {
                format!("{}\t{}\t{}", d.mtime, d.inode, d.at)
            });
            let rec = format!(
                "{state}\t{}\t{}\t{}\t{disk}\t{path}\0",
                e.version, e.size, e.mtime
            );
            out.extend(rec.as_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Index> {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return invalid("the index is not UTF-8");
        };
        let mut ix = Index::default();
        for rec in text.split_terminator('\0') {
            let (path, e) = decode_record(rec)?;
            if ix.0.insert(path.to_string(), e).is_some() {
                return invalid(format!("index: {path:?} appears twice"));
            }
        }
        Ok(ix)
    }

    /// Reads the index; an absent file is an empty index.
    pub fn load(path: &Path) -> Result<Index> {
        Index::decode(&read_or_empty(path)?)
    }

    /// Rewrites the index whole, via a temporary file and a rename.
    pub fn save(&self, path: &Path, fsync: bool) -> Result<()> {
        write_atomic(path, &self.encode(), fsync)
    }
}

fn decode_record(rec: &str) -> Result<(&str, Entry)> {
    let f: Vec<&str> = rec.splitn(8, '\t').collect();
    let [state, version, size, mtime, dmtime, inode, at, path] = f[..] else {
        return invalid(format!("index record with {} fields", f.len()));
    };
    let bad = |what: &str| Error::Invalid(format!("index record for {path:?}: bad {what}"));
    check_path(path).map_err(|_| bad("path"))?;
    let kind = match state {
        "-" => None,
        "0" => Some(Kind::Regular),
        "1" => Some(Kind::Executable),
        "2" => Some(Kind::Symlink),
        "3" => Some(Kind::Directory),
        _ => return Err(bad("state")),
    };
    let disk = match (dmtime, inode, at) {
        ("-", "-", "-") => None,
        _ => Some(Disk {
            mtime: dmtime.parse().map_err(|_| bad("disk mtime"))?,
            inode: inode.parse().map_err(|_| bad("inode"))?,
            at: at.parse().map_err(|_| bad("checked time"))?,
        }),
    };
    let e = Entry {
        kind,
        version: Hash::from_hex(version).map_err(|_| bad("version"))?,
        size: size.parse().map_err(|_| bad("size"))?,
        mtime: mtime.parse().map_err(|_| bad("mtime"))?,
        disk,
    };
    if kind.is_none() && (e.size, e.mtime, e.disk) != (0, 0, None) {
        return Err(bad("tombstone"));
    }
    Ok((path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::TempDir;

    fn sample() -> Index {
        let m = Meta {
            kind: Kind::Executable,
            size: 12_345,
            mtime: -7,
            inode: 99,
        };
        let mut ix = Index::default();
        let live = Entry::observed(Hash([0xab; 32]), &m, 1_761_134_400_000_000_001, 5);
        ix.0.insert("dir/with\ttab and space.sh".into(), live);
        ix.0.insert("gone".into(), Entry::tombstone(Hash([1; 32])));
        let held = Entry {
            kind: Some(Kind::Directory),
            disk: None,
            ..Entry::tombstone(Hash([2; 32]))
        };
        ix.0.insert("Grüße/".into(), held);
        ix
    }

    #[test]
    fn round_trip() {
        let ix = sample();
        assert_eq!(Index::decode(&ix.encode()).unwrap(), ix);
        assert_eq!(Index::decode(b"").unwrap(), Index::default());
        let text = String::from_utf8(ix.encode()).unwrap();
        let tomb = format!("-\t{}\t0\t0\t-\t-\t-\tgone\0", "01".repeat(32));
        assert!(text.contains(&tomb), "{text:?}");
    }

    #[test]
    fn checked_holds_entries_with_a_file() {
        let c = sample().checked();
        assert_eq!(c.keys().collect::<Vec<_>>(), ["dir/with\ttab and space.sh"]);
        let got = c["dir/with\ttab and space.sh"];
        assert_eq!(
            (got.meta.size, got.meta.mtime, got.meta.inode, got.at),
            (12_345, -7, 99, 5)
        );
    }

    #[test]
    fn decode_rejects_damage() {
        let v = "00".repeat(32);
        let bad = [
            format!("x\t{v}\t0\t0\t-\t-\t-\tp\0"),
            "0\tnothex\t0\t0\t-\t-\t-\tp\0".to_string(),
            format!("0\t{v}\t1\t2\0"),
            format!("0\t{v}\tx\t0\t-\t-\t-\tp\0"),
            format!("0\t{v}\t0\t0\t1\t-\t-\tp\0"),
            format!("-\t{v}\t1\t0\t-\t-\t-\tp\0"),
            format!("0\t{v}\t0\t0\t-\t-\t-\t../p\0"),
            format!("0\t{v}\t0\t0\t-\t-\t-\tp\0").repeat(2),
        ];
        for b in bad {
            assert!(Index::decode(b.as_bytes()).is_err(), "{b:?}");
        }
    }

    #[test]
    fn save_and_load() {
        let tmp = TempDir::new("index");
        let p = tmp.path().join("index");
        assert_eq!(Index::load(&p).unwrap(), Index::default());
        sample().save(&p, false).unwrap();
        assert_eq!(Index::load(&p).unwrap(), sample());
    }
}
