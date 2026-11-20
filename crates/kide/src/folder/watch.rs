//! Noticing changes in a folder as they happen (§5.8), through `notify`
//! (inotify on Linux, FSEvents on macOS, ReadDirectoryChangesW on Windows):
//! the folder is watched recursively, and every batch of changes outside
//! the `skip` directories (its state in `.kide`, ignored paths) calls
//! `changed`. Nothing here decides what changed: the daemon scans, and
//! still scans every `scan_interval` in case an event was lost.

use crate::error::{Error, Result};
use crate::log;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::path::{Component, Path};
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

/// How long a batch of events gathers before `changed` is called.
const BATCH: Duration = Duration::from_millis(30);

/// Watches `root` on a thread of its own, calling `changed` after every
/// batch of events; `skip` names relative directory paths whose changes
/// do not count. Fails if the platform's watcher is unavailable.
pub fn watch(
    root: &Path,
    skip: impl Fn(&str) -> bool + Send + 'static,
    changed: impl Fn() + Send + 'static,
) -> Result<()> {
    let (tx, rx) = channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |e| {
        let _ = tx.send(e);
    })
    .map_err(|e| Error::Unavailable(format!("watch: {e}")))?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|e| Error::Unavailable(format!("watch {}: {e}", root.display())))?;
    let root = root.to_path_buf();
    thread::spawn(move || {
        let _keep = watcher; // watching ends when it is dropped
        let counts = |e: &notify::Result<Event>| match e {
            Ok(e) => {
                !matches!(e.kind, EventKind::Access(_))
                    && e.paths.iter().any(|p| !skipped(&root, p, &skip))
            }
            Err(err) => {
                log!("watch: {err}; the next scan finds the changes");
                true
            }
        };
        while let Ok(e) = rx.recv() {
            let mut any = counts(&e);
            while let Ok(e) = rx.recv_timeout(BATCH) {
                any |= counts(&e);
            }
            if any {
                changed();
            }
        }
    });
    Ok(())
}

/// Whether `path` is in a skipped directory (or is one), or outside `root`.
fn skipped(root: &Path, path: &Path, skip: &impl Fn(&str) -> bool) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return true;
    };
    let mut dir = String::new();
    for c in rel.components() {
        let Component::Normal(name) = c else { continue };
        if !dir.is_empty() {
            dir.push('/');
        }
        dir.push_str(&name.to_string_lossy());
        if skip(&dir) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::TempDir;
    use std::fs;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    fn watched(root: &Path) -> Receiver<()> {
        let (tx, rx) = channel();
        let skip = |rel: &str| rel == ".kide" || rel == "ignored";
        watch(root, skip, move || {
            let _ = tx.send(());
        })
        .unwrap();
        rx
    }

    fn wakes(rx: &Receiver<()>) -> bool {
        let woke = rx.recv_timeout(Duration::from_secs(2)).is_ok();
        while rx.recv_timeout(Duration::from_millis(100)).is_ok() {}
        woke
    }

    #[test]
    fn writes_renames_and_new_directories_wake_it() {
        let tmp = TempDir::new("watch");
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        let rx = watched(tmp.path());
        fs::write(tmp.path().join("a/b/note.md"), b"x").unwrap();
        assert!(wakes(&rx), "a write in a directory present at the start");
        fs::rename(
            tmp.path().join("a/b/note.md"),
            tmp.path().join("a/moved.md"),
        )
        .unwrap();
        assert!(wakes(&rx), "a rename");
        fs::create_dir_all(tmp.path().join("new/deep")).unwrap();
        wakes(&rx);
        fs::write(tmp.path().join("new/deep/x.md"), b"y").unwrap();
        assert!(wakes(&rx), "a write in a directory made after the start");
        fs::remove_file(tmp.path().join("new/deep/x.md")).unwrap();
        assert!(wakes(&rx), "a deletion");
    }

    #[test]
    fn skipped_directories_stay_quiet() {
        let tmp = TempDir::new("watch-skip");
        for d in [".kide/objects", "ignored/x"] {
            fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        let rx = watched(tmp.path());
        fs::write(tmp.path().join(".kide/objects/pack"), b"x").unwrap();
        fs::write(tmp.path().join("ignored/x/workspace.json"), b"{}").unwrap();
        assert!(!wakes(&rx), "nothing watched there");
    }
}
