//! `InProcess`: the protocol of §4.2 for tests, its bytes passed through
//! memory to a responder instead of a socket. Peers are registered by name;
//! clones share them.

use super::Served;
use super::driver::{Blocking, serve};
use super::live::{Close, PING, connect};
use super::pipe::{Chan, connection};
use super::tcp::TIMEOUT;
use crate::error::{Error, Result};
use crate::iface::{Live, LiveEvents, Session, Transport};
use crate::log;
use crate::protocol::{Client, Responder};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;

#[derive(Clone, Default)]
pub struct InProcess {
    peers: Arc<Mutex<HashMap<String, Peer>>>,
}

#[derive(Clone)]
struct Peer {
    served: Served,
    /// Cleared when the peer is unregistered: its open sessions break.
    up: Arc<AtomicBool>,
    /// The connections of its live sessions, broken with it.
    conns: Arc<Mutex<Vec<Weak<Chan>>>>,
}

impl Peer {
    fn stop(&self) {
        self.up.store(false, Ordering::Relaxed);
        let conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        conns
            .iter()
            .filter_map(Weak::upgrade)
            .for_each(|c| c.break_off());
    }
}

impl InProcess {
    /// Makes `served` reachable as `name`, replacing an earlier registration.
    pub fn register(&self, name: &str, served: Served) {
        let peer = Peer {
            served,
            up: Arc::new(AtomicBool::new(true)),
            conns: Arc::default(),
        };
        if let Some(old) = self.peers().insert(name.into(), peer) {
            old.stop();
        }
    }

    /// Makes `name` unreachable: open sessions break, new connections fail.
    #[cfg(test)]
    pub(crate) fn unregister(&self, name: &str) {
        if let Some(old) = self.peers().remove(name) {
            old.stop();
        }
    }

    fn peers(&self) -> MutexGuard<'_, HashMap<String, Peer>> {
        // A poisoned lock only means another thread panicked; the map is intact.
        self.peers.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Transport for InProcess {
    fn connect(&self, peer: &str) -> Result<Box<dyn Session>> {
        match self.peers().get(peer) {
            Some(p) => Ok(Box::new(Blocking::new(Pipe {
                responder: Responder::new(p.served.clone(), super::nonce()?),
                up: p.up.clone(),
                pending: Vec::new(),
            }))),
            None => Err(Error::Unavailable(format!("no peer {peer:?}"))),
        }
    }

    /// A live session over an in-memory connection, its server on a thread.
    fn live(&self, peer: &str, events: LiveEvents) -> Result<Arc<dyn Live>> {
        let Some(p) = self.peers().get(peer).cloned() else {
            return Err(Error::Unavailable(format!("no peer {peer:?}")));
        };
        let ((cr, cw), (sr, sw), chans) = connection(TIMEOUT);
        let chans = Arc::new(chans);
        let mut conns = p.conns.lock().unwrap_or_else(|e| e.into_inner());
        conns.retain(|c| c.strong_count() > 0);
        conns.extend(chans.iter().map(Arc::downgrade));
        drop(conns);
        let close = || -> Close {
            let c = chans.clone();
            Box::new(move || c.iter().for_each(|c| c.break_off()))
        };
        let r = Responder::new(p.served.clone(), super::nonce()?);
        let (app, server_close) = (p.served.live.clone(), close());
        thread::spawn(move || {
            if let Err(e) = serve(sr, sw, r, app, server_close, PING) {
                log!("in-process live session: {e}");
            }
        });
        connect(cr, cw, Client::new(), close(), events, PING)
    }
}

/// A connection to a responder in memory: what is written is received at
/// once, and reads take its answers frame by frame. A read with no answer
/// waiting is the end of the stream.
struct Pipe {
    responder: Responder,
    up: Arc<AtomicBool>,
    pending: Vec<u8>,
}

impl Pipe {
    fn up(&self) -> io::Result<()> {
        match self.up.load(Ordering::Relaxed) {
            true => Ok(()),
            false => Err(io::ErrorKind::ConnectionReset.into()),
        }
    }
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.up()?;
        if self.pending.is_empty() {
            match self.responder.transmit()? {
                Some(frame) => self.pending = frame,
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.up()?;
        let events = self.responder.receive(buf)?;
        super::refuse_live(&mut self.responder, events)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{TransportFixture, live, transport};
    use crate::hash::Hash;
    use crate::protocol::PROTOCOL as VERSION;
    use crate::protocol::sample_served as served;

    fn in_process(objects: &[Vec<u8>], root: Hash, protocol: u8) -> TransportFixture {
        let t = InProcess::default();
        t.register("peer", served(objects, root, protocol));
        let stop = t.clone();
        TransportFixture {
            transport: Box::new(t),
            peer: "peer".into(),
            stop: Box::new(move || stop.unregister("peer")),
        }
    }

    #[test]
    fn conformance() {
        transport(&in_process);
    }

    #[test]
    fn live_conformance() {
        live(&in_process);
    }

    #[test]
    fn a_request_before_hello_is_a_protocol_error() {
        let t = InProcess::default();
        t.register("p", served(&[], Hash::ZERO, VERSION));
        let mut s = t.connect("p").unwrap();
        assert!(matches!(s.get(&[Hash::ZERO]), Err(Error::Protocol(_))));
    }
}
