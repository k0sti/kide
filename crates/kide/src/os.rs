//! What the file system code needs from the platform, on unix and Windows:
//! positional reads and writes, a file's identity, the executable bit and
//! symbolic links. On Windows a file has no inode here (0: changes are
//! told by size and mtime), no executable bit, and a symbolic link needs
//! the right to make one (Developer Mode or an administrator).

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

/// Reads exactly `buf.len()` bytes at `off`.
pub fn read_exact_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(f, buf, off)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let (mut buf, mut off) = (buf, off);
        while !buf.is_empty() {
            match f.seek_read(buf, off) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    off += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Writes all of `buf` at `off`.
pub fn write_all_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::write_all_at(f, buf, off)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let (mut buf, mut off) = (buf, off);
        while !buf.is_empty() {
            match f.seek_write(buf, off) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    buf = &buf[n..];
                    off += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// The file's inode; 0 where there is none.
pub fn inode(m: &Metadata) -> u64 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::ino(m)
    }
    #[cfg(windows)]
    {
        let _ = m;
        0
    }
}

/// Whether two names are the same file (a file system that conflates
/// them). Never on Windows, whose file systems keep normalisation forms
/// apart.
pub fn same_file(a: &Metadata, b: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (a.dev(), a.ino()) == (b.dev(), b.ino())
    }
    #[cfg(windows)]
    {
        let _ = (a, b);
        false
    }
}

/// Whether the file is executable by its owner (never on Windows).
pub fn executable(m: &Metadata) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o100 != 0
    }
    #[cfg(windows)]
    {
        let _ = m;
        false
    }
}

/// Creates a new file (never following a link at `path`), executable or
/// not where that exists.
pub fn create_new(path: &Path, executable: bool) -> io::Result<File> {
    let mut o = File::options();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, mode(executable));
    #[cfg(windows)]
    let _ = executable;
    o.open(path)
}

/// Sets the file's mode past the umask (unix only).
pub fn set_executable(f: &File, executable: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(mode(executable)))
    }
    #[cfg(windows)]
    {
        let _ = (f, executable);
        Ok(())
    }
}

#[cfg(unix)]
fn mode(executable: bool) -> u32 {
    if executable { 0o755 } else { 0o644 }
}

/// Makes a symbolic link at `link` to `target` (its bytes as stored).
pub fn symlink(target: &[u8], link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), link)
    }
    #[cfg(windows)]
    {
        let target = std::str::from_utf8(target)
            .map_err(|_| io::Error::other("a link target that is not UTF-8"))?
            .replace('/', "\\");
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// Opens a file to set its times: Windows needs the right to write its
/// attributes, which a read-only open lacks; unix needs only the owner.
pub fn open_for_times(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        File::open(path)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES
        File::options().access_mode(0x0080 | 0x0100).open(path)
    }
}

/// The length of a symbolic link's target as stored (§5.1): unix reports
/// it as the link's size, Windows as zero.
pub fn link_len(path: &Path, m: &Metadata) -> io::Result<u64> {
    #[cfg(unix)]
    {
        let _ = path;
        Ok(m.len())
    }
    #[cfg(windows)]
    {
        let _ = m;
        Ok(link_target(path)?.len() as u64)
    }
}

/// A symbolic link's target, as bytes.
pub fn link_target(path: &Path) -> io::Result<Vec<u8>> {
    let target = std::fs::read_link(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(target.into_os_string().into_vec())
    }
    #[cfg(windows)]
    {
        let t = target
            .to_str()
            .ok_or_else(|| io::Error::other("a link target that is not UTF-8"))?;
        Ok(t.replace('\\', "/").into_bytes())
    }
}

/// Makes a directory's entries durable (unix; Windows has no such call
/// for a directory, and its renames are durable with the file's flush).
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(())
    }
}
