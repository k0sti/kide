//! Conformance suite for live sessions over a `Transport` (§4.2), against a
//! peer whose live sessions run `echo`.

use super::TransportFixture;
use super::transport::MakePeer;
use crate::error::Error;
use crate::hash::Hash;
use crate::iface::{Live, LiveEvent, LiveEvents};
use crate::protocol::PROTOCOL as VERSION;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread;
use std::time::Duration;

/// Updates the peer sends at most on a stream opened as "flood".
const FLOOD: usize = 64;

/// The application of the peer's live sessions. It refuses names under
/// `refused/`, accepts others at version `server` and sends `hello `
/// followed by the client's version, echoes every update, closes a stream
/// on the update `close`, and on a stream opened as `flood` sends updates of
/// 1 MiB, at most `FLOOD`, until one fails.
pub fn echo() -> LiveEvents {
    let (tx, rx) = sync_channel::<(Arc<dyn Live>, LiveEvent)>(16);
    thread::spawn(move || {
        for (live, e) in rx {
            // A failure means the session ended; its Ended event follows.
            let _ = match e {
                LiveEvent::Open { name, .. } if name.starts_with("refused/") => live.close(&name),
                LiveEvent::Open { name, .. } if name == "flood" => {
                    let _ = live.open(&name, b"server");
                    let big = vec![7; 1 << 20];
                    let _ = (0..FLOOD).try_for_each(|_| live.update(&name, &big));
                    Ok(())
                }
                LiveEvent::Open { name, version } => live
                    .open(&name, b"server")
                    .and_then(|_| live.update(&name, &[b"hello ", &version[..]].concat())),
                LiveEvent::Update { name, bytes } if bytes == b"close" => live.close(&name),
                LiveEvent::Update { name, bytes } => live.update(&name, &bytes),
                LiveEvent::Close { .. } | LiveEvent::Ended(_) => Ok(()),
            };
        }
    });
    tx
}

type Events = Receiver<(Arc<dyn Live>, LiveEvent)>;
type Check = fn(&TransportFixture);

/// Runs every check against a peer from `make`.
pub fn live(make: &MakePeer) {
    let fx = make(&[], Hash::ZERO, VERSION);
    let checks: [(&str, Check); 7] = [
        ("a stream opens, echoes and closes", streams),
        ("the server refuses and closes streams", refusals),
        ("updates arrive in order both ways", order),
        ("sessions share one channel", shared),
        ("a full queue ends the session", overflow),
        ("bye ends the session", bye),
        ("a stopped peer ends the session", stopped),
    ];
    for (name, check) in checks {
        eprintln!("live: {name}");
        check(&fx);
    }
    eprintln!("live: another protocol is incompatible");
    let other = make(&[], Hash::ZERO, VERSION + 1);
    let (tx, _rx) = sync_channel(16);
    let r = other.transport.live(&other.peer, tx);
    assert!(
        matches!(r, Err(Error::Incompatible(_))),
        "live with protocol {}: expected Incompatible, got {:?}",
        VERSION + 1,
        r.err()
    );
    (other.stop)();
}

fn session(fx: &TransportFixture) -> (Arc<dyn Live>, Events) {
    let (tx, rx) = sync_channel(16);
    let live = fx.transport.live(&fx.peer, tx);
    (live.unwrap_or_else(|e| panic!("live: {e}")), rx)
}

fn next(rx: &Events) -> (Arc<dyn Live>, LiveEvent) {
    rx.recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|e| panic!("no live event: {e}"))
}

/// Whether the next event is `want`, compared by what they print.
fn expect(rx: &Events, want: LiveEvent) {
    let (_, got) = next(rx);
    assert_eq!(format!("{got:?}"), format!("{want:?}"));
}

fn open(name: &str, version: &[u8]) -> LiveEvent {
    LiveEvent::Open {
        name: name.into(),
        version: version.to_vec(),
    }
}

fn update(name: &str, bytes: &[u8]) -> LiveEvent {
    LiveEvent::Update {
        name: name.into(),
        bytes: bytes.to_vec(),
    }
}

fn close(name: &str) -> LiveEvent {
    LiveEvent::Close { name: name.into() }
}

fn ended(rx: &Events) -> Error {
    match next(rx).1 {
        LiveEvent::Ended(e) => e,
        e => panic!("expected the end, got {e:?}"),
    }
}

fn streams(fx: &TransportFixture) {
    let (live, rx) = session(fx);
    assert_eq!(live.peer(), None, "a client knows no key of the server");
    live.open("doc", b"7").unwrap();
    expect(&rx, open("doc", b"server"));
    expect(&rx, update("doc", b"hello 7"));
    live.update("doc", b"edit").unwrap();
    expect(&rx, update("doc", b"edit"));
    for _ in 0..200 {
        if live.queued() == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(live.queued(), 0, "the writer takes what is queued");
    live.close("doc").unwrap();
    expect(&rx, close("doc"));
    let r = live.update("doc", b"late");
    assert!(matches!(r, Err(Error::Protocol(_))), "closed: {r:?}");
    live.open("doc", b"8").unwrap();
    expect(&rx, open("doc", b"server"));
    live.bye().unwrap();
}

fn refusals(fx: &TransportFixture) {
    let (live, rx) = session(fx);
    live.open("refused/x", b"").unwrap();
    expect(&rx, close("refused/x"));
    live.open("doc", b"").unwrap();
    expect(&rx, open("doc", b"server"));
    expect(&rx, update("doc", b"hello "));
    live.update("doc", b"close").unwrap();
    expect(&rx, close("doc"));
    live.bye().unwrap();
}

fn order(fx: &TransportFixture) {
    let (live, rx) = session(fx);
    for name in ["a", "b"] {
        live.open(name, b"").unwrap();
        expect(&rx, open(name, b"server"));
        expect(&rx, update(name, b"hello "));
    }
    let n = 500;
    for i in 0..n {
        let name = ["a", "b"][i % 2];
        live.update(name, &i.to_le_bytes()).unwrap();
    }
    for i in 0..n {
        let name = ["a", "b"][i % 2];
        expect(&rx, update(name, &i.to_le_bytes()));
    }
    live.bye().unwrap();
}

fn shared(fx: &TransportFixture) {
    let (tx, rx) = sync_channel(16);
    let one = fx.transport.live(&fx.peer, tx.clone()).unwrap();
    let two = fx.transport.live(&fx.peer, tx).unwrap();
    one.open("x", b"1").unwrap();
    let (from, _) = next(&rx);
    assert!(Arc::ptr_eq(&from, &one), "the event names its session");
    next(&rx);
    two.open("x", b"2").unwrap();
    let (from, _) = next(&rx);
    assert!(Arc::ptr_eq(&from, &two));
    let (_, e) = next(&rx);
    assert_eq!(format!("{e:?}"), format!("{:?}", update("x", b"hello 2")));
    one.bye().unwrap();
    two.bye().unwrap();
}

/// The client reads nothing for a while, so the peer's queue fills while it
/// floods, and the peer ends the session instead of queueing without bound.
fn overflow(fx: &TransportFixture) {
    let (tx, rx) = sync_channel(1);
    let live = fx.transport.live(&fx.peer, tx).unwrap();
    live.open("flood", b"").unwrap();
    thread::sleep(Duration::from_millis(300));
    // What was queued before the overflow may be lost with it, the open
    // included: the peer can fill its queue before it sends anything.
    let mut updates = 0;
    let end = loop {
        match next(&rx).1 {
            LiveEvent::Open { name, .. } if name == "flood" && updates == 0 => {}
            LiveEvent::Update { .. } => updates += 1,
            LiveEvent::Ended(e) => break e,
            e => panic!("expected the open, updates, then the end, got {e:?}"),
        }
    };
    assert!(updates < FLOOD, "all {updates} updates were queued");
    assert!(matches!(end, Error::Unavailable(_)), "{end:?}");
    let r = live.update("flood", b"x");
    assert!(
        matches!(r, Err(Error::Unavailable(_))),
        "after the end: {r:?}"
    );
}

fn bye(fx: &TransportFixture) {
    let (live, rx) = session(fx);
    live.open("doc", b"").unwrap();
    expect(&rx, open("doc", b"server"));
    live.bye().unwrap();
    let mut e = next(&rx).1;
    if matches!(e, LiveEvent::Update { .. }) {
        e = next(&rx).1; // the catch-up, if it came before Bye
    }
    assert!(
        matches!(e, LiveEvent::Ended(Error::Unavailable(_))),
        "{e:?}"
    );
    assert!(live.update("doc", b"x").is_err(), "after bye");
}

/// Runs last.
fn stopped(fx: &TransportFixture) {
    let (live, rx) = session(fx);
    live.open("doc", b"").unwrap();
    expect(&rx, open("doc", b"server"));
    expect(&rx, update("doc", b"hello "));
    (fx.stop)();
    let e = ended(&rx);
    assert!(matches!(e, Error::Unavailable(_)), "{e:?}");
    let (tx, _rx) = sync_channel(1);
    match fx.transport.live(&fx.peer, tx) {
        Err(Error::Unavailable(_)) => {}
        r => panic!(
            "live to a stopped peer: expected Unavailable, got {:?}",
            r.err()
        ),
    }
}
