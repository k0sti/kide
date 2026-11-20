//! The local folder commands, garbage collection, and serving, pulling
//! and the daemon.

use crate::args::{Args, Out, failed, hash_arg, usage};
use kide::change::Rescan;
use kide::folder::Folder;
use kide::folder::config::{Config, Mode};
use kide::folder::guard::{self, Deletions};
use kide::folder::net;
use kide::folder::npub::Key;
use kide::folder::pull::Pulling;
use kide::format::manifest::Kind;
use kide::hash::Hash;
use kide::history::{Event, graph, history};
use kide::iface::Resolver;
use kide::load::{node, read_manifest, record};
use kide::names::{Platform, held};
use kide::report::{Why, reported_paths};
use kide::resolve::{DecisionTable, Join, Reset};
use kide::time::now_secs;
use kide::transport::Served;
use kide::workspace::PlainFolder;
use kide_cli::daemon::{Daemon, Timers};
use kide_cli::heads::{Heads, Wake, follow};
use kide_cli::network::Network;
use kide_cli::quic::{self, Serving};
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

pub fn init(a: &Args) -> Out<()> {
    a.expect(0..=1, &[])?;
    let dir = a.pos.first().map_or_else(|| a.dir(), Into::into);
    fs::create_dir_all(&dir)?;
    Folder::init(&dir)?;
    let (key, pin) = quic::init(&dir.join(".kide"))?;
    println!(
        "initialised {}\nkey {key}\ncertificate {pin}",
        dir.display()
    );
    Ok(())
}

pub fn move_path(a: &Args) -> Out<()> {
    a.expect(2..=2, &[])?;
    let mut folder = Folder::open(&a.dir())?;
    let ws = folder.workspace();
    kide::folder::scan::scan(&mut folder, &ws, &mut Rescan, &Platform::current())?;
    let head = kide::folder::moves::move_path(&mut folder, &ws, &a.pos[0], &a.pos[1])?;
    println!("head {head}");
    Ok(())
}

pub fn restore_tree(a: &Args) -> Out<()> {
    a.expect(1..=1, &["replace"])?;
    if !a.flag("replace") {
        return usage("restore-tree requires --replace: accept replacing intervening edits");
    }
    let earlier = hash_arg(&a.pos[0])?;
    let mut folder = Folder::open(&a.dir())?;
    let ws = folder.workspace();
    kide::folder::scan::scan(&mut folder, &ws, &mut Rescan, &Platform::current())?;
    let expected = folder.head;
    let report = kide::folder::restore::restore_tree(&mut folder, &ws, &earlier, &expected)?;
    println!(
        "written {}\ntrashed {}\nhead {}",
        report.written, report.trashed, report.head
    );
    for p in &report.skipped {
        println!("left {p}");
    }
    if !report.skipped.is_empty() {
        return failed("restore left raced paths; review them before retrying");
    }
    Ok(())
}

pub fn renew_tls(a: &Args) -> Out<()> {
    a.expect(0..=0, &[])?;
    let folder = Folder::open(&a.dir())?;
    println!("certificate {}", quic::renew(&folder)?);
    Ok(())
}

pub fn scan(a: &Args) -> Out<()> {
    a.expect(0..=0, &[])?;
    let mut folder = Folder::open(&a.dir())?;
    let ws = folder.workspace();
    let r = kide::folder::scan::scan(&mut folder, &ws, &mut Rescan, &Platform::current())?;
    println!(
        "versions {}\ntombstones {}\nmetadata {}\nraced {}\nopened {}",
        r.versions, r.tombstones, r.metadata, r.raced, r.opened
    );
    unrepresented();
    println!("head {}", r.head);
    Ok(())
}

/// The status lines of §5.3: every skipped name, held entry and conflict
/// copy not made, reported while this program ran.
fn unrepresented() {
    let all = [
        (Why::Skipped, "skipped"),
        (Why::Held, "held"),
        (Why::NotCopied, "not-copied"),
    ];
    for (why, what) in all {
        for p in reported_paths(why) {
            println!("{what} {p}");
        }
    }
}

/// Pulls from one configured device, or from all of them in turn (§5.6).
pub fn pull(a: &Args) -> Out<()> {
    pull_with(a, &DecisionTable, Deletions::Guarded, false)
}

/// Resets the folder to one device's state (§5.6): local files the scan
/// misses are written back first, then every path takes the device's entry.
pub fn reset(a: &Args) -> Out<()> {
    pull_with(a, &Reset, Deletions::Restore, true)
}

/// Joins a device's folder without local history (§5.6): where they
/// differ, the device's version wins and the local one is kept as a copy.
pub fn join(a: &Args) -> Out<()> {
    pull_with(a, &Join, Deletions::Guarded, true)
}

/// Confirms the deletions a guarded scan refused (§5.5): the next scan at
/// this `HEAD` records them. Works while a daemon holds the folder.
pub fn confirm(a: &Args) -> Out<()> {
    a.expect(0..=0, &[])?;
    let folder = Folder::open_read_only(&a.dir())?;
    guard::confirm(&folder.kide(), &folder.head)?;
    println!(
        "confirmed the deletions of the next scan at {}",
        folder.head
    );
    Ok(())
}

/// Writes the files a scan finds gone back from the store (§5.5): HEAD
/// stays, nothing is deleted.
pub fn restore(a: &Args) -> Out<()> {
    a.expect(0..=0, &[])?;
    let mut folder = Folder::open(&a.dir())?;
    folder.deletions = Deletions::Restore;
    let ws = folder.workspace();
    let r = kide::folder::scan::scan(&mut folder, &ws, &mut Rescan, &Platform::current())?;
    println!("written {}\nversions {}", r.written, r.versions);
    unrepresented();
    println!("head {}", r.head);
    Ok(())
}

fn pull_with(a: &Args, resolver: &dyn Resolver, deletions: Deletions, one: bool) -> Out<()> {
    a.expect(0..=1, &[])?;
    let mut folder = Folder::open(&a.dir())?;
    if folder.config.mode == Mode::Send {
        return failed("a send folder never pulls (mode = send)");
    }
    if deletions != Deletions::Guarded {
        folder.deletions = deletions;
    }
    let peers = peers(&folder, a.pos.first(), one)?;
    let transport = quic::client(&folder)?;
    let choice = folder.config.transport;
    let ws = folder.workspace();
    let mut failures = 0;
    for (key, addr) in peers {
        let with = Pulling {
            ws: &ws,
            source: &mut Rescan,
            resolver,
            transport: &*transport,
            platform: &Platform::current(),
            review: None,
        };
        match kide::folder::pull::pull(&mut folder, with, &quic::address(choice, addr)) {
            Ok(r) => {
                let x = &r.apply;
                println!(
                    "pulled {key}: decided {} written {} trashed {} metadata {} moved {} left {} rounds {}",
                    r.decided,
                    x.written,
                    x.trashed,
                    x.metadata,
                    x.moved,
                    x.skipped.len(),
                    r.fetch.as_ref().map_or(0, |f| f.rounds)
                );
                for p in &x.skipped {
                    println!("left {p}");
                }
                for p in &r.merged {
                    println!("merged {p}");
                }
            }
            Err(e) => {
                eprintln!("kide: pull from {key}: {e}");
                failures += 1;
            }
        }
    }
    unrepresented();
    println!("head {}", folder.head);
    match failures {
        0 => Ok(()),
        n => failed(format!("{n} of the pulls failed")),
    }
}

/// One line per manifest entry: kind, size, mtime, version and path; then
/// the live entries this device holds (§5.3).
pub fn ls(a: &Args) -> Out<()> {
    a.expect(0..=0, &["live"])?;
    let folder = Folder::open_read_only(&a.dir())?;
    let store = &*folder.store;
    let mut live = Vec::new();
    for e in read_manifest(store, &folder.head)? {
        let kind = match Kind::of(&record(store, &e.version)?)? {
            None if a.flag("live") => continue,
            None => "deleted",
            Some(Kind::Regular) => "file",
            Some(Kind::Executable) => "exec",
            Some(Kind::Symlink) => "link",
            Some(Kind::Directory) => "dir",
        };
        println!("{kind} {} {} {} {}", e.size, e.mtime, e.version, e.path);
        if kind != "deleted" {
            live.push(e.path);
        }
    }
    let on_disk = |p: &str| folder.index.0.get(p).is_some_and(|e| e.disk.is_some());
    let live = live.iter().map(|p| (p.as_str(), on_disk(p)));
    let (held, mismatch) = held(live, &Platform::current());
    for p in held {
        println!("held {p}");
    }
    for p in mismatch {
        println!("mismatch {p}");
    }
    Ok(())
}

/// The history of one path in HEAD (§3), newest first: lanes, the first 12
/// hex digits of the version, how it came about, its content size, and a
/// merge's parents.
pub fn log(a: &Args) -> Out<()> {
    a.expect(1..=1, &[])?;
    let folder = Folder::open_read_only(&a.dir())?;
    let store = &*folder.store;
    let path = &a.pos[0];
    let entries = read_manifest(store, &folder.head)?;
    let Some(e) = entries.iter().find(|e| e.path == *path) else {
        return failed(format!("{path} is not in HEAD"));
    };
    let rows = history(store, &e.version)?;
    let lanes: Vec<String> = rows.iter().map(graph).collect();
    let width = lanes.iter().map(String::len).max().unwrap_or(0);
    let short = |id: &Hash| id.to_hex()[..12].to_string();
    for (r, lanes) in rows.iter().zip(&lanes) {
        let size = match r.record.is_tombstone() {
            true => "-".to_string(),
            false => node(store, &r.record.root)?.bytes().to_string(),
        };
        let mut line = format!("{lanes:width$}  {} {} {size}", short(&r.id), r.event.name());
        if r.event == Event::Merge {
            r.record
                .parents
                .iter()
                .for_each(|p| line += &format!(" {}", short(p)));
        }
        match kide::format::change::Change::decode(&r.record.change)? {
            Some(kide::format::change::Change::Move { from, to }) => {
                line += &format!(" {from:?} -> {to:?}")
            }
            Some(kide::format::change::Change::Restore(tree)) => {
                line += &format!(" tree {}", short(&tree))
            }
            None => {}
        }
        println!("{line}");
    }
    Ok(())
}

/// The configured devices, or the one named; with `one`, exactly one.
fn peers(folder: &Folder, name: Option<&String>, one: bool) -> Out<Vec<(Key, SocketAddr)>> {
    let net = Network::of(&folder.config)?;
    let mut peers = net.peers.clone();
    if let Some(name) = name {
        let Ok(key) = Key::from_npub(name) else {
            return usage(format!("not a device key: {name:?}"));
        };
        peers.retain(|p| p.0 == key);
        if peers.is_empty() {
            return failed(format!("{name} is not a configured device"));
        }
    }
    if one && peers.len() != 1 {
        return usage(format!("{} devices are configured: name one", peers.len()));
    }
    Ok(peers)
}

/// Verifies the store; with `--repair`, drops what is corrupt (§4.1) and
/// fetches what the kept roots lack from a device.
pub fn verify(a: &Args) -> Out<()> {
    match a.flag("repair") {
        true => a.expect(0..=1, &["repair"])?,
        false => a.expect(0..=0, &[])?,
    }
    if a.flag("repair") {
        let folder = Folder::open(&a.dir())?;
        kide::folder::verify::verify(&*folder.store, &folder.head)?;
        let (key, addr) = peers(&folder, a.pos.first(), true)?[0];
        let transport = quic::client(&folder)?;
        let mut session = transport.connect(&quic::address(folder.config.transport, addr))?;
        session.hello(&folder.head)?;
        let roots = kide::folder::repair::kept(&folder)?;
        let r = kide::folder::repair::repair(&folder, &mut *session, &roots)?;
        session.bye()?;
        println!(
            "repaired from {key}: fetched {} absent {}",
            r.fetched, r.absent
        );
    }
    let folder = Folder::open_read_only(&a.dir())?;
    let r = kide::folder::verify::verify(&*folder.store, &folder.head)?;
    println!("objects {}", r.objects);
    for id in &r.corrupt {
        println!("corrupt {id}");
    }
    for p in &r.problems {
        println!("problem {p}");
    }
    match r.ok() {
        true => Ok(()),
        false => failed("verification found problems"),
    }
}

pub fn checkout(a: &Args) -> Out<()> {
    a.expect(2..=2, &[])?;
    let folder = Folder::open_read_only(&a.dir())?;
    let root = match a.pos[0].as_str() {
        "HEAD" => folder.head,
        hex => hash_arg(hex)?,
    };
    let target = Path::new(&a.pos[1]);
    if target.exists() && fs::read_dir(target)?.next().is_some() {
        return failed(format!("{} is not empty", target.display()));
    }
    fs::create_dir_all(target)?;
    let n = kide::folder::checkout::checkout(&*folder.store, &root, &PlainFolder::new(target))?;
    println!("written {n}");
    Ok(())
}

/// Serves HEAD (§4.2), holding the lock so that HEAD stays put.
pub fn serve(a: &Args) -> Out<()> {
    a.expect(0..=0, &[])?;
    let folder = Folder::open(&a.dir())?;
    let net = Network::of(&folder.config)?;
    warn_fips(&folder.config, net.listen);
    let head = folder.head;
    let served = net::served(folder.store.clone(), move || head);
    let server = Serving::start(&folder, &net, served)?;
    println!("serving {head} on {}", server.addr());
    server.wait();
    Ok(())
}

/// The daemon (§5.8): serves the HEAD it publishes, scans and pulls on the
/// configured intervals, until the process is killed.
pub fn run(a: &Args) -> Out<()> {
    a.expect(0..=0, &[])?;
    let mut folder = Folder::open(&a.dir())?;
    let net = Network::of(&folder.config)?;
    warn_fips(&folder.config, net.listen);
    let head = Arc::new(Mutex::new(folder.head));
    let shared = head.clone();
    // The live stream `head` announces every new root to the devices (§5.8).
    let (heads, live) = Heads::new(folder.head);
    let served = Served {
        live: Some(live),
        ..net::served(folder.store.clone(), move || *lock(&shared))
    };
    let server = Serving::start(&folder, &net, served)?;
    println!("running {} on {}", folder.head, server.addr());
    let ws = folder.workspace();
    // A send folder serves and scans, and pulls from no one.
    let pulled = match folder.config.mode {
        Mode::Sync => &net.peers[..],
        Mode::Send => &[],
    };
    let (wake, wakes) = channel();
    watch(&folder, wake.clone());
    let transport = quic::client(&folder)?;
    follow(
        transport.clone(),
        pulled
            .iter()
            .map(|p| quic::address(folder.config.transport, p.1))
            .collect(),
        wake,
    );
    let peers = pulled
        .iter()
        .map(|(k, a)| (k.to_string(), quic::address(folder.config.transport, *a)));
    let d = Daemon {
        ws: &ws,
        source: &mut Rescan,
        resolver: &DecisionTable,
        transport: &*transport,
        platform: &Platform::current(),
        peers: peers.collect(),
        timers: Timers::of(&folder.config),
        publish: &|h| {
            *lock(&head) = h;
            heads.publish(h);
        },
        warn: &|w| eprintln!("kide: {w}"),
        wakes: Some(&wakes),
    };
    kide_cli::daemon::run(&mut folder, d, &AtomicBool::new(false));
    Ok(())
}

/// Starts the file watcher; without one the daemon scans only every
/// `scan_interval`.
#[cfg(feature = "watch")]
fn watch(folder: &Folder, wake: Sender<Wake>) {
    if let Err(e) = kide_cli::watch::watch_folder(folder, wake) {
        eprintln!("kide: no file watcher ({e}); scanning every scan_interval");
    }
}

#[cfg(not(feature = "watch"))]
fn watch(_: &Folder, _: Sender<Wake>) {}

fn lock(head: &Mutex<Hash>) -> std::sync::MutexGuard<'_, Hash> {
    // A poisoned lock only means another thread panicked; a hash is intact.
    head.lock().unwrap_or_else(|e| e.into_inner())
}

/// The start-up warning of §4.2 for a FIPS server.
#[cfg(feature = "fips")]
fn warn_fips(c: &Config, listen: SocketAddr) {
    use kide::folder::config::Transport;
    if let (Transport::Fips, std::net::IpAddr::V6(addr)) = (c.transport, listen.ip()) {
        eprintln!("kide: {}", kide_cli::fips::warning(&addr));
    }
}

#[cfg(not(feature = "fips"))]
fn warn_fips(_: &Config, _: SocketAddr) {}

/// Garbage collection (§5.7); refused while a daemon holds the lock.
pub fn gc(a: &Args) -> Out<()> {
    a.expect(0..=0, &[])?;
    let folder = Folder::open(&a.dir())?;
    let r = kide::folder::gc::gc(&folder, now_secs())?;
    println!(
        "kept {}\nremoved {}\nroots dropped {}\nunstaged {}\ntrash removed {}",
        r.kept, r.removed, r.roots_dropped, r.unstaged, r.trash_removed
    );
    Ok(())
}
