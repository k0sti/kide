//! `kide git …`: the folder linked to a branch of a Git repository
//! (kide-git).

use crate::args::{Args, Out, usage};
use kide::change::Rescan;
use kide::folder::Folder;
use kide::folder::scan::scan;
use kide::names::Platform;

pub fn run(a: &Args) -> Out<()> {
    let (sub, rest) = match a.pos.split_first() {
        Some((s, rest)) => (s.as_str(), rest),
        None => return usage("git needs a subcommand"),
    };
    let n = rest.len();
    a.expect(0..=usize::MAX, &[])?;
    match (sub, n) {
        ("link", 3) => link(a, &rest[0], &rest[1], &rest[2]),
        ("status", 0) => status(a),
        ("commit", 1) => commit(a, &rest[0]),
        ("push", 0) => push(a),
        ("pull", 0) => pull(a),
        ("link" | "status" | "commit" | "push" | "pull", _) => {
            usage(format!("wrong number of arguments for git {sub}"))
        }
        _ => usage(format!("unknown git command {sub:?}")),
    }
}

fn link(a: &Args, url: &str, branch: &str, author: &str) -> Out<()> {
    let mut folder = Folder::open(&a.dir())?;
    let linked = kide_git::link(&mut folder, url, branch, author)?;
    println!("linked {url} {branch}");
    if let Some(c) = linked.adopted {
        println!("adopted {c}");
    }
    Ok(())
}

/// Read-only, so it works while `kide run` holds the folder.
fn status(a: &Args) -> Out<()> {
    let folder = Folder::open_read_only(&a.dir())?;
    let st = kide_git::status(&folder)?;
    println!("url {}\nbranch {}", st.url, st.branch);
    println!("base {}", st.base.as_deref().unwrap_or("none"));
    for (what, path) in &st.changes {
        println!("{what} {path}");
    }
    for p in &st.conflicts {
        println!("conflict {p}");
    }
    for p in &st.empty_dirs {
        println!("empty-dir {p}");
    }
    Ok(())
}

/// Scans first, so the commit holds what is on disk now.
fn commit(a: &Args, message: &str) -> Out<()> {
    let mut folder = Folder::open(&a.dir())?;
    let ws = folder.workspace();
    scan(&mut folder, &ws, &mut Rescan, &Platform::current())?;
    let message = match message.ends_with('\n') {
        true => message.to_string(),
        false => format!("{message}\n"),
    };
    match kide_git::commit(&folder, &message)? {
        Some(id) => println!("commit {id}"),
        None => println!("nothing to commit"),
    }
    Ok(())
}

fn push(a: &Args) -> Out<()> {
    let folder = Folder::open_read_only(&a.dir())?;
    println!("pushed {}", kide_git::push(&folder)?);
    Ok(())
}

fn pull(a: &Args) -> Out<()> {
    let mut folder = Folder::open(&a.dir())?;
    let p = kide_git::pull(&mut folder)?;
    match p.commit {
        Some(c) => println!("pulled {c} decided {}", p.decided),
        None => println!("up to date"),
    }
    for path in &p.merged {
        println!("merged {path}");
    }
    println!("head {}", folder.head);
    Ok(())
}
