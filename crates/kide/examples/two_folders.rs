//! Kide as a library, in one page: two folders on one machine, kept equal
//! through an in-process network. A real application swaps `InProcess` for
//! a network transport (`Tcp`, `Quic`, FIPS) and runs `scan` when files
//! change (`kide::folder::watch`) and `pull` when a peer announces a root.
//!
//!     cargo run -p kide --example two_folders

use kide::change::Rescan;
use kide::folder::Folder;
use kide::folder::net;
use kide::folder::pull::{Pulling, pull};
use kide::folder::scan::scan;
use kide::names::Platform;
use kide::resolve::DecisionTable;
use kide::transport::InProcess;
use std::path::Path;
use std::{fs, process};

fn main() -> kide::Result<()> {
    let base = std::env::temp_dir().join(format!("kide-two-folders-{}", process::id()));
    let (a, b) = (base.join("a"), base.join("b"));
    let result = run(&a, &b);
    let _ = fs::remove_dir_all(&base);
    result
}

fn run(a: &Path, b: &Path) -> kide::Result<()> {
    let platform = Platform::current();
    let mut a = open(a)?;
    let mut b = open(b)?;

    // A file appears in `a`; a scan records it as a version and moves HEAD.
    fs::write(a.root.join("hello.md"), "Hello from a\n")?;
    let ws = a.workspace();
    let report = scan(&mut a, &ws, &mut Rescan, &platform)?;
    println!(
        "a scanned: {} new version(s), HEAD {}",
        report.versions,
        short(&report.head)
    );

    // `a` serves its HEAD; `b` pulls it. Pulling compares the two roots,
    // fetches only the objects `b` lacks, decides every path that differs
    // (the decision table merges concurrent edits or keeps conflict copies)
    // and writes the result into `b`'s folder.
    let network = InProcess::default();
    let head = a.head;
    network.register("a", net::served(a.store.clone(), move || head));
    let ws = b.workspace();
    let with = Pulling {
        ws: &ws,
        source: &mut Rescan,
        resolver: &DecisionTable,
        transport: &network,
        platform: &platform,
        review: None,
    };
    let report = pull(&mut b, with, "a")?;
    println!(
        "b pulled from a: {} path(s) decided, HEAD {}",
        report.decided,
        short(&report.head)
    );

    let text = fs::read_to_string(b.root.join("hello.md"))?;
    print!("b/hello.md: {text}");
    assert_eq!(report.head, a.head, "equal folders have equal roots");
    Ok(())
}

/// Makes `dir` a Kide folder (it holds its state in `dir/.kide`) and opens it.
fn open(dir: &Path) -> kide::Result<Folder> {
    fs::create_dir_all(dir)?;
    Folder::init(dir)?;
    Folder::open(dir)
}

fn short(id: &kide::hash::Hash) -> String {
    id.to_hex()[..12].to_string()
}
