//! A Kide folder linked to a branch of a local bare repository, with a
//! second, ordinary clone standing for everyone who works with Git.

use kide::Error;
use kide::change::Rescan;
use kide::conformance::TempDir;
use kide::folder::Folder;
use kide::folder::scan::scan;
use kide::names::Platform;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const AUTHOR: &str = "Ada <ada@example.com>";

/// Runs git with a fixed identity and no user configuration.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["-c", "user.name=Up", "-c", "user.email=up@example.com"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}

struct World {
    _tmp: TempDir,
    remote: PathBuf,
    clone: PathBuf,
    dir: PathBuf,
}

impl World {
    fn new() -> World {
        let tmp = TempDir::new("kide-git");
        let remote = tmp.subdir();
        git(&remote, &["init", "-q", "--bare"]);
        let clone = tmp.subdir();
        git(&clone, &["init", "-q"]);
        git(
            &clone,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        let dir = tmp.subdir();
        Folder::init(&dir).unwrap();
        World {
            _tmp: tmp,
            remote,
            clone,
            dir,
        }
    }

    fn url(&self) -> &str {
        self.remote.to_str().unwrap()
    }

    fn folder(&self) -> Folder {
        Folder::open(&self.dir).unwrap()
    }

    /// Writes files into the folder and scans.
    fn edit(&self, files: &[(&str, &str)]) -> Folder {
        for (p, text) in files {
            let full = self.dir.join(p);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, text).unwrap();
        }
        let mut f = self.folder();
        let ws = f.workspace();
        scan(&mut f, &ws, &mut Rescan, &Platform::current()).unwrap();
        f
    }

    /// Commits files in the clone and pushes them to the remote.
    fn upstream(&self, files: &[(&str, &str)], message: &str) -> String {
        if !git(&self.clone, &["ls-remote", "origin", "main"]).is_empty() {
            git(&self.clone, &["pull", "-q", "--ff-only", "origin", "main"]);
        }
        for (p, text) in files {
            let full = self.clone.join(p);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, text).unwrap();
        }
        git(&self.clone, &["add", "-A"]);
        git(&self.clone, &["commit", "-q", "-m", message]);
        git(&self.clone, &["push", "-q", "origin", "HEAD:main"]);
        git(&self.clone, &["rev-parse", "HEAD"])
    }

    fn remote_file(&self, path: &str) -> String {
        git(&self.remote, &["show", &format!("main:{path}")])
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.dir.join(path)).unwrap()
    }
}

#[test]
fn a_folder_publishes_to_an_empty_branch() {
    let w = World::new();
    w.edit(&[("README.md", "hello\n"), ("src/lib.rs", "fn a() {}\n")]);
    fs::create_dir_all(w.dir.join(".git")).unwrap();
    fs::write(w.dir.join(".git/HEAD"), "local\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let run = w.dir.join("run.sh");
        fs::write(&run, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("README.md", w.dir.join("readme")).unwrap();
    }
    let mut f = w.edit(&[]);
    kide_git::link(&mut f, w.url(), "main", AUTHOR).unwrap();

    let id = kide_git::commit(&f, "First\n").unwrap().expect("a commit");
    assert_eq!(kide_git::push(&f).unwrap(), id);
    assert_eq!(git(&w.remote, &["rev-parse", "main"]), id);
    assert_eq!(w.remote_file("README.md"), "hello");
    assert_eq!(w.remote_file("src/lib.rs"), "fn a() {}");
    assert_eq!(
        git(&w.remote, &["log", "-1", "--format=%an <%ae>|%s", "main"]),
        "Ada <ada@example.com>|First"
    );
    let tree = git(&w.remote, &["ls-tree", "-r", "main"]);
    assert!(!tree.contains(".git/"), "ignored .git is not exported");
    #[cfg(unix)]
    {
        assert!(
            tree.lines()
                .any(|l| l.starts_with("100755") && l.ends_with("run.sh"))
        );
        assert!(
            tree.lines()
                .any(|l| l.starts_with("120000") && l.ends_with("readme"))
        );
    }
    // Nothing changed: no echo commit.
    drop(f);
    assert_eq!(kide_git::commit(&w.edit(&[]), "Again\n").unwrap(), None);
}

#[test]
fn edits_and_deletions_become_the_next_commit() {
    let w = World::new();
    let mut f = w.edit(&[("a.txt", "1\n"), ("b.txt", "2\n")]);
    kide_git::link(&mut f, w.url(), "main", AUTHOR).unwrap();
    let first = kide_git::commit(&f, "First\n").unwrap().unwrap();
    drop(f);
    fs::remove_file(w.dir.join("b.txt")).unwrap();
    let f = w.edit(&[("a.txt", "one\n")]);
    let st = kide_git::status(&f).unwrap();
    assert_eq!(
        st.changes,
        vec![('M', "a.txt".into()), ('D', "b.txt".into())]
    );
    let second = kide_git::commit(&f, "Second\n").unwrap().unwrap();
    kide_git::push(&f).unwrap();
    assert_eq!(git(&w.remote, &["rev-parse", "main^"]), first);
    assert_eq!(git(&w.remote, &["rev-parse", "main"]), second);
    assert_eq!(git(&w.remote, &["ls-tree", "--name-only", "main"]), "a.txt");
    assert_eq!(w.remote_file("a.txt"), "one");
}

#[test]
fn an_empty_folder_adopts_the_branch() {
    let w = World::new();
    let head = w.upstream(&[("README.md", "up\n"), ("docs/a.md", "a\n")], "Up");
    let mut f = w.folder();
    let linked = kide_git::link(&mut f, w.url(), "main", AUTHOR).unwrap();
    assert_eq!(linked.adopted.as_deref(), Some(head.as_str()));
    assert_eq!(w.read("README.md"), "up\n");
    assert_eq!(w.read("docs/a.md"), "a\n");
    assert!(kide_git::status(&f).unwrap().changes.is_empty());
    assert_eq!(kide_git::commit(&f, "Nothing\n").unwrap(), None);
}

#[test]
fn linking_refuses_when_both_sides_have_files() {
    let w = World::new();
    w.upstream(&[("a.txt", "up\n")], "Up");
    let mut f = w.edit(&[("b.txt", "local\n")]);
    let e = kide_git::link(&mut f, w.url(), "main", AUTHOR).unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    assert!(!w.dir.join("a.txt").exists());
    assert!(!w.dir.join(".kide/git").exists(), "no half-made link");
}

#[test]
fn upstream_and_local_edits_to_different_files_both_survive() {
    let w = World::new();
    w.upstream(&[("a.txt", "1\n"), ("b.txt", "2\n")], "Base");
    let mut f = w.folder();
    kide_git::link(&mut f, w.url(), "main", AUTHOR).unwrap();
    drop(f);
    let up = w.upstream(&[("a.txt", "upstream\n")], "Edit a");
    let mut f = w.edit(&[("b.txt", "local\n")]);
    let pulled = kide_git::pull(&mut f).unwrap();
    assert_eq!(pulled.commit.as_deref(), Some(up.as_str()));
    assert_eq!(w.read("a.txt"), "upstream\n");
    assert_eq!(w.read("b.txt"), "local\n");
    let st = kide_git::status(&f).unwrap();
    assert_eq!(st.changes, vec![('M', "b.txt".into())]);
    let id = kide_git::commit(&f, "Edit b\n").unwrap().unwrap();
    kide_git::push(&f).unwrap();
    assert_eq!(git(&w.remote, &["rev-parse", "main^"]), up);
    assert_eq!(git(&w.remote, &["rev-parse", "main"]), id);
    assert_eq!(w.remote_file("a.txt"), "upstream");
}

#[test]
fn the_same_file_changed_on_both_sides_blocks_the_commit() {
    let w = World::new();
    w.upstream(&[("main.rs", "fn main() {}\n")], "Base");
    let mut f = w.folder();
    kide_git::link(&mut f, w.url(), "main", AUTHOR).unwrap();
    drop(f);
    w.upstream(&[("main.rs", "fn main() { up() }\n")], "Up");
    let mut f = w.edit(&[("main.rs", "fn main() { local() }\n")]);
    kide_git::pull(&mut f).unwrap();
    let st = kide_git::status(&f).unwrap();
    assert_eq!(st.conflicts.len(), 1, "{st:?}");
    assert!(st.conflicts[0].starts_with("main.kide-conflict-"));
    let e = kide_git::commit(&f, "Both\n").unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
}

#[test]
fn prose_changed_on_both_sides_merges() {
    let w = World::new();
    w.upstream(&[("notes.md", "one\n\ntwo\n\nthree\n")], "Base");
    let mut f = w.folder();
    kide_git::link(&mut f, w.url(), "main", AUTHOR).unwrap();
    drop(f);
    w.upstream(&[("notes.md", "ONE\n\ntwo\n\nthree\n")], "Up");
    let mut f = w.edit(&[("notes.md", "one\n\ntwo\n\nTHREE\n")]);
    kide_git::pull(&mut f).unwrap();
    assert_eq!(w.read("notes.md"), "ONE\n\ntwo\n\nTHREE\n");
    assert!(kide_git::status(&f).unwrap().conflicts.is_empty());
}

#[test]
fn an_unpushed_commit_is_redone_on_top_of_upstream() {
    let w = World::new();
    w.upstream(&[("a.txt", "1\n"), ("b.txt", "2\n")], "Base");
    let mut f = w.folder();
    kide_git::link(&mut f, w.url(), "main", AUTHOR).unwrap();
    drop(f);
    let f = w.edit(&[("b.txt", "local\n")]);
    kide_git::commit(&f, "Local\n").unwrap().unwrap();
    let up = w.upstream(&[("a.txt", "upstream\n")], "Up");
    let e = kide_git::push(&f).unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    drop(f);
    let mut f = w.folder();
    kide_git::pull(&mut f).unwrap();
    assert_eq!(w.read("a.txt"), "upstream\n");
    assert_eq!(w.read("b.txt"), "local\n");
    let id = kide_git::commit(&f, "Local again\n").unwrap().unwrap();
    kide_git::push(&f).unwrap();
    assert_eq!(git(&w.remote, &["rev-parse", "main^"]), up);
    assert_eq!(git(&w.remote, &["rev-parse", "main"]), id);
    assert_eq!(w.remote_file("b.txt"), "local");
}

#[test]
fn repositories_using_lfs_are_refused() {
    let w = World::new();
    w.upstream(
        &[(
            ".gitattributes",
            "*.png filter=lfs diff=lfs merge=lfs -text\n",
        )],
        "LFS",
    );
    let mut f = w.folder();
    let e = kide_git::link(&mut f, w.url(), "main", AUTHOR).unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    assert!(!w.dir.join(".gitattributes").exists());
}
