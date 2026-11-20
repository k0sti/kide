//! `PackStore`: objects appended to segment files of up to 64 MiB, with an
//! index from id to segment, offset and length, so that small objects do not
//! each take a file and a disk block. The files are described in `disk`.
//!
//! Durability: `sync` flushes the segments, then the index, then records the
//! synced point. Opening for writing trusts the index up to that point and
//! checks every record after it, truncating at the first bad one; so a crash
//! loses at most unsynced objects, never a synced one.

mod disk;
#[cfg(test)]
mod tests;

use crate::error::{Error, Result, invalid};
use crate::file::{read_or_empty, sync_dir, write_atomic};
use crate::hash::{Hash, h};
use crate::iface::ObjectStore;
use crate::log;
use disk::{
    ENTRY, HEADER, Loc, REMOVAL, Segment, decode_entry, decode_point, encode_entry, encode_point,
    encode_record, parse_record, seg_name, segments,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// A new segment starts when the next record would take the newest past this.
pub const SEGMENT_MAX: u64 = 64 << 20;
/// The file whose presence marks a store damaged: a corrupt object was
/// dropped, and objects may lack part of their closure until repaired.
pub const DAMAGED: &str = "damaged";
/// Pending index entries are appended to the file once they reach this size.
const PENDING_MAX: usize = 1 << 20;

#[derive(Default)]
struct State {
    present: HashMap<Hash, Loc>,
    /// Segments holding object records of an id other than its present record.
    stale: HashMap<Hash, Vec<u32>>,
    segs: BTreeMap<u32, Segment>,
    /// Index entries not yet appended to the index file.
    pending: Vec<u8>,
    /// Segments written since the last sync.
    dirty: BTreeSet<u32>,
    /// Whether segments were created since the last sync.
    dir_dirty: bool,
    /// The point last recorded in `synced`: newest segment and its length.
    synced: (u32, u64),
}

impl State {
    /// Takes one record into account; records are applied in record order.
    fn apply(&mut self, id: Hash, loc: Loc) {
        let old = match loc.removal() {
            true => self.present.remove(&id),
            false => self.present.insert(id, loc),
        };
        if let Some(old) = old {
            self.stale.entry(id).or_default().push(old.seg);
            if let Some(s) = self.segs.get_mut(&old.seg) {
                s.live -= old.size();
            }
        }
        if !loc.removal()
            && let Some(s) = self.segs.get_mut(&loc.seg)
        {
            s.live += loc.size();
        }
    }

    fn point(&self) -> (u32, u64) {
        let newest = self.segs.iter().next_back();
        newest.map_or((0, 0), |(n, s)| (*n, s.size))
    }

    /// Whether the record lies within its (existing) segment.
    fn holds(&self, loc: &Loc) -> bool {
        self.segs.get(&loc.seg).is_some_and(|s| loc.end() <= s.size)
    }
}

pub struct PackStore {
    dir: PathBuf,
    max: u64,
    writable: bool,
    /// Whether `sync` forces data to disk; false only in tests.
    fsync: bool,
    state: Mutex<State>,
}

impl PackStore {
    /// Opens the store in `dir` for writing, creating it if needed, and
    /// repairs it. Only the holder of the folder lock may do this.
    pub fn open(dir: impl Into<PathBuf>) -> Result<PackStore> {
        PackStore::open_with(dir.into(), SEGMENT_MAX)
    }

    /// `open` with another segment size, for tests.
    pub fn open_with(dir: PathBuf, max: u64) -> Result<PackStore> {
        fs::create_dir_all(&dir)?;
        let mut st = State {
            segs: segments(&dir, true)?,
            ..State::default()
        };
        let index = read_or_empty(&dir.join("index"))?;
        let mut point = decode_point(&read_or_empty(&dir.join("synced"))?);
        // A synced segment cut below its synced length is checked from its start.
        if st.segs.get(&point.0).is_none_or(|s| s.size < point.1) {
            point.1 = 0;
        }
        let (all, rest) = index.as_chunks::<ENTRY>();
        let mut kept = Vec::with_capacity(index.len());
        let mut changed = !rest.is_empty();
        let mut n = 0;
        for e in all {
            let (id, loc) = decode_entry(e);
            if loc.seg > point.0 || (loc.seg == point.0 && loc.end() > point.1) {
                break;
            }
            n += 1;
            if st.holds(&loc) {
                st.apply(id, loc);
                kept.extend(e);
            } else {
                changed = true;
            }
        }
        changed |= n != all.len();
        let (tail, truncated) = check_tail(&dir, &mut st, point)?;
        changed |= truncated || !tail.is_empty();
        kept.extend(tail);
        st.synced = point;
        let store = PackStore {
            dir,
            max,
            writable: true,
            fsync: true,
            state: Mutex::new(st),
        };
        if changed {
            let mut st = store.state();
            write_atomic(&store.dir.join("index"), &kept, true)?;
            st.dirty = st.segs.range(point.0..).map(|(n, _)| *n).collect();
            st.dir_dirty = true;
            st.synced = (u32::MAX, u64::MAX); // forces a new synced point
            store.sync_locked(&mut st)?;
        }
        Ok(store)
    }

    /// Opens the store without the folder lock: changes nothing, and ignores
    /// an incomplete last index entry and any unindexed tail.
    pub fn open_read_only(dir: impl Into<PathBuf>) -> Result<PackStore> {
        let dir = dir.into();
        let mut st = State {
            segs: segments(&dir, false)?,
            ..State::default()
        };
        for e in read_or_empty(&dir.join("index"))?.as_chunks::<ENTRY>().0 {
            let (id, loc) = decode_entry(e);
            if st.holds(&loc) {
                st.apply(id, loc);
            }
        }
        Ok(PackStore {
            dir,
            max: SEGMENT_MAX,
            writable: false,
            fsync: true,
            state: Mutex::new(st),
        })
    }

    /// The same store with `sync` no longer forcing data to disk: faster
    /// tests that never lose power.
    pub fn without_fsync(mut self) -> PackStore {
        self.fsync = false;
        self
    }

    /// The bytes of `id` as stored: `Corrupt` if they do not hash to it.
    fn read(&self, id: &Hash) -> Result<Vec<u8>> {
        let (loc, file) = {
            let st = self.state();
            let Some(loc) = st.present.get(id).copied() else {
                return Err(Error::NotFound(*id));
            };
            (loc, st.segs.get(&loc.seg).map(|s| s.file.clone()))
        };
        let Some(file) = file else {
            return Err(Error::Corrupt(*id));
        };
        let mut rec = vec![0; loc.size() as usize];
        match crate::os::read_exact_at(&file, &mut rec, loc.off) {
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Err(Error::Corrupt(*id)),
            r => r?,
        }
        if parse_record(&rec).is_none_or(|(got, len, _)| got != *id || len != loc.len) {
            return Err(Error::Corrupt(*id));
        }
        rec.drain(..HEADER);
        Ok(rec)
    }

    /// Drops a corrupt object, so that it counts as absent and a fetch
    /// brings it again (§4.1), and marks the store damaged: objects that
    /// link to it are stored without their whole closure until a repair
    /// walks them. A read-only store only reports it.
    fn drop_corrupt(&self, id: &Hash) -> Error {
        if self.writable {
            let mut st = self.state();
            let dropped = self.append(&mut st, id, None);
            let marked = fs::write(self.dir.join(DAMAGED), id.to_hex());
            if let Err(e) = dropped.and(marked.map_err(Error::from)) {
                log!("dropping corrupt object {id}: {e}");
            }
        }
        Error::Corrupt(*id)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A poisoned lock only means another thread panicked; appends complete
        // before the state changes, so it is intact.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn insert(&self, id: &Hash, bytes: &[u8]) -> Result<()> {
        let mut st = self.state();
        if st.present.contains_key(id) {
            return Ok(());
        }
        self.append(&mut st, id, Some(bytes))
    }

    /// Appends an object record, or a removal record if `bytes` is `None`.
    fn append(&self, st: &mut State, id: &Hash, bytes: Option<&[u8]>) -> Result<()> {
        if !self.writable {
            return invalid("the pack store is open read-only");
        }
        if let Some(b) = bytes.filter(|b| b.len() >= REMOVAL as usize) {
            return invalid(format!("an object of {} bytes", b.len()));
        }
        let rec = encode_record(id, bytes);
        let n = self.segment_for(st, rec.len() as u64)?;
        let Some(seg) = st.segs.get_mut(&n) else {
            return invalid(format!("pack segment {n} vanished"));
        };
        crate::os::write_all_at(&seg.file, &rec, seg.size)?;
        let loc = Loc {
            seg: n,
            off: seg.size,
            len: bytes.map_or(REMOVAL, |b| b.len() as u32),
        };
        seg.size += rec.len() as u64;
        st.dirty.insert(n);
        st.pending.extend(encode_entry(id, loc));
        st.apply(*id, loc);
        if st.pending.len() >= PENDING_MAX {
            self.flush_pending(st)?;
        }
        Ok(())
    }

    /// The segment a record of `size` bytes goes to: the newest, or a new one.
    fn segment_for(&self, st: &mut State, size: u64) -> Result<u32> {
        let newest = st.segs.iter().next_back();
        if let Some((n, s)) = newest
            && (s.size == 0 || s.size + size <= self.max)
        {
            return Ok(*n);
        }
        let n = newest.map_or(1, |(n, _)| n + 1);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.dir.join(seg_name(n)))?;
        st.segs.insert(n, Segment::new(file, 0));
        st.dir_dirty = true;
        Ok(n)
    }

    fn flush_pending(&self, st: &mut State) -> Result<File> {
        let mut index = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("index"))?;
        index.write_all(&st.pending)?;
        st.pending.clear();
        Ok(index)
    }

    /// Segments, then the index, then the synced point.
    fn sync_locked(&self, st: &mut State) -> Result<()> {
        if !self.writable {
            return Ok(());
        }
        for n in std::mem::take(&mut st.dirty) {
            if let Some(s) = st.segs.get(&n).filter(|_| self.fsync) {
                s.file.sync_data()?;
            }
        }
        if std::mem::take(&mut st.dir_dirty) && self.fsync {
            sync_dir(&self.dir)?;
        }
        let point = st.point();
        if st.pending.is_empty() && point == st.synced {
            return Ok(());
        }
        let index = self.flush_pending(st)?;
        if self.fsync {
            index.sync_data()?;
        }
        write_atomic(&self.dir.join("synced"), &encode_point(point), self.fsync)?;
        if self.fsync {
            sync_dir(&self.dir)?;
        }
        st.synced = point;
        Ok(())
    }

    /// Copies the present records of segments less than half live into the
    /// newest one, then deletes those segments.
    fn compact_locked(&self, st: &mut State) -> Result<()> {
        let newest = st.segs.keys().next_back().copied();
        let victims: Vec<u32> = st
            .segs
            .iter()
            .filter(|&(n, s)| Some(*n) != newest && (s.size == 0 || s.live * 2 < s.size))
            .map(|(n, _)| *n)
            .collect();
        if victims.is_empty() {
            return Ok(());
        }
        for &k in &victims {
            self.copy_live(st, k)?;
        }
        self.sync_locked(st)?;
        for k in &victims {
            fs::remove_file(self.dir.join(seg_name(*k)))?;
            st.segs.remove(k);
        }
        st.stale.retain(|_, v| {
            v.retain(|s| !victims.contains(s));
            !v.is_empty()
        });
        let index = read_or_empty(&self.dir.join("index"))?;
        let kept: Vec<u8> = index
            .as_chunks::<ENTRY>()
            .0
            .iter()
            .filter(|e| !victims.contains(&decode_entry(e).1.seg))
            .flatten()
            .copied()
            .collect();
        write_atomic(&self.dir.join("index"), &kept, self.fsync)?;
        if self.fsync {
            sync_dir(&self.dir)?;
        }
        Ok(())
    }

    /// Appends the present records of segment `k` again, and its removals
    /// that an older segment still needs.
    fn copy_live(&self, st: &mut State, k: u32) -> Result<()> {
        let (file, size) = (st.segs[&k].file.clone(), st.segs[&k].size);
        let mut data = vec![0; size as usize];
        crate::os::read_exact_at(&file, &mut data, 0)?;
        let mut off = 0;
        while off < data.len() {
            let Some((id, len, rec)) = parse_record(&data[off..]) else {
                return invalid(format!("pack segment {k} is damaged at {off}"));
            };
            let loc = Loc {
                seg: k,
                off: off as u64,
                len,
            };
            if loc.removal() {
                let needed = !st.present.contains_key(&id)
                    && st.stale.get(&id).is_some_and(|v| v.iter().any(|&s| s < k));
                if needed {
                    self.append(st, &id, None)?;
                }
            } else if st.present.get(&id) == Some(&loc) {
                self.append(st, &id, Some(&data[off + HEADER..off + rec]))?;
            }
            off += rec;
        }
        Ok(())
    }
}

/// Checks every record after `point`, applying and returning the index
/// entries of the good ones. At the first bad record its segment is truncated
/// and every newer segment deleted; the flag says whether that happened.
fn check_tail(dir: &Path, st: &mut State, point: (u32, u64)) -> Result<(Vec<u8>, bool)> {
    let mut entries = Vec::new();
    let nums: Vec<u32> = st.segs.range(point.0..).map(|(n, _)| *n).collect();
    for (i, &n) in nums.iter().enumerate() {
        let start = if n == point.0 { point.1 } else { 0 };
        let (file, size) = (st.segs[&n].file.clone(), st.segs[&n].size);
        let mut data = vec![0; (size - start) as usize];
        crate::os::read_exact_at(&file, &mut data, start)?;
        let mut off = 0;
        while off < data.len() {
            let Some((id, len, rec)) = parse_record(&data[off..]) else {
                let cut = start + off as u64;
                file.set_len(cut)?;
                if let Some(s) = st.segs.get_mut(&n) {
                    s.size = cut;
                }
                for later in &nums[i + 1..] {
                    fs::remove_file(dir.join(seg_name(*later)))?;
                    st.segs.remove(later);
                }
                crate::log!("pack store {}: segment {n} cut at {cut}", dir.display());
                return Ok((entries, true));
            };
            let loc = Loc {
                seg: n,
                off: start + off as u64,
                len,
            };
            entries.extend(encode_entry(&id, loc));
            st.apply(id, loc);
            off += rec;
        }
    }
    Ok((entries, false))
}

impl ObjectStore for PackStore {
    /// Deletes every segment, the index and the synced point.
    fn clear(&self) -> Result<()> {
        let mut st = self.state();
        if !self.writable {
            return invalid("the pack store is open read-only");
        }
        for n in st.segs.keys() {
            fs::remove_file(self.dir.join(seg_name(*n)))?;
        }
        for name in ["index", "synced"] {
            match fs::remove_file(self.dir.join(name)) {
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                r => r?,
            }
        }
        *st = State::default();
        Ok(())
    }

    fn has(&self, id: &Hash) -> Result<bool> {
        Ok(self.state().present.contains_key(id))
    }

    fn get(&self, id: &Hash) -> Result<Vec<u8>> {
        self.read(id).map_err(|e| match e {
            Error::Corrupt(x) => self.drop_corrupt(&x),
            e => e,
        })
    }

    fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let id = h(bytes);
        self.insert(&id, bytes)?;
        Ok(id)
    }

    fn put_known(&self, id: &Hash, bytes: &[u8]) -> Result<()> {
        if h(bytes) != *id {
            return Err(Error::Corrupt(*id));
        }
        self.insert(id, bytes)
    }

    fn remove(&self, id: &Hash) -> Result<()> {
        let mut st = self.state();
        if !st.present.contains_key(id) {
            return Ok(());
        }
        self.append(&mut st, id, None)
    }

    fn ids(&self) -> Result<Vec<Hash>> {
        Ok(self.state().present.keys().copied().collect())
    }

    fn sync(&self) -> Result<()> {
        self.sync_locked(&mut self.state())
    }

    fn compact(&self) -> Result<()> {
        match self.writable {
            true => self.compact_locked(&mut self.state()),
            false => Ok(()),
        }
    }
}
