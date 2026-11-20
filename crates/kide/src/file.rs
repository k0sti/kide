//! File helpers shared by the stores and the folder's local state (§5.4).

use crate::error::Result;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

/// Replaces `path` whole via `.<name>.kide-tmp` and a rename, so a reader
/// sees the old or the new bytes. `fsync` false skips the flush (tests).
pub fn write_atomic(path: &Path, bytes: &[u8], fsync: bool) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.kide-tmp"));
    let written = File::create(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        if fsync { f.sync_all() } else { Ok(()) }
    });
    let renamed = written.and_then(|()| fs::rename(&tmp, path));
    if renamed.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(renamed?)
}

/// Makes the directory's entries durable.
pub fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(any(unix, windows))]
    crate::os::sync_dir(dir)?;
    #[cfg(not(any(unix, windows)))]
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// The file's bytes, or none if it does not exist.
pub fn read_or_empty(path: &Path) -> Result<Vec<u8>> {
    match fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        r => Ok(r?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::TempDir;

    #[test]
    fn atomic_write_replaces_and_leaves_no_temporary() {
        let tmp = TempDir::new("file");
        let p = tmp.path().join("HEAD");
        assert!(read_or_empty(&p).unwrap().is_empty());
        write_atomic(&p, b"one", false).unwrap();
        write_atomic(&p, b"two", true).unwrap();
        assert_eq!(read_or_empty(&p).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(tmp.path()).unwrap().flatten().collect();
        assert_eq!(names.len(), 1, "a temporary file remains");
        sync_dir(tmp.path()).unwrap();
    }
}
