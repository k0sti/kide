//! `OpfsStore`: an object store in the browser's origin private file system,
//! for a dedicated worker, the only place with synchronous file access.
//! Opening is asynchronous; every `ObjectStore` call after it is not.
//!
//! A store is a directory with two files, `objects-0` and `objects-1`. The
//! current one starts with its generation (`u64`, little-endian, never 0) and
//! holds records as the pack store does: `id: [u8; 32] | len: u32 | bytes`,
//! where `len = u32::MAX` marks a removal. Opening reads the valid file of
//! the higher generation, rebuilds the index and cuts the file at the first
//! record that is incomplete or does not hash to its id, so a crash loses at
//! most what was not synced. `compact` writes the live records to the other
//! file and only then its header, so a crash keeps the old file current.

use crate::js::{DirectoryHandle, FileHandle, SyncHandle, describe, object, storage_root};
use kide::error::{Error, Result};
use kide::hash::{Hash, h};
use kide::iface::ObjectStore;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

#[cfg(target_feature = "atomics")]
compile_error!("OpfsStore holds JavaScript objects and assumes a single thread");

const FILES: [&str; 2] = ["objects-0", "objects-1"];
/// The generation.
const HEAD: u64 = 8;
/// Id and length field.
const HEADER: u64 = 36;
const REMOVAL: u32 = u32::MAX;

fn io(what: &str, e: &JsValue) -> Error {
    Error::Io(std::io::Error::other(format!("{what}: {}", describe(e))))
}

/// Where an object's bytes lie in the current file.
#[derive(Clone, Copy)]
struct Loc {
    off: u64,
    len: u32,
}

struct State {
    files: [SyncHandle; 2],
    cur: usize,
    generation: u64,
    end: u64,
    present: HashMap<Hash, Loc>,
    /// Bytes of removed or removal records, which `compact` drops.
    garbage: u64,
}

pub struct OpfsStore(Mutex<State>);

// SAFETY: without the atomics target feature (checked above), a wasm32
// program has one thread, so the JavaScript handles are only ever used from it.
unsafe impl Send for OpfsStore {}
unsafe impl Sync for OpfsStore {}

impl OpfsStore {
    /// Opens the store in the directory `name` at the root of the origin
    /// private file system, creating it if needed. Only one store may have a
    /// directory open at a time.
    pub async fn open(name: &str) -> Result<OpfsStore> {
        let root = storage_root().await.map_err(|e| io("storage", &e))?;
        let create = object("create", true.into());
        let dir = JsFuture::from(root.get_directory_handle(name, &create))
            .await
            .map_err(|e| io(name, &e))?;
        let dir: DirectoryHandle = dir.unchecked_into();
        let files = [
            open_file(&dir, FILES[0]).await?,
            open_file(&dir, FILES[1]).await?,
        ];
        Ok(OpfsStore(Mutex::new(State::load(files)?)))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A poisoned lock only means a caller panicked; the state is intact.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for OpfsStore {
    fn drop(&mut self) {
        self.state().files.iter().for_each(SyncHandle::close);
    }
}

/// A synchronous handle on the file `name` in `dir`, created if needed.
async fn open_file(dir: &DirectoryHandle, name: &str) -> Result<SyncHandle> {
    let create = object("create", true.into());
    let file = JsFuture::from(dir.get_file_handle(name, &create))
        .await
        .map_err(|e| io(name, &e))?;
    let file: FileHandle = file.unchecked_into();
    let handle = JsFuture::from(file.create_sync_access_handle())
        .await
        .map_err(|e| io(name, &e))?;
    Ok(handle.unchecked_into())
}

fn at(off: u64) -> JsValue {
    object("at", (off as f64).into())
}

fn read(f: &SyncHandle, off: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0; len];
    let n = f.read(&mut buf, &at(off)).map_err(|e| io("read", &e))?;
    buf.truncate(n as usize);
    Ok(buf)
}

fn write(f: &SyncHandle, off: u64, bytes: &[u8]) -> Result<()> {
    let n = f.write(bytes, &at(off)).map_err(|e| io("write", &e))?;
    match n as usize == bytes.len() {
        true => Ok(()),
        false => Err(Error::Io(std::io::Error::other("short write"))),
    }
}

fn size(f: &SyncHandle) -> Result<u64> {
    Ok(f.get_size().map_err(|e| io("size", &e))? as u64)
}

fn truncate(f: &SyncHandle, len: u64) -> Result<()> {
    f.truncate(len as f64).map_err(|e| io("truncate", &e))
}

fn flush(f: &SyncHandle) -> Result<()> {
    f.flush().map_err(|e| io("flush", &e))
}

/// The generation a file starts with; 0 if none.
fn generation(f: &SyncHandle) -> Result<u64> {
    let head = read(f, 0, HEAD as usize)?;
    Ok(head.try_into().map_or(0, u64::from_le_bytes))
}

fn record(id: &Hash, bytes: Option<&[u8]>) -> Vec<u8> {
    let len = bytes.map_or(REMOVAL, |b| b.len() as u32);
    let mut r = Vec::with_capacity(HEADER as usize + bytes.map_or(0, <[u8]>::len));
    r.extend_from_slice(&id.0);
    r.extend_from_slice(&len.to_le_bytes());
    r.extend_from_slice(bytes.unwrap_or_default());
    r
}

impl State {
    fn load(files: [SyncHandle; 2]) -> Result<State> {
        let gens = [generation(&files[0])?, generation(&files[1])?];
        let cur = usize::from(gens[1] > gens[0]);
        let mut s = State {
            cur,
            generation: gens[cur],
            end: HEAD,
            present: HashMap::new(),
            garbage: 0,
            files,
        };
        if s.generation == 0 {
            s.generation = 1;
            truncate(s.file(), 0)?;
            write(s.file(), 0, &1u64.to_le_bytes())?;
        } else {
            s.scan()?;
        }
        truncate(&s.files[1 - cur], 0)?; // an interrupted compaction
        Ok(s)
    }

    fn file(&self) -> &SyncHandle {
        &self.files[self.cur]
    }

    /// Reads every record of the current file, up to the first bad one.
    fn scan(&mut self) -> Result<()> {
        let len = size(self.file())?;
        let all = read(self.file(), 0, len as usize)?;
        let mut off = HEAD;
        while let Some((id, len, size)) = parse(&all[off as usize..]) {
            match len {
                REMOVAL => {
                    if let Some(old) = self.present.remove(&id) {
                        self.garbage += HEADER + old.len as u64;
                    }
                    self.garbage += HEADER;
                }
                len => {
                    let old = self.present.insert(id, Loc { off, len });
                    self.garbage += old.map_or(0, |o| HEADER + o.len as u64);
                }
            }
            off += size;
        }
        self.end = off;
        if off < len {
            truncate(self.file(), off)?;
        }
        Ok(())
    }

    fn append(&mut self, id: &Hash, bytes: Option<&[u8]>) -> Result<Loc> {
        let r = record(id, bytes);
        write(self.file(), self.end, &r)?;
        let loc = Loc {
            off: self.end,
            len: bytes.map_or(REMOVAL, |b| b.len() as u32),
        };
        self.end += r.len() as u64;
        Ok(loc)
    }

    fn get(&self, id: &Hash) -> Result<Vec<u8>> {
        let loc = self.present.get(id).ok_or(Error::NotFound(*id))?;
        let bytes = read(self.file(), loc.off + HEADER, loc.len as usize)?;
        match h(&bytes) == *id {
            true => Ok(bytes),
            false => Err(Error::Corrupt(*id)),
        }
    }

    fn put(&mut self, id: &Hash, bytes: &[u8]) -> Result<()> {
        if !self.present.contains_key(id) {
            let loc = self.append(id, Some(bytes))?;
            self.present.insert(*id, loc);
        }
        Ok(())
    }

    /// Copies the live records to the other file, then makes it current.
    fn compact(&mut self) -> Result<()> {
        if self.garbage == 0 {
            return Ok(());
        }
        let other = 1 - self.cur;
        let new = &self.files[other];
        truncate(new, 0)?;
        write(new, 0, &0u64.to_le_bytes())?;
        let (mut end, mut moved) = (HEAD, HashMap::new());
        for (id, loc) in &self.present {
            let r = read(self.file(), loc.off, (HEADER + loc.len as u64) as usize)?;
            write(new, end, &r)?;
            moved.insert(
                *id,
                Loc {
                    off: end,
                    len: loc.len,
                },
            );
            end += r.len() as u64;
        }
        flush(new)?;
        write(new, 0, &(self.generation + 1).to_le_bytes())?;
        flush(new)?;
        truncate(self.file(), 0)?;
        (self.cur, self.generation, self.end) = (other, self.generation + 1, end);
        (self.present, self.garbage) = (moved, 0);
        Ok(())
    }
}

/// The record at the start of `b`: (id, length field, size), or `None` if it
/// is incomplete or its bytes do not hash to its id.
fn parse(b: &[u8]) -> Option<(Hash, u32, u64)> {
    let id = Hash(b.get(..32)?.try_into().ok()?);
    let len = u32::from_le_bytes(b.get(32..HEADER as usize)?.try_into().ok()?);
    if len == REMOVAL {
        return Some((id, len, HEADER));
    }
    let end = HEADER as usize + len as usize;
    (h(b.get(HEADER as usize..end)?) == id).then_some((id, len, end as u64))
}

impl ObjectStore for OpfsStore {
    fn has(&self, id: &Hash) -> Result<bool> {
        Ok(self.state().present.contains_key(id))
    }

    fn get(&self, id: &Hash) -> Result<Vec<u8>> {
        self.state().get(id)
    }

    fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let id = h(bytes);
        self.state().put(&id, bytes)?;
        Ok(id)
    }

    fn put_known(&self, id: &Hash, bytes: &[u8]) -> Result<()> {
        if h(bytes) != *id {
            return Err(Error::Corrupt(*id));
        }
        self.state().put(id, bytes)
    }

    fn remove(&self, id: &Hash) -> Result<()> {
        let mut s = self.state();
        if let Some(old) = s.present.remove(id) {
            s.append(id, None)?;
            s.garbage += 2 * HEADER + old.len as u64;
        }
        Ok(())
    }

    fn ids(&self) -> Result<Vec<Hash>> {
        Ok(self.state().present.keys().copied().collect())
    }

    fn sync(&self) -> Result<()> {
        flush(self.state().file())
    }

    fn compact(&self) -> Result<()> {
        self.state().compact()
    }
}

#[cfg(test)]
mod tests;
