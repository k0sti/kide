//! `Rescan`: walks the whole folder and reports every path whose metadata
//! differs from the index, every racy entry, and new and vanished paths.

use crate::error::Result;
use crate::iface::{ChangeSource, CheckedIndex, Workspace};
use std::collections::BTreeSet;

#[derive(Default)]
pub struct Rescan;

impl ChangeSource for Rescan {
    fn changed(&mut self, ws: &dyn Workspace, index: &CheckedIndex) -> Result<Vec<String>> {
        let mut out = BTreeSet::new();
        let mut present = BTreeSet::new();
        for (path, meta) in ws.list()? {
            let same = index
                .get(&path)
                .is_some_and(|c| c.meta == meta && !c.is_racy());
            if !same {
                out.insert(path.clone());
            }
            present.insert(path);
        }
        out.extend(index.keys().filter(|p| !present.contains(*p)).cloned());
        Ok(out.into_iter().collect())
    }
}

// The suite needs unix symbolic links and permission bits.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::conformance::{ChangeFixture, TempDir, change_source};
    use crate::format::manifest::Kind;
    use crate::iface::Checked;
    use crate::workspace::PlainFolder;

    #[test]
    fn conformance() {
        let tmp = TempDir::new("rescan");
        change_source(&|| {
            let root = tmp.subdir();
            ChangeFixture {
                ws: Box::new(PlainFolder::new(&root)),
                root,
                source: Box::new(Rescan),
            }
        });
    }

    #[test]
    fn unchanged_paths_are_not_reported() {
        let tmp = TempDir::new("rescan");
        let ws = PlainFolder::new(tmp.path());
        ws.write("a.txt", Kind::Regular, &mut &b"a"[..], 1_000_000_000)
            .unwrap();
        ws.write("d/", Kind::Directory, &mut &b""[..], 0).unwrap();
        let index: CheckedIndex = ws
            .list()
            .unwrap()
            .into_iter()
            .map(|(p, meta)| {
                (
                    p,
                    Checked {
                        meta,
                        at: crate::time::now() + 3_000_000_000,
                    },
                )
            })
            .collect();
        assert_eq!(index.len(), 2);
        assert!(Rescan.changed(&ws, &index).unwrap().is_empty());
    }
}
