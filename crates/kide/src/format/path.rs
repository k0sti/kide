//! Paths and their components (§5.1, §5.3), as decoders check them. NFC and
//! the platform key belong to scanning, not to validity.

use crate::error::{Result, invalid};
use crate::hash::Hash;

/// The longest path, a trailing `/` included.
pub const MAX_PATH: usize = 4096;
/// The longest component.
pub(crate) const MAX_COMPONENT: usize = 255;

/// Whether `c` is a valid component; `top` says it is the first of its path.
/// Reserved names match in any ASCII case.
pub fn valid_component(c: &str, top: bool) -> bool {
    let reserved = |name: &str| {
        c.len() >= name.len() && {
            let tail = &c.as_bytes()[c.len() - name.len()..];
            tail.eq_ignore_ascii_case(name.as_bytes())
        }
    };
    (1..=MAX_COMPONENT).contains(&c.len())
        && !c.contains(['/', '\0'])
        && c != "."
        && c != ".."
        && !reserved(".kide-tmp")
        && !(top && c.eq_ignore_ascii_case(".kide"))
}

/// Checks a manifest path: components joined by `/`, plus one trailing `/`
/// for an empty directory; at most 4,096 bytes. A `&str` is valid UTF-8.
pub fn check_path(p: &str) -> Result<()> {
    if p.len() > MAX_PATH {
        return invalid(format!("path of {} bytes", p.len()));
    }
    let body = p.strip_suffix('/').unwrap_or(p);
    let mut parts = body.split('/');
    let top_ok = parts.next().is_some_and(|c| valid_component(c, true));
    if !top_ok || !parts.all(|c| valid_component(c, false)) {
        return invalid(format!("invalid path {p:?}"));
    }
    Ok(())
}

/// Whether the path names an empty directory.
pub fn is_dir_path(p: &str) -> bool {
    p.ends_with('/')
}

/// Extensions of files that are never text (§5.6): their versions keep no
/// history content and they never merge as text.
pub(crate) const BINARY: &[&str] = &[
    "7z", "a", "aac", "avi", "avif", "bin", "bmp", "bz2", "class", "cr2", "db", "dll", "dmg",
    "dng", "doc", "docx", "dylib", "epub", "exe", "flac", "gif", "gz", "heic", "heif", "ico",
    "iso", "jar", "jpeg", "jpg", "m4a", "m4v", "mkv", "mov", "mp3", "mp4", "nef", "o", "odp",
    "ods", "odt", "ogg", "opus", "otf", "pack", "pdf", "png", "ppt", "pptx", "psd", "rar", "so",
    "sqlite", "tar", "tgz", "tif", "tiff", "ttf", "wasm", "wav", "webm", "webp", "woff", "woff2",
    "xls", "xlsx", "xz", "zip", "zst",
];

/// Extensions of prose (§5.6): the only files merged as text, line by line.
/// Code and structured text (JSON, canvases, lock files) would merge into
/// something wrong without saying so, so they keep conflict copies.
pub(crate) const PROSE: &[&str] = &["adoc", "markdown", "md", "org", "rst", "text", "txt"];

/// Whether `path` merges as text (§5.6): its last component has an
/// extension in `PROSE`, in any ASCII case.
pub fn is_prose(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => PROSE.contains(&ext.to_ascii_lowercase().as_str()),
        _ => false,
    }
}

/// Whether `path` may hold text (§5.6): its last component has no extension
/// in `BINARY`, in any ASCII case. Every version of such a path keeps its
/// content, so any device can read its history back and merge it.
pub fn may_be_text(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => {
            !BINARY.contains(&ext.to_ascii_lowercase().as_str())
        }
        _ => true,
    }
}

/// Where the content of record `x` at `path` is preserved (§5.6):
/// `<stem>.kide-conflict-<first 12 hex digits of x><.ext>` on the last
/// component, the extension starting at its last dot unless that dot is the
/// first character. A stem too long for a 255-byte name is cut to the longest
/// prefix, at a character boundary, that fits. The result may still be too
/// long to be valid: as a path, or as a name whose extension alone is too long.
pub fn conflict_path(path: &str, x: &Hash) -> String {
    let (dir, name) = path.rsplit_once('/').map_or(("", path), |(d, n)| (d, n));
    let tag = format!(".kide-conflict-{}", &x.to_hex()[..12]);
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    let stem = prefix(stem, MAX_COMPONENT.saturating_sub(tag.len() + ext.len()));
    match dir {
        "" => format!("{stem}{tag}{ext}"),
        _ => format!("{dir}/{stem}{tag}{ext}"),
    }
}

/// The longest prefix of `s` of at most `max` bytes that ends at a
/// character boundary.
fn prefix(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1; // 0 is a boundary
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_prose_merges_as_text() {
        for p in [
            "a.md",
            "n/B.MD",
            "x.txt",
            "notes.org",
            "doc.rst",
            "r.markdown",
        ] {
            assert!(is_prose(p), "{p}");
        }
        for p in [
            "a.json",
            "c.canvas",
            "Cargo.lock",
            "m.rs",
            "Makefile",
            ".md",
            "d/.txt",
        ] {
            assert!(!is_prose(p), "{p}");
        }
    }

    #[test]
    fn invalid_paths() {
        let long = format!("{}b", "a/".repeat(2048));
        let wide = "a".repeat(256);
        let bad = [
            "",
            "/",
            "/a",
            "a//b",
            "a//",
            "a/./b",
            "a/..",
            "./a",
            ".kide",
            ".kide/x",
            ".KiDe/",
            "x.kide-tmp",
            "x.KIDE-TMP",
            "a/.b.Kide-Tmp/c",
            "a\0b",
            &wide,
            &long,
        ];
        for p in bad {
            assert!(check_path(p).is_err(), "{p:?}");
        }
    }

    #[test]
    fn valid_paths() {
        let max = format!(
            "{}{}/",
            format!("{}/", "x".repeat(255)).repeat(15),
            "y".repeat(255)
        );
        assert_eq!(max.len(), MAX_PATH);
        let good = [
            "a",
            "a/b",
            "d/",
            "a/.kide",
            "a/.KIDE/b",
            ".kide-tmp.x",
            ".kidex",
            "é",
            "e\u{301}",
            "İKIDE",
            "..a",
            &max,
        ];
        for p in good {
            assert!(check_path(p).is_ok(), "{p:?}");
        }
        assert!(check_path(&format!("z{max}")).is_err(), "one byte too long");
    }

    #[test]
    fn conflict_paths() {
        let x = Hash::from_hex(&format!("3fa2b1c90d4e{}", "7".repeat(52))).unwrap();
        let cases = [
            ("notes.md", "notes.kide-conflict-3fa2b1c90d4e.md"),
            (".bashrc", ".bashrc.kide-conflict-3fa2b1c90d4e"),
            (
                "archive.tar.gz",
                "archive.tar.kide-conflict-3fa2b1c90d4e.gz",
            ),
            ("Makefile", "Makefile.kide-conflict-3fa2b1c90d4e"),
            ("a.d/notes", "a.d/notes.kide-conflict-3fa2b1c90d4e"),
            ("d/.env.local", "d/.env.kide-conflict-3fa2b1c90d4e.local"),
        ];
        for (path, want) in cases {
            assert_eq!(conflict_path(path, &x), want);
        }
    }

    #[test]
    fn long_conflict_names_are_cut() {
        let x = Hash::from_hex(&format!("3fa2b1c90d4e{}", "7".repeat(52))).unwrap();
        let tag = ".kide-conflict-3fa2b1c90d4e";
        let long = format!("d/{}.md", "n".repeat(250));
        let got = conflict_path(&long, &x);
        assert_eq!(got, format!("d/{}{tag}.md", "n".repeat(255 - 27 - 3)));
        assert!(check_path(&got).is_ok());
        // A cut never splits a character: é is two bytes.
        let wide = format!("{}.md", "é".repeat(120));
        let got = conflict_path(&wide, &x);
        assert_eq!(got, format!("{}{tag}.md", "é".repeat(112)));
        assert_eq!(got.len(), MAX_COMPONENT - 1);
        // An extension too long for any stem, and a path too long: invalid.
        let ext = format!("a.{}", "e".repeat(240));
        assert!(check_path(&conflict_path(&ext, &x)).is_err());
        let deep = format!("{}f", "d/".repeat(2040));
        assert!(check_path(&deep).is_ok());
        assert!(check_path(&conflict_path(&deep, &x)).is_err());
    }

    #[test]
    fn dir_paths() {
        assert!(is_dir_path("d/"));
        assert!(!is_dir_path("d"));
    }
}
