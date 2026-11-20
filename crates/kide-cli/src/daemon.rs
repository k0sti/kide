//! `kide run` (§5.8): scans every `scan_interval`, pulls from every device
//! every `pull_interval` and collects garbage once a day, on one thread, so
//! scans, pulls, collections and their writes never overlap; whoever starts
//! it serves the folder meanwhile, on other threads that only read the
//! store and the published `HEAD`.
//!
//! An `Unavailable` device is retried after a delay that starts at
//! `pull_interval` and doubles up to 5 minutes, reset by a success; a
//! corrupt object backs off the same way and is always reported; any other
//! failure is reported and retried after `pull_interval`. A pull applies
//! nothing unless it succeeds, and writes `HEAD` last, so the daemon may be
//! killed at any moment.

use crate::heads::Wake;
use kide::error::{Error, Result};
use kide::folder::Folder;
use kide::folder::config::Config;
use kide::folder::gc::gc;
use kide::folder::pull::{Pulling, pull};
use kide::folder::scan::scan;
use kide::hash::Hash;
use kide::iface::{ChangeSource, Resolver, Transport, Workspace};
use kide::log;
use kide::names::Platform;
use kide::time::now_secs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// The longest wait before an unreachable device is tried again.
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// How often the daemon collects garbage (§5.8).
pub const GC_INTERVAL: Duration = Duration::from_secs(86_400);
/// How often the stop flag is checked while waiting.
const TICK: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timers {
    pub scan: Duration,
    pub pull: Duration,
    pub gc: Duration,
}

impl Timers {
    /// The intervals of the configuration, and a day between collections;
    /// tests use shorter ones.
    pub fn of(c: &Config) -> Timers {
        Timers {
            scan: Duration::from_secs(c.scan_interval),
            pull: Duration::from_secs(c.pull_interval),
            gc: GC_INTERVAL,
        }
    }
}

/// What the daemon works with besides the folder.
pub struct Daemon<'a> {
    pub ws: &'a dyn Workspace,
    pub source: &'a mut dyn ChangeSource,
    pub resolver: &'a dyn Resolver,
    pub transport: &'a dyn Transport,
    pub platform: &'a Platform,
    /// Every device: a name for reports, and the address pulled from.
    pub peers: Vec<(String, String)>,
    pub timers: Timers,
    /// Receives `HEAD` after every scan and pull, for the server.
    pub publish: &'a dyn Fn(Hash),
    /// Receives what is reported whether or not logging is enabled.
    pub warn: &'a dyn Fn(&str),
    /// What wakes the daemon before its timers: a change in the folder, a
    /// peer's new root (§5.8); without it the daemon only polls.
    pub wakes: Option<&'a Receiver<Wake>>,
}

/// The next retry delay after a failure: doubled, from `base` up to 5 minutes.
pub fn backoff(previous: Option<Duration>, base: Duration) -> Duration {
    previous
        .map_or(base, |d| (d * 2).max(base))
        .min(MAX_BACKOFF.max(base))
}

/// One device's schedule.
pub struct Peer {
    pub name: String,
    pub addr: String,
    pub next: Instant,
    pub backoff: Option<Duration>,
}

impl Peer {
    pub fn new(name: String, addr: String, now: Instant) -> Peer {
        Peer {
            name,
            addr,
            next: now,
            backoff: None,
        }
    }

    /// Schedules the next pull after one that ended with `r` at `now`, and
    /// returns what to report, if anything.
    pub fn done(&mut self, r: Result<()>, base: Duration, now: Instant) -> Option<String> {
        let mut back = || {
            let d = backoff(self.backoff, base);
            self.backoff = Some(d);
            d
        };
        let (delay, warning) = match r {
            Ok(()) => {
                self.backoff = None;
                (base, None)
            }
            Err(Error::Unavailable(m)) => {
                let d = back();
                log!("{} is unavailable ({m}); retrying in {d:?}", self.name);
                (d, None)
            }
            Err(Error::Corrupt(id)) => {
                let w = format!(
                    "pull from {}: corrupt object {id}, sent by the device or damaged here; \
                     nothing from this pull was applied",
                    self.name
                );
                (back(), Some(w))
            }
            Err(e @ Error::Incompatible(_)) => {
                // Nothing changes until one side is updated: try rarely.
                self.backoff = Some(MAX_BACKOFF);
                (MAX_BACKOFF, Some(format!("pull from {}: {e}", self.name)))
            }
            Err(e) => (base, Some(format!("pull from {}: {e}", self.name))),
        };
        self.next = now + delay;
        warning
    }
}

/// Runs until `stop` is set.
pub fn run(folder: &mut Folder, d: Daemon, stop: &AtomicBool) {
    let start = Instant::now();
    let mut peers: Vec<Peer> = d
        .peers
        .iter()
        .map(|(name, addr)| Peer::new(name.clone(), addr.clone(), start))
        .collect();
    let (mut next_scan, mut next_gc) = (start, start + d.timers.gc);
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= next_scan {
            match scan(folder, d.ws, d.source, d.platform) {
                Ok(_) => (d.publish)(folder.head),
                Err(e) => (d.warn)(&format!("scan: {e}")),
            }
            next_scan = Instant::now() + d.timers.scan;
        }
        for p in peers.iter_mut().filter(|p| now >= p.next) {
            let with = Pulling {
                ws: d.ws,
                source: &mut *d.source,
                resolver: d.resolver,
                transport: d.transport,
                platform: d.platform,
                review: None,
            };
            let began = Instant::now();
            let r = pull(folder, with, &p.addr);
            // A pull scans first, so HEAD may have moved even if it failed.
            (d.publish)(folder.head);
            if let Ok(r) = &r {
                let f = r.fetch.clone().unwrap_or_default();
                log!(
                    "pulled from {} in {:?}: {} paths decided; {} rounds, {} pushed, {} requested, {} bytes",
                    p.name,
                    began.elapsed(),
                    r.decided,
                    f.rounds,
                    f.pushed,
                    f.requested,
                    f.bytes
                );
                for path in &r.merged {
                    (d.warn)(&format!("merged {path} with {}'s changes", p.name));
                }
            }
            if let Some(w) = p.done(r.map(drop), d.timers.pull, Instant::now()) {
                (d.warn)(&w);
            }
        }
        if Instant::now() >= next_gc {
            match gc(folder, now_secs()) {
                Ok(r) => log!("gc: kept {}, removed {}", r.kept, r.removed),
                Err(e) => (d.warn)(&format!("gc: {e}")),
            }
            next_gc = Instant::now() + d.timers.gc;
        }
        let first = next_scan.min(next_gc);
        let at = peers.iter().map(|p| p.next).fold(first, Instant::min);
        let wait = at.saturating_duration_since(Instant::now()).min(TICK);
        match d.wakes {
            Some(rx) => woken(rx, wait, &mut next_scan, &mut peers),
            None => sleep(wait),
        }
    }
}

/// Waits up to `wait` for wakes, and brings forward the scan or the pulls
/// they ask for; takes every wake already queued, so a burst of changes
/// makes one scan. A pull asked for by a peer's announcement runs even
/// while that peer is backing off: the announcement says it is reachable.
fn woken(rx: &Receiver<Wake>, wait: Duration, next_scan: &mut Instant, peers: &mut [Peer]) {
    let first = match rx.recv_timeout(wait) {
        Ok(w) => w,
        Err(RecvTimeoutError::Timeout) => return,
        Err(RecvTimeoutError::Disconnected) => return sleep(wait),
    };
    let now = Instant::now();
    for w in std::iter::once(first).chain(rx.try_iter()) {
        match w {
            Wake::Scan => *next_scan = now,
            Wake::Pull(i) => {
                if let Some(p) = peers.get_mut(i) {
                    p.next = now;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_five_minutes() {
        let base = Duration::from_secs(10);
        assert_eq!(backoff(None, base), base);
        assert_eq!(backoff(Some(base), base), Duration::from_secs(20));
        assert_eq!(
            backoff(Some(Duration::from_secs(40)), base),
            Duration::from_secs(80)
        );
        assert_eq!(backoff(Some(Duration::from_secs(200)), base), MAX_BACKOFF);
        assert_eq!(backoff(Some(MAX_BACKOFF), base), MAX_BACKOFF);
        let long = Duration::from_secs(600);
        assert_eq!(backoff(None, long), long, "never shorter than the interval");
    }

    #[test]
    fn a_device_is_scheduled_by_how_its_pull_ended() {
        let (base, t) = (Duration::from_millis(100), Instant::now());
        let mut p = Peer::new("b".into(), "addr".into(), t);
        let unreachable = || Err(Error::Unavailable("refused".into()));
        assert_eq!(p.done(unreachable(), base, t), None);
        assert_eq!(p.next, t + base);
        assert_eq!(p.done(unreachable(), base, t), None);
        assert_eq!(p.next, t + 2 * base);
        let w = p.done(Err(Error::Corrupt(Hash([1; 32]))), base, t).unwrap();
        assert!(w.contains("corrupt object") && w.contains("nothing"), "{w}");
        assert_eq!(p.next, t + 4 * base, "a corrupt device backs off too");
        let w = p.done(Err(Error::Protocol("odd".into())), base, t).unwrap();
        assert!(w.starts_with("pull from b: protocol error"), "{w}");
        assert_eq!(p.next, t + base, "other failures retry after the interval");
        assert_eq!(p.done(Ok(()), base, t), None);
        assert_eq!((p.next, p.backoff), (t + base, None), "a success resets");
        p.done(unreachable(), base, t);
        assert_eq!(p.next, t + base, "and the backoff starts again");
    }
}
