use super::*;
use crate::conformance::TempDir;
use crate::load::read_manifest;

#[test]
fn init_then_open() {
    let tmp = TempDir::new("folder");
    Folder::init(tmp.path()).unwrap();
    let kide = tmp.path().join(".kide");
    for p in [
        "config", "format", "HEAD", "index", "roots", "lock", "objects", "staging", "trash",
    ] {
        assert!(kide.join(p).exists(), "{p}");
    }
    let f = Folder::open(tmp.path()).unwrap();
    assert_eq!(f.config, Config::parse(config::TEMPLATE).unwrap());
    assert!(f.config.ignores(".git"));
    assert!(f.index.0.is_empty());
    assert!(
        read_manifest(&*f.store, &f.head).unwrap().is_empty(),
        "HEAD is stored"
    );
    assert!(f.roots().unwrap().is_empty());
}

#[test]
fn init_twice_and_open_without_init_are_refused() {
    let tmp = TempDir::new("folder");
    assert!(matches!(Folder::open(tmp.path()), Err(Error::Invalid(_))));
    Folder::init(tmp.path()).unwrap();
    assert!(matches!(Folder::init(tmp.path()), Err(Error::Invalid(_))));
}

#[test]
fn the_lock_is_exclusive_and_reading_needs_none() {
    let tmp = TempDir::new("folder");
    Folder::init(tmp.path()).unwrap();
    let first = Folder::open(tmp.path()).unwrap();
    assert!(matches!(
        Folder::open(tmp.path()),
        Err(Error::Unavailable(_))
    ));
    let mut ro = Folder::open_read_only(tmp.path()).unwrap();
    assert_eq!(ro.head, first.head);
    assert!(matches!(ro.set_head(Hash([1; 32])), Err(Error::Invalid(_))));
    drop(first);
    Folder::open(tmp.path()).unwrap();
}

#[test]
fn head_is_rewritten_and_the_old_root_retained() {
    let tmp = TempDir::new("folder");
    Folder::init(tmp.path()).unwrap();
    let mut f = Folder::open_unsynced(tmp.path()).unwrap();
    let first = f.head;
    f.set_head(Hash([7; 32])).unwrap();
    drop(f);
    let f = Folder::open_unsynced(tmp.path()).unwrap();
    assert_eq!(f.head, Hash([7; 32]));
    let roots = f.roots().unwrap();
    assert_eq!(roots.iter().map(|r| r.1).collect::<Vec<_>>(), [first]);
    assert!(roots[0].0 > 1_700_000_000);
    let names: Vec<_> = fs::read_dir(f.kide())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(
        !names
            .iter()
            .any(|n| n.to_string_lossy().ends_with(".kide-tmp"))
    );
}

#[test]
fn bad_config_is_invalid() {
    let tmp = TempDir::new("folder");
    Folder::init(tmp.path()).unwrap();
    fs::write(tmp.path().join(".kide/config"), "colour = blue\n").unwrap();
    assert!(matches!(Folder::open(tmp.path()), Err(Error::Invalid(_))));
}

#[test]
fn a_folder_of_another_format_is_incompatible() {
    let tmp = TempDir::new("folder");
    Folder::init(tmp.path()).unwrap();
    let format = tmp.path().join(".kide/format");
    assert_eq!(fs::read_to_string(&format).unwrap(), format!("{FORMAT}\n"));
    fs::write(&format, "9\n").unwrap();
    let r = Folder::open(tmp.path());
    assert!(matches!(r, Err(Error::Incompatible(m)) if m.contains("format 9")));
    fs::remove_file(&format).unwrap();
    assert!(matches!(
        Folder::open_read_only(tmp.path()),
        Err(Error::Incompatible(_))
    ));
    assert!(matches!(
        Folder::open(tmp.path()),
        Err(Error::Incompatible(_))
    ));
    assert!(
        !format.exists(),
        "missing markers are never inferred or migrated"
    );
}
