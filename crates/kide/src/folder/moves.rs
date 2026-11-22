//! Explicit in-space moves (§5.2): one disk rename and one published HEAD.
use super::{
    Folder,
    index::{Entry, Index},
};
use crate::error::{Error, Result, invalid};
use crate::file::{sync_dir, write_atomic};
use crate::format::{change::Change, manifest::build_manifest, path::check_path, record::Record};
use crate::hash::Hash;
use crate::iface::{ObjectStore, Workspace};
use crate::load;
use std::collections::BTreeMap;
use std::fs;

const INTENT: &str = "move-intent";

/// Refuses unscanned disk state, rather than moving or overwriting it.
/// Content is checked too: unchanged stat metadata alone is not proof.
pub(crate) fn scanned(folder: &Folder, ws: &dyn Workspace) -> Result<()> {
    let disk: BTreeMap<_, _> = ws
        .list()?
        .into_iter()
        .filter(|(p, _)| !folder.config.ignores(p))
        .collect();
    let live: BTreeMap<_, _> = folder
        .index
        .0
        .iter()
        .filter(|(p, e)| e.is_live() && e.disk.is_some() && !folder.config.ignores(p))
        .collect();
    if disk.len() != live.len() {
        return invalid("scan local changes before this operation");
    }
    for (p, e) in live {
        let Some(m) = disk.get(p) else {
            return invalid("scan local changes before this operation");
        };
        if e.checked().is_none_or(|c| c.meta != *m) {
            return invalid("unscanned or held entry");
        }
        let r = load::record(&*folder.store, &e.version)?;
        let root = super::scan::store_content(&*folder.store, &mut *ws.open(p)?)?;
        if root.id != r.root || ws.stat(p)? != Some(*m) {
            return invalid("source changed since the scan");
        }
    }
    Ok(())
}

fn under(path: &str, operand: &str) -> bool {
    path == operand || (operand.ends_with('/') && path.starts_with(operand))
}

/// A scanned file/link or directory prefix, moved explicitly. Destination
/// live content is refused; deleted destination history is retained.
/// A directory operand must end in `/`. No cross-folder moves or inference.
pub fn move_path(folder: &mut Folder, ws: &dyn Workspace, from: &str, to: &str) -> Result<Hash> {
    folder.writable()?;
    if folder.config.mode == super::config::Mode::Send {
        return invalid("a send folder does not write into the workspace");
    }
    let intent = prepare(folder, ws, from, to)?;
    let before = folder.index.clone();
    let old = folder.head;
    persist(folder, &intent)?;
    let result = finish(folder, ws, intent);
    if result.is_err()
        && folder.head == old
        && folder.index == before
        && scanned(folder, ws).is_ok()
    {
        // A refused rename that left every scanned file untouched needs no
        // recovery. Preserve the intent for any partial or ambiguous change.
        fs::remove_file(folder.kide().join(INTENT))?;
        if folder.fsync {
            sync_dir(&folder.kide())?;
        }
    }
    result.map(|()| folder.head)
}

struct Intent {
    old: Hash,
    next: Hash,
    from: String,
    to: String,
    index: Index,
}

fn prepare(folder: &Folder, ws: &dyn Workspace, from: &str, to: &str) -> Result<Intent> {
    let change = Change::Move {
        from: from.into(),
        to: to.into(),
    }
    .encode()?;
    if from.ends_with('/') && (to.starts_with(from) || from.starts_with(to)) {
        return invalid("move operands overlap");
    }
    scanned(folder, ws)?;
    if folder.config.ignores(from) || folder.config.ignores(to) {
        return invalid("cannot move an ignored path");
    }
    let disk = ws.list()?;
    if disk
        .iter()
        .any(|(p, _)| under(p, to) || p.trim_end_matches('/') == to.trim_end_matches('/'))
    {
        return invalid("move destination exists on disk");
    }
    // An ignored descendant must not be silently moved without lineage.
    if disk
        .iter()
        .any(|(p, _)| under(p, from) && folder.config.ignores(p))
    {
        return invalid("move source contains ignored files");
    }
    let entries: Vec<_> = folder
        .index
        .0
        .iter()
        .filter(|(p, e)| under(p, from) && e.is_live())
        .map(|(p, e)| (p.clone(), *e))
        .collect();
    if entries.is_empty() {
        return invalid("move source has no live entries");
    }
    let store = &*folder.store;
    let mut next = folder.index.clone();
    for (path, e) in entries {
        if e.disk.is_none() {
            return invalid("move source is held");
        }
        let dest = format!("{to}{}", &path[from.len()..]);
        check_path(&dest)?;
        if folder.config.ignores(&dest) {
            return invalid("move destination is ignored");
        }
        let previous = next.0.get(&dest).copied();
        if previous.is_some_and(|e| e.is_live()) {
            return invalid("move destination has live history");
        }
        let mut record = load::record(store, &e.version)?;
        let mut parents = vec![e.version];
        if let Some(old) = previous {
            parents = vec![non_merge(store, e.version)?, non_merge(store, old.version)?];
            parents.sort();
            parents.dedup();
        }
        record.parents = parents;
        if record.is_merge() {
            // The resolver flattens merge records. Keep the explicit move as
            // a single-parent child so future reconciliation retains it.
            record.change.clear();
            let joined = store.put(&record.encode()?)?;
            record.parents = vec![joined];
        }
        record.change = change.clone();
        let moved = store.put(&record.encode()?)?;
        let mut tomb = Record::tombstone(vec![e.version]);
        tomb.change = change.clone();
        next.0
            .insert(path, Entry::tombstone(store.put(&tomb.encode()?)?));
        next.0.insert(
            dest,
            Entry {
                version: moved,
                ..e
            },
        );
    }
    // Empty-directory entries stop being live when the move puts content
    // below them, just as in a scan or reconciliation (§5.1, §5.6).
    let parents: Vec<_> = next
        .0
        .iter()
        .filter(|(p, e)| {
            e.kind == Some(crate::format::manifest::Kind::Directory)
                && to.starts_with(p.as_str())
                && p.as_str() != to
        })
        .map(|(p, e)| (p.clone(), e.version))
        .collect();
    for (path, version) in parents {
        let tomb = store.put(&Record::tombstone(vec![version]).encode()?)?;
        next.0.insert(path, Entry::tombstone(tomb));
    }
    let paths = next.0.iter().filter(|(_, e)| e.is_live());
    let (held, _) = crate::names::held(
        paths.map(|(p, e)| (p.as_str(), e.disk.is_some())),
        &crate::names::Platform::current(),
    );
    if held.iter().any(|p| under(p, to)) {
        return invalid("move destination cannot be represented");
    }
    let root = build_manifest(&next.manifest(), &mut |id, b| store.put_known(id, b))?;
    load::validate_manifest(store, &root.id)?;
    Ok(Intent {
        old: folder.head,
        next: root.id,
        from: from.into(),
        to: to.into(),
        index: next,
    })
}

// Explicit operations can have two parents, but no merge under a merge (§3.1).
fn non_merge(store: &dyn ObjectStore, id: Hash) -> Result<Hash> {
    let mut r = load::record(store, &id)?;
    if !r.is_merge() {
        return Ok(id);
    }
    r.parents = vec![id];
    r.change.clear();
    store.put(&r.encode()?)
}

fn persist(folder: &Folder, i: &Intent) -> Result<()> {
    folder.store.sync()?;
    let mut bytes = format!("{}\n{}\n{}\0{}\0", i.old, i.next, i.from, i.to).into_bytes();
    bytes.extend(i.index.encode());
    let mut checked = crate::hash::h(&bytes).0.to_vec();
    checked.extend(bytes);
    write_atomic(&folder.kide().join(INTENT), &checked, folder.fsync)?;
    if folder.fsync {
        sync_dir(&folder.kide())?;
    }
    Ok(())
}

fn decode(bytes: &[u8]) -> Result<Intent> {
    let Some(checksum) = bytes.first_chunk::<32>() else {
        return invalid("short move intent checksum");
    };
    let expected = Hash(*checksum);
    let bytes = &bytes[32..];
    if crate::hash::h(bytes) != expected {
        return Err(Error::Corrupt(expected));
    }
    let mut fields = bytes.splitn(3, |b| *b == b'\n');
    let mut hash = || -> Result<Hash> {
        let b = fields
            .next()
            .ok_or_else(|| Error::Invalid("incomplete move intent".into()))?;
        Hash::from_hex(
            std::str::from_utf8(b).map_err(|_| Error::Invalid("move intent hash".into()))?,
        )
    };
    let (old, next) = (hash()?, hash()?);
    let rest = fields
        .next()
        .ok_or_else(|| Error::Invalid("incomplete move intent".into()))?;
    let mut rest = rest.splitn(3, |b| *b == 0);
    let mut path = || -> Result<String> {
        let b = rest
            .next()
            .ok_or_else(|| Error::Invalid("incomplete move operands".into()))?;
        let p = std::str::from_utf8(b).map_err(|_| Error::Invalid("move operand UTF-8".into()))?;
        check_path(p)?;
        Ok(p.into())
    };
    let (from, to) = (path()?, path()?);
    let index = Index::decode(
        rest.next()
            .ok_or_else(|| Error::Invalid("missing move index".into()))?,
    )?;
    Ok(Intent {
        old,
        next,
        from,
        to,
        index,
    })
}

fn finish(folder: &mut Folder, ws: &dyn Workspace, i: Intent) -> Result<()> {
    if folder.lock.is_none() {
        return invalid("move recovery requires the folder lock");
    }
    if folder.head != i.old && folder.head != i.next {
        return invalid("move intent does not match HEAD");
    }
    // Rebuild rather than trusting a damaged local intent's target hash.
    let root = build_manifest(&i.index.manifest(), &mut |id, b| {
        folder.store.put_known(id, b)
    })?;
    if root.id != i.next {
        return invalid("move intent index does not match target");
    }
    load::validate_manifest(&*folder.store, &i.next)?;
    if folder.head == i.old {
        let disk = ws.list()?;
        let exists = |p: &str| disk.iter().any(|(q, _)| under(q, p));
        match (exists(&i.from), exists(&i.to)) {
            (true, false) => {
                scanned(folder, ws)?;
                ws.rename(&i.from, &i.to)?;
            }
            (false, true) => {} // rename completed before the interruption
            _ => return invalid("ambiguous interrupted move; both operands exist or neither does"),
        }
    }
    folder.index = i.index;
    folder
        .index
        .save(&folder.kide().join("index"), folder.fsync)?;
    if folder.head != i.next {
        folder.publish_head(i.next)?;
    }
    fs::remove_file(folder.kide().join(INTENT))?;
    if folder.fsync {
        sync_dir(&folder.kide())?;
    }
    Ok(())
}

/// Called under the folder lock before any new changes. No intent is ignored.
pub(crate) fn recover(folder: &mut Folder, ws: &dyn Workspace) -> Result<()> {
    let bytes = match fs::read(folder.kide().join(INTENT)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        r => r?,
    };
    let intent = decode(&bytes)?;
    finish(folder, ws, intent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::Rescan;
    use crate::conformance::TempDir;
    use crate::names::Platform;

    #[test]
    fn interrupted_moves_finish_at_each_publication_boundary() {
        for stage in 0..4 {
            let tmp = TempDir::new("move-recovery");
            Folder::init(tmp.path()).unwrap();
            let mut f = Folder::open_unsynced(tmp.path()).unwrap();
            let ws = f.workspace();
            ws.write(
                "old/nested/a",
                crate::format::manifest::Kind::Regular,
                &mut &b"kept"[..],
                1,
            )
            .unwrap();
            super::super::scan::scan(&mut f, &ws, &mut Rescan, &Platform::current()).unwrap();
            let i = prepare(&f, &ws, "old/", "new/").unwrap();
            let expected = i.next;
            persist(&f, &i).unwrap();
            if stage >= 1 {
                ws.rename(&i.from, &i.to).unwrap();
            }
            if stage >= 2 {
                f.index = i.index.clone();
                f.index.save(&f.kide().join("index"), f.fsync).unwrap();
            }
            if stage >= 3 {
                f.publish_head(i.next).unwrap();
            }
            let published = Folder::open_read_only(tmp.path()).unwrap().head;
            assert_eq!(published, if stage >= 3 { expected } else { i.old });
            drop(f);
            let f = Folder::open_unsynced(tmp.path()).unwrap();
            assert_eq!(f.head, expected, "stage {stage}");
            assert_eq!(fs::read(tmp.path().join("new/nested/a")).unwrap(), b"kept");
            assert!(!tmp.path().join("old").exists());
            assert!(!f.kide().join(INTENT).exists());
        }
    }

    #[test]
    fn corrupted_intents_fail_before_any_disk_rename() {
        let tmp = TempDir::new("move-corrupt-intent");
        Folder::init(tmp.path()).unwrap();
        let mut f = Folder::open_unsynced(tmp.path()).unwrap();
        let ws = f.workspace();
        ws.write(
            "a",
            crate::format::manifest::Kind::Regular,
            &mut &b"kept"[..],
            1,
        )
        .unwrap();
        super::super::scan::scan(&mut f, &ws, &mut Rescan, &Platform::current()).unwrap();
        let i = prepare(&f, &ws, "a", "b").unwrap();
        persist(&f, &i).unwrap();
        let path = f.kide().join(INTENT);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(path, bytes).unwrap();
        drop(f);
        assert!(matches!(
            Folder::open_unsynced(tmp.path()),
            Err(Error::Corrupt(_))
        ));
        assert_eq!(fs::read(tmp.path().join("a")).unwrap(), b"kept");
        assert!(!tmp.path().join("b").exists());
    }

    #[test]
    fn ambiguous_intents_are_not_guessed_away() {
        let tmp = TempDir::new("move-ambiguous");
        Folder::init(tmp.path()).unwrap();
        let mut f = Folder::open_unsynced(tmp.path()).unwrap();
        let ws = f.workspace();
        ws.write(
            "a",
            crate::format::manifest::Kind::Regular,
            &mut &b"kept"[..],
            1,
        )
        .unwrap();
        super::super::scan::scan(&mut f, &ws, &mut Rescan, &Platform::current()).unwrap();
        let i = prepare(&f, &ws, "a", "b").unwrap();
        persist(&f, &i).unwrap();
        fs::write(tmp.path().join("b"), b"external").unwrap();
        assert!(
            f.set_head(i.next).is_err(),
            "a pending intent blocks unrelated publication"
        );
        assert!(super::super::scan::scan(&mut f, &ws, &mut Rescan, &Platform::current()).is_err());
        drop(f);
        assert!(matches!(
            Folder::open_unsynced(tmp.path()),
            Err(Error::Invalid(_))
        ));
        assert_eq!(fs::read(tmp.path().join("a")).unwrap(), b"kept");
        assert_eq!(fs::read(tmp.path().join("b")).unwrap(), b"external");
    }
}
