//! Waking the daemon when the folder changes (§5.8).

use crate::heads::Wake;
use kide::error::Result;
use kide::folder::Folder;
use kide::folder::watch::watch;
use std::sync::mpsc::Sender;

/// Watches a folder's directory for the daemon: every change sends
/// `Wake::Scan`; changes in its state in `.kide`, and in ignored
/// directories nothing below which is synced (§5.8), do not.
pub fn watch_folder(folder: &Folder, wake: Sender<Wake>) -> Result<()> {
    let config = folder.config.clone();
    let skip = move |rel: &str| rel == ".kide" || !config.rules.looks_below(rel);
    watch(&folder.root, skip, move || {
        // No receiver: the daemon has stopped; nothing to wake.
        let _ = wake.send(Wake::Scan);
    })
}
