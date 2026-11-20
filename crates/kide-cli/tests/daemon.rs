//! The daemon (§5.8): two daemons over plain TCP on loopback carry edits
//! both ways, a corrupt device is reported and applies nothing, and garbage
//! is collected between pulls.
//! Unix only, as the sync fixtures it shares with the library's tests;
//! one test watches the folder.
#![cfg(all(unix, feature = "watch"))]

// Shared with the library's sync tests, which use the rest of it.
#[allow(dead_code)]
#[path = "../../kide/tests/sync/device.rs"]
mod device;
#[path = "../../kide/tests/sync/failures.rs"]
mod failures;

use crate::device::Device;
use crate::failures::{Damaged, deleted_on_b, snapshot};
use kide::change::Rescan;
use kide::conformance::TempDir;
use kide::folder::Folder;
use kide::folder::net;
use kide::hash::Hash;
use kide::iface::Transport;
use kide::names::Platform;
use kide::resolve::DecisionTable;
use kide::transport::{Served, Server, Tcp};
use kide::workspace::PlainFolder;
use kide_cli::daemon::{Daemon, Timers, run};
use kide_cli::heads::{Heads, follow};
use kide_cli::watch::watch_folder;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const FAST: Timers = Timers {
    scan: Duration::from_millis(100),
    pull: Duration::from_millis(100),
    gc: Duration::from_secs(3600),
};

/// Runs the daemon of `folder` on its own thread, pulling from `peer` and
/// publishing HEAD to `head`, until `stop` is set. Any warning fails.
fn daemon(
    mut folder: Folder,
    head: Arc<Mutex<Hash>>,
    peer: String,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let ws = PlainFolder::new(&folder.root);
        let d = Daemon {
            ws: &ws,
            source: &mut Rescan,
            resolver: &DecisionTable,
            transport: &Tcp::default(),
            platform: &Platform::LINUX,
            peers: vec![(peer.clone(), peer)],
            timers: FAST,
            publish: &|h| *head.lock().unwrap() = h,
            warn: &|w| panic!("{w}"),
            wakes: None,
        };
        run(&mut folder, d, &stop);
    })
}

/// Opens the folder and serves it on loopback.
fn served(dir: &Path) -> (Folder, Arc<Mutex<Hash>>, Server) {
    Folder::init(dir).unwrap();
    let folder = Folder::open_unsynced(dir).unwrap();
    let head = Arc::new(Mutex::new(folder.head));
    let h = head.clone();
    let served = net::served(folder.store.clone(), move || *h.lock().unwrap());
    let loopback = vec![([1; 32], "127.0.0.1".parse().unwrap())];
    let server = Server::start("127.0.0.1:0".parse().unwrap(), loopback, served).unwrap();
    (folder, head, server)
}

fn arrives(path: &Path, want: &[u8]) {
    let start = Instant::now();
    while fs::read(path).ok().as_deref() != Some(want) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{path:?} not updated"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn two_daemons_carry_edits_both_ways() {
    let (ta, tb) = (TempDir::new("daemon-a"), TempDir::new("daemon-b"));
    let (fa, ha, sa) = served(ta.path());
    let (fb, hb, sb) = served(tb.path());
    let stop = Arc::new(AtomicBool::new(false));
    let threads = [
        daemon(fa, ha, sb.addr().to_string(), stop.clone()),
        daemon(fb, hb, sa.addr().to_string(), stop.clone()),
    ];
    fs::write(ta.path().join("hello.txt"), b"from a\n").unwrap();
    arrives(&tb.path().join("hello.txt"), b"from a\n");
    fs::write(tb.path().join("hello.txt"), b"from b\n").unwrap();
    arrives(&ta.path().join("hello.txt"), b"from b\n");
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().unwrap();
    }
    // Stopped between pulls: the folders are open again, and the same.
    let heads = [&ta, &tb].map(|t| Folder::open(t.path()).unwrap().head);
    assert_eq!(heads[0], heads[1]);
}

/// A daemon as `kide run` wires it: its server announces every new root on
/// the live stream `head`, it follows the peer's, and a file watcher wakes
/// its scans. Its timers are a minute long, so only those can carry an edit
/// quickly.
fn woken_daemon(
    dir: &Path,
    stop: Arc<AtomicBool>,
) -> (Server, impl FnOnce(String) -> thread::JoinHandle<()>) {
    Folder::init(dir).unwrap();
    let mut folder = Folder::open_unsynced(dir).unwrap();
    let head = Arc::new(Mutex::new(folder.head));
    let (heads, live) = Heads::new(folder.head);
    let h = head.clone();
    let served = Served {
        live: Some(live),
        ..net::served(folder.store.clone(), move || *h.lock().unwrap())
    };
    let loopback = vec![([1; 32], "127.0.0.1".parse().unwrap())];
    let server = Server::start("127.0.0.1:0".parse().unwrap(), loopback, served).unwrap();
    let start = move |peer: String| {
        thread::spawn(move || {
            let (wake, wakes) = channel();
            watch_folder(&folder, wake.clone()).unwrap();
            let transport: Arc<dyn Transport> = Arc::new(Tcp::default());
            follow(transport.clone(), vec![peer.clone()], wake);
            let ws = PlainFolder::new(&folder.root);
            let slow = Duration::from_secs(60);
            let d = Daemon {
                ws: &ws,
                source: &mut Rescan,
                resolver: &DecisionTable,
                transport: &*transport,
                platform: &Platform::LINUX,
                peers: vec![(peer.clone(), peer)],
                timers: Timers {
                    scan: slow,
                    pull: slow,
                    gc: slow,
                },
                publish: &|h| {
                    *head.lock().unwrap() = h;
                    heads.publish(h);
                },
                warn: &|w| panic!("{w}"),
                wakes: Some(&wakes),
            };
            run(&mut folder, d, &stop);
        })
    };
    (server, start)
}

/// How long until `path` holds `want`, at most 5 s.
fn arrival(path: &Path, want: &[u8]) -> Duration {
    let start = Instant::now();
    arrives(path, want);
    start.elapsed()
}

#[test]
fn a_watched_and_announced_edit_arrives_well_under_a_second() {
    let (ta, tb) = (TempDir::new("woken-a"), TempDir::new("woken-b"));
    let stop = Arc::new(AtomicBool::new(false));
    let (sa, start_a) = woken_daemon(ta.path(), stop.clone());
    let (sb, start_b) = woken_daemon(tb.path(), stop.clone());
    let threads = [
        start_a(sb.addr().to_string()),
        start_b(sa.addr().to_string()),
    ];
    // Let both make their first scan and pull, and follow each other.
    thread::sleep(Duration::from_millis(500));
    let mut times = Vec::new();
    for i in 0..3 {
        let text = format!("from a {i}\n");
        fs::write(ta.path().join("note.md"), &text).unwrap();
        times.push(arrival(&tb.path().join("note.md"), text.as_bytes()));
        let text = format!("from b {i}\n");
        fs::write(tb.path().join("note.md"), &text).unwrap();
        times.push(arrival(&ta.path().join("note.md"), text.as_bytes()));
    }
    stop.store(true, Ordering::Relaxed);
    eprintln!("edit to arrival, a and b in turn: {times:?}");
    let slowest = times.iter().max().unwrap();
    assert!(*slowest < Duration::from_millis(1000), "{times:?}");
    // The daemons end at their next wake; the stop flag is checked then.
    drop(threads);
}

#[test]
fn a_corrupt_device_is_reported_and_applies_nothing() {
    let (net, mut a, b) = deleted_on_b();
    let v = b.folder.index.0["kept"].version;
    let damaged = Served {
        store: Arc::new(Damaged(b.folder.store.clone(), v)),
        ..b.served()
    };
    net.0.register("b", damaged);
    let before = snapshot(&a);
    let (stop, warnings) = (AtomicBool::new(false), Mutex::new(Vec::new()));
    let transport: &dyn Transport = &net.0;
    let d = Daemon {
        ws: &*a.ws,
        source: &mut Rescan,
        resolver: &DecisionTable,
        transport,
        platform: &Platform::LINUX,
        peers: vec![("b".into(), "b".into())],
        timers: FAST,
        publish: &|_| {},
        warn: &|w| {
            warnings.lock().unwrap().push(w.to_string());
            stop.store(true, Ordering::Relaxed);
        },
        wakes: None,
    };
    run(&mut a.folder, d, &stop);
    let warnings = warnings.into_inner().unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains(&format!("corrupt object {v}")),
        "{warnings:?}"
    );
    assert_eq!(snapshot(&a), before);
}

#[test]
fn the_daemon_collects_garbage_between_pulls() {
    let mut a = Device::new("daemon-gc");
    let old = a.root().join(".kide/trash/1000");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("x"), b"trashed long ago\n").unwrap();
    let junk = a.folder.store.put(b"no root needs this").unwrap();
    let store = a.folder.store.clone();
    let stop = AtomicBool::new(false);
    let watch = || {
        let start = Instant::now();
        while old.exists() || store.has(&junk).unwrap() {
            assert!(start.elapsed() < Duration::from_secs(5), "no collection");
            thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
    };
    let d = Daemon {
        ws: &*a.ws,
        source: &mut Rescan,
        resolver: &DecisionTable,
        transport: &Tcp::default(),
        platform: &Platform::LINUX,
        peers: vec![],
        timers: Timers {
            gc: Duration::from_millis(50),
            ..FAST
        },
        publish: &|_| {},
        warn: &|w| panic!("{w}"),
        wakes: None,
    };
    thread::scope(|s| {
        s.spawn(watch);
        run(&mut a.folder, d, &stop);
    });
}
