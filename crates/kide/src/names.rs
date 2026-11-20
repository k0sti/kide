//! Names on disk (§5.3): NFC, the platform key and held entries. None of
//! this enters an object, so it may follow any Unicode version; whether a
//! manifest is valid never depends on it (`format::path` decides that).

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use unicode_normalization::{UnicodeNormalization, is_nfc};

/// `name` in Unicode Normalization Form C.
pub fn nfc(name: &str) -> Cow<'_, str> {
    match is_nfc(name) {
        true => Cow::Borrowed(name),
        false => Cow::Owned(name.nfc().collect()),
    }
}

/// Unicode simple case folding (CaseFolding.txt, status C and S): one
/// character to one character, so `ß` stays and `ẞ` becomes `ß`.
fn simple_fold(s: &str) -> String {
    let fold = |c: char| {
        unicode_case_mapping::case_folded(c)
            .and_then(|f| char::from_u32(f.get()))
            .unwrap_or(c)
    };
    s.chars().map(fold).collect()
}

/// The platform key of a component: its NFC form after simple case folding,
/// with trailing dots and spaces removed.
pub fn platform_key(component: &str) -> String {
    let folded = simple_fold(&nfc(component));
    nfc(&folded).trim_end_matches(['.', ' ']).to_string()
}

/// What a platform cannot represent on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    /// Whether the file system conflates names with equal platform keys.
    pub conflates: bool,
    /// Whether the Windows names are forbidden (§5.3).
    pub windows_names: bool,
    /// The longest relative path, in UTF-16 code units, if there is a limit.
    pub max_path: Option<usize>,
}

impl Platform {
    /// Distinguishes every name and forbids none.
    pub const LINUX: Platform = Platform {
        conflates: false,
        windows_names: false,
        max_path: None,
    };
    /// Case-insensitive, with the default volume format.
    pub const MACOS: Platform = Platform {
        conflates: true,
        windows_names: false,
        max_path: None,
    };
    pub(crate) const WINDOWS: Platform = Platform {
        conflates: true,
        windows_names: true,
        max_path: Some(259),
    };

    /// The platform this program runs on.
    pub fn current() -> Platform {
        match () {
            _ if cfg!(windows) => Platform::WINDOWS,
            _ if cfg!(target_os = "macos") => Platform::MACOS,
            _ => Platform::LINUX,
        }
    }

    fn forbids(&self, path: &str) -> bool {
        let body = path.strip_suffix('/').unwrap_or(path);
        let too_long = self
            .max_path
            .is_some_and(|max| body.encode_utf16().count() > max);
        too_long || (self.windows_names && body.split('/').any(windows_forbids))
    }
}

/// The Windows rules of §5.3 for one component.
fn windows_forbids(c: &str) -> bool {
    const DEVICES: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let bad_char = |ch: char| "<>:\"\\|?*".contains(ch) || ('\u{1}'..='\u{1f}').contains(&ch);
    let stem = c.split('.').next().unwrap_or(c);
    let numbered = |prefix: &str| {
        stem.len() == 4
            && stem.as_bytes()[..3].eq_ignore_ascii_case(prefix.as_bytes())
            && (b'1'..=b'9').contains(&stem.as_bytes()[3])
    };
    c.chars().any(bad_char)
        || DEVICES.iter().any(|d| stem.eq_ignore_ascii_case(d))
        || numbered("COM")
        || numbered("LPT")
        || c.ends_with(['.', ' '])
}

/// The shown entries as a tree of components, each level with the
/// components of every platform key.
#[derive(Default)]
struct Shown {
    children: HashMap<String, Shown>,
    keys: HashMap<String, Vec<String>>,
}

impl Shown {
    /// Whether `parts` collides with a shown path: at the first component
    /// where they differ by bytes, the keys are equal (§5.3).
    fn collides(&self, parts: &[&str]) -> bool {
        let mut node = self;
        for c in parts {
            let same_key = node.keys.get(&platform_key(c));
            if same_key.is_some_and(|cs| cs.iter().any(|x| x != c)) {
                return true;
            }
            match node.children.get(*c) {
                Some(next) => node = next,
                None => return false,
            }
        }
        false
    }

    fn insert(&mut self, parts: &[&str]) {
        let mut node = self;
        for c in parts {
            let cs = node.keys.entry(platform_key(c)).or_default();
            if !cs.iter().any(|x| x == c) {
                cs.push(c.to_string());
            }
            node = node.children.entry(c.to_string()).or_default();
        }
    }
}

/// The live paths a device holds instead of writing (§5.3), from every live
/// path and whether it is already on disk: every name the platform forbids,
/// and, where names with equal keys are conflated, every path that collides
/// with one already shown. The paths on disk are shown first, then the
/// others, each in byte order. Forbidden paths take no part in collisions:
/// they are never on disk. A directory entry's trailing `/` does not count.
/// Returns (held paths, mismatched on-disk collisions). When both colliding
/// entries are on disk, the later one is both held and reported as a mismatch.
pub fn held<'a>(
    live: impl IntoIterator<Item = (&'a str, bool)>,
    p: &Platform,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut live: Vec<(&str, bool)> = live.into_iter().collect();
    live.sort_unstable_by_key(|&(path, on_disk)| (!on_disk, path));
    let (mut held_out, mut mismatch, mut shown) =
        (BTreeSet::new(), BTreeSet::new(), Shown::default());
    for (path, on_disk) in live {
        let parts: Vec<&str> = path.strip_suffix('/').unwrap_or(path).split('/').collect();
        if p.forbids(path) {
            held_out.insert(path.to_string());
        } else if p.conflates && shown.collides(&parts) && on_disk {
            // Both this entry and the one it collides with are on disk: the file
            // system doesn't conflate keys, so the platform setting is wrong
            // (§5.3). Both stay shown, since a held entry is never checked
            // against the disk and edits to it would never be scanned.
            mismatch.insert(path.to_string());
        } else if p.conflates && shown.collides(&parts) {
            held_out.insert(path.to_string());
        } else if p.conflates {
            shown.insert(&parts);
        }
    }
    (held_out, mismatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live paths of which none is on disk yet.
    fn new<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<(&'a str, bool)> {
        paths.into_iter().map(|p| (p, false)).collect()
    }

    #[test]
    fn nfc_composes() {
        assert_eq!(nfc("e\u{301}"), "é");
        assert!(matches!(nfc("plain"), Cow::Borrowed(_)));
        assert_eq!(nfc("Å"), "Å", "U+212B ANGSTROM SIGN");
    }

    #[test]
    fn keys_fold_simply() {
        assert_eq!(platform_key("README.md"), "readme.md");
        assert_eq!(
            platform_key("Straße"),
            "straße",
            "ß has only a full folding"
        );
        assert_eq!(platform_key("STRASSE"), "strasse");
        assert_eq!(platform_key("\u{1e9e}"), "ß", "ẞ folds simply to ß");
        assert_eq!(platform_key("ΣΑΣ"), "σασ");
        assert_eq!(platform_key("e\u{301}"), platform_key("É"));
        assert_eq!(platform_key("name. . "), "name");
        assert_eq!(
            platform_key("İ"),
            "İ",
            "İ has only full and Turkic foldings"
        );
    }

    #[test]
    fn linux_holds_nothing() {
        let live = ["A", "a", "a.", "CON", "x?", "\u{1e9e}", "ß"];
        let (h, m) = held(new(live), &Platform::LINUX);
        assert!(h.is_empty());
        assert!(m.is_empty());
    }

    #[test]
    fn case_insensitive_holds_all_but_the_smallest() {
        let live = [
            "Notes.md", "a/x", "notes.md", "NOTES.MD", "A/x", "b/", "B", "Straße", "STRASSE",
        ];
        let (got, mismatch) = held(new(live), &Platform::MACOS);
        let want = ["Notes.md", "a/x", "b/", "notes.md"];
        assert_eq!(got.into_iter().collect::<Vec<_>>(), want);
        assert!(mismatch.is_empty());
    }

    #[test]
    fn collisions_are_per_component() {
        let live = [
            "A/x", "a/y", "a", "B", "b/c/d", "c/x", "c/y", "C/", "d/E", "d/e/f",
        ];
        let (got, mismatch) = held(new(live), &Platform::MACOS);
        let want = ["a", "a/y", "b/c/d", "c/x", "c/y", "d/e/f"];
        assert_eq!(got.into_iter().collect::<Vec<_>>(), want);
        assert!(mismatch.is_empty());
        let apart = ["a/x", "a/y", "a/X/z", "ab", "a b"];
        let (got, mismatch) = held(new(apart), &Platform::MACOS);
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            ["a/x"],
            "X sorts first"
        );
        assert!(mismatch.is_empty());
    }

    #[test]
    fn entries_on_disk_are_shown_first() {
        // A new remote `B` sorts before the local file `b`: `b` stays shown.
        let (got, mismatch) = held([("B", false), ("b", true)], &Platform::MACOS);
        assert_eq!(got.into_iter().collect::<Vec<_>>(), ["B"]);
        assert!(mismatch.is_empty());
        let live = [("A/x", false), ("a/y", true), ("C", false), ("c/", true)];
        let (got, mismatch) = held(live, &Platform::MACOS);
        assert_eq!(got.into_iter().collect::<Vec<_>>(), ["A/x", "C"]);
        assert!(mismatch.is_empty());
        // Forbidden names are held even on disk, and shown by none.
        let (got, mismatch) = held([("A.", true), ("a", false)], &Platform::WINDOWS);
        assert_eq!(got.into_iter().collect::<Vec<_>>(), ["A."]);
        assert!(mismatch.is_empty());
    }

    #[test]
    fn colliding_on_disk_entries_are_both_shown() {
        // Two on-disk entries that collide: neither is held; the mismatch is reported.
        let (h, m) = held([("A", true), ("a", true)], &Platform::MACOS);
        assert!(h.is_empty(), "no on-disk entry is held");
        assert_eq!(
            m.into_iter().collect::<Vec<_>>(),
            ["a"],
            "the mismatch is reported"
        );
        // A new entry colliding with them is still held.
        let (h, _) = held([("A", true), ("a", true), ("A ", false)], &Platform::MACOS);
        assert_eq!(h.into_iter().collect::<Vec<_>>(), ["A "]);
        // Mixed: one on disk, one not.
        let (h, m) = held([("A", false), ("a", true)], &Platform::MACOS);
        assert_eq!(
            h.into_iter().collect::<Vec<_>>(),
            ["A"],
            "the not-on-disk one is held"
        );
        assert!(m.is_empty(), "no mismatch when only one is on disk");
    }

    #[test]
    fn windows_forbids_names() {
        let forbidden = [
            "a<b",
            "a>b",
            "a:b",
            "a\"b",
            "a\\b",
            "a|b",
            "a?b",
            "a*b",
            "tab\tname",
            "CON",
            "con.txt",
            "Prn",
            "aux.tar.gz",
            "NUL",
            "COM1",
            "lpt9.log",
            "dot.",
            "space ",
            "dir/ok/bad.",
        ];
        for p in forbidden {
            let (h, _) = held(new([p]), &Platform::WINDOWS);
            assert_eq!(h.len(), 1, "{p:?}");
        }
        let allowed = [
            "CONSOLE", "COM0", "COM10", "LPT", "nul_", ".bashrc", "a.b", "über",
            // Four bytes whose third is inside a character.
            "Zoë.md", "a中.md",
        ];
        for p in allowed {
            let (h, _) = held(new([p]), &Platform::WINDOWS);
            assert!(h.is_empty(), "{p:?}");
        }
        let long = "x".repeat(260);
        let (h, _) = held(new([long.as_str()]), &Platform::WINDOWS);
        assert_eq!(h.len(), 1);
        // A forbidden name does not take the place of a colliding one.
        let (got, mismatch) = held(new(["A.", "a"]), &Platform::WINDOWS);
        assert_eq!(got.into_iter().collect::<Vec<_>>(), ["A."]);
        assert!(mismatch.is_empty());
    }
}
