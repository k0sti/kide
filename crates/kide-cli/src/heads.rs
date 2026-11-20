//! Announcing a folder's root as it changes (§5.8), on the live stream
//! `head` (§4.2): a device's server sends its root to every device that
//! opened the stream, at once and after every change, and a device that
//! follows a peer pulls from it as soon as it announces a root, instead of
//! waiting for `pull_interval`. Announcements only wake pulls early: a pull
//! still compares roots and fetches what it lacks, so a lost one costs
//! time, never correctness.

use kide::error::Result;
use kide::hash::Hash;
use kide::iface::{Live, LiveEvent, LiveEvents, Transport};
use kide::log;
use std::sync::mpsc::{Sender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// The name of the live stream that carries a folder's root.
pub const STREAM: &str = "head";
/// The first wait before following a peer again, doubled up to `RETRY_MAX`.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// What wakes the daemon before its timers (§5.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Something in the folder changed: scan now.
    Scan,
    /// The peer with this index announced a root: pull from it now.
    Pull(usize),
}

/// The server side: the root, and the sessions that follow it.
pub struct Heads {
    state: Mutex<(Hash, Vec<Arc<dyn Live>>)>,
}

impl Heads {
    /// Announces `head` until `publish` changes it; the events go to the
    /// server's `Served::live`.
    pub fn new(head: Hash) -> (Arc<Heads>, LiveEvents) {
        let heads = Arc::new(Heads {
            state: Mutex::new((head, Vec::new())),
        });
        let (tx, rx) = sync_channel::<(Arc<dyn Live>, LiveEvent)>(64);
        let h = heads.clone();
        thread::spawn(move || rx.into_iter().for_each(|(live, e)| h.event(live, e)));
        (heads, tx)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, (Hash, Vec<Arc<dyn Live>>)> {
        // A poisoned lock only means a thread panicked; the state is whole.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn event(&self, live: Arc<dyn Live>, e: LiveEvent) {
        match e {
            LiveEvent::Open { name, .. } if name == STREAM => {
                let mut s = self.state();
                if live.open(STREAM, &s.0.0).is_ok() {
                    s.1.push(live);
                }
            }
            // Refused: a failure means the session ended, and Ended follows.
            LiveEvent::Open { name, .. } => drop(live.close(&name)),
            LiveEvent::Close { .. } | LiveEvent::Ended(_) => {
                let gone = Arc::as_ptr(&live);
                self.state()
                    .1
                    .retain(|l| !std::ptr::addr_eq(Arc::as_ptr(l), gone));
            }
            LiveEvent::Update { .. } => {}
        }
    }

    /// Sends `head` to every follower if it is a new root; a follower whose
    /// session ended is dropped.
    pub fn publish(&self, head: Hash) {
        let mut s = self.state();
        if s.0 != head {
            s.0 = head;
            s.1.retain(|l| l.update(STREAM, &head.0).is_ok());
        }
    }
}

/// The client side: follows the stream of every peer, sending `Pull` with
/// the peer's index whenever it announces a root, on a thread per peer that
/// opens a new session after one ends, waiting longer each time it fails.
/// The threads end when `wake` has no receiver.
pub fn follow(transport: Arc<dyn Transport>, peers: Vec<String>, wake: Sender<Wake>) {
    for (i, addr) in peers.into_iter().enumerate() {
        let (t, wake) = (transport.clone(), wake.clone());
        thread::spawn(move || {
            let mut delay = RETRY_MIN;
            loop {
                let began = Instant::now();
                match session(&*t, &addr, i, &wake) {
                    Ok(false) => return,
                    Ok(true) => {}
                    Err(e) => log!("following {addr}: {e}"),
                }
                if began.elapsed() > RETRY_MAX {
                    delay = RETRY_MIN;
                }
                thread::sleep(delay);
                delay = (delay * 2).min(RETRY_MAX);
            }
        });
    }
}

/// One session following `addr`; false once nothing receives wakes.
fn session(t: &dyn Transport, addr: &str, i: usize, wake: &Sender<Wake>) -> Result<bool> {
    let (tx, rx) = sync_channel(16);
    let live = t.live(addr, tx)?;
    live.open(STREAM, &[])?;
    for (_, e) in rx {
        match e {
            LiveEvent::Open { .. } | LiveEvent::Update { .. } => {
                if wake.send(Wake::Pull(i)).is_err() {
                    let _ = live.bye();
                    return Ok(false);
                }
            }
            LiveEvent::Close { .. } => {
                let _ = live.bye();
                return Ok(true);
            }
            LiveEvent::Ended(e) => return Err(e),
        }
    }
    Ok(true)
}
