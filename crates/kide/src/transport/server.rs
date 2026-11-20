//! The TCP server (§4.2): accepts a connection only from a configured
//! device's address, or, behind a gateway, from anyone and identifies each
//! by the challenge; serves each on its own thread until Bye.

use super::Served;
use super::driver::serve;
use super::live::PING;
use super::net_err;
use super::tcp::{TIMEOUT, closer, prepare};
use crate::auth::PeerKey;
use crate::error::{Error, Result};
use crate::log;
use crate::protocol::Responder;
use std::collections::HashMap;
use std::io::{BufWriter, ErrorKind};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The device a connection from `source` comes from, if any: only those are
/// served (§4.2), and the device's key is the peer read rights see. In the
/// FIPS transport the addresses are FIPS addresses, which FIPS rebuilds from
/// the authenticated session.
pub fn device(source: IpAddr, devices: &[(PeerKey, IpAddr)]) -> Option<PeerKey> {
    let source = source.to_canonical();
    let found = devices.iter().find(|d| d.1.to_canonical() == source);
    found.map(|d| d.0)
}

/// How often the accept loop looks at the stop flag.
const POLL: Duration = Duration::from_millis(20);

type Conns = Arc<Mutex<HashMap<u64, TcpStream>>>;

/// Whom a TCP server serves.
enum Trust {
    /// Connections from these addresses, each the device of its key.
    Devices(Vec<(PeerKey, IpAddr)>),
    /// Every connection, identified by the challenge alone.
    Challenge,
}

/// A running server; stopped when dropped.
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    conns: Conns,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Listens on `listen` and serves `served` to connections from
    /// `devices`, each a key and its address.
    pub fn start(
        listen: SocketAddr,
        devices: Vec<(PeerKey, IpAddr)>,
        served: Served,
    ) -> Result<Server> {
        Server::listen(listen, Trust::Devices(devices), served)
    }

    /// Listens on `listen` and serves every connection, each identified by
    /// the challenge alone (`served.admission`, required; §4.2): an
    /// application's server behind a gateway, which relays sessions of the
    /// QUIC transport to it. Listen where only the gateway reaches (on
    /// loopback): the challenge proves the client's key, while TLS, which
    /// keeps a challenge from being relayed, ends at the gateway.
    pub fn challenged(listen: SocketAddr, served: Served) -> Result<Server> {
        if served.admission.is_none() {
            return Err(Error::Invalid(
                "a server that trusts the challenge alone needs an admission rule".into(),
            ));
        }
        Server::listen(listen, Trust::Challenge, served)
    }

    fn listen(listen: SocketAddr, trust: Trust, served: Served) -> Result<Server> {
        let listener = TcpListener::bind(listen)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let conns = Conns::default();
        let (s, c) = (stop.clone(), conns.clone());
        let thread = thread::spawn(move || accept_loop(listener, &trust, &served, &s, &c));
        Ok(Server {
            addr,
            stop,
            conns,
            thread: Some(thread),
        })
    }

    /// The address listened on, with the port the system chose for port 0.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops listening and breaks every open session.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join(); // the listener is closed when the loop returns
        }
        for c in lock(&self.conns).values() {
            let _ = c.shutdown(Shutdown::Both);
        }
    }

    /// Serves until the process ends.
    pub fn wait(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn lock(conns: &Conns) -> MutexGuard<'_, HashMap<u64, TcpStream>> {
    // A poisoned lock only means another thread panicked; the map is intact.
    conns.lock().unwrap_or_else(|e| e.into_inner())
}

fn accept_loop(
    listener: TcpListener,
    trust: &Trust,
    served: &Served,
    stop: &AtomicBool,
    conns: &Conns,
) {
    let mut n = 0;
    while !stop.load(Ordering::Relaxed) {
        let (stream, from) = match listener.accept() {
            Ok(c) => c,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(POLL);
                continue;
            }
            Err(e) => {
                log!("accept: {e}");
                thread::sleep(POLL);
                continue;
            }
        };
        let peer = match trust {
            Trust::Devices(devices) => match device(from.ip(), devices) {
                Some(peer) => Some(peer),
                None => {
                    log!("refused a connection from {from}");
                    continue;
                }
            },
            Trust::Challenge => None,
        };
        if lock(conns).len() >= super::MAX_SESSIONS {
            log!(
                "refused a connection from {from}: {} sessions",
                super::MAX_SESSIONS
            );
            continue;
        }
        let Ok(clone) = stream.try_clone() else {
            continue;
        };
        n += 1;
        lock(conns).insert(n, clone);
        let (served, conns) = (served.clone(), conns.clone());
        thread::spawn(move || {
            if let Err(e) = handle(stream, &served, peer) {
                log!("session with {from}: {e}");
            }
            lock(&conns).remove(&n);
        });
    }
}

/// Serves one connection from `peer` (if the address tells): Hello, then
/// requests until Bye or the end.
fn handle(stream: TcpStream, served: &Served, peer: Option<PeerKey>) -> Result<()> {
    prepare(&stream, TIMEOUT).map_err(net_err)?;
    let reader = stream.try_clone().map_err(net_err)?;
    let close = closer(&stream)?;
    let r = Responder::new(served.clone(), super::nonce()?);
    let r = match peer {
        Some(peer) => r.with_peer(peer),
        None => r,
    };
    let live = served.live.clone();
    serve(reader, BufWriter::new(stream), r, live, close, PING)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{TransportFixture, live, transport};
    use crate::error::Error;
    use crate::hash::Hash;
    use crate::iface::Transport;
    use crate::protocol::PROTOCOL as VERSION;
    use crate::protocol::sample_served;
    use crate::transport::Tcp;

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    fn local() -> Vec<(PeerKey, IpAddr)> {
        vec![([1; 32], "127.0.0.1".parse().unwrap())]
    }

    fn plain(objects: &[Vec<u8>], root: Hash, protocol: u8) -> TransportFixture {
        let served = sample_served(objects, root, protocol);
        let server = Server::start(loopback(), local(), served).unwrap();
        let peer = server.addr().to_string();
        let server = Mutex::new(server);
        TransportFixture {
            transport: Box::new(Tcp::default()),
            peer,
            stop: Box::new(move || server.lock().unwrap().stop()),
        }
    }

    #[test]
    fn conformance_plain_on_loopback() {
        transport(&plain);
    }

    #[test]
    fn live_conformance_plain_on_loopback() {
        live(&plain);
    }

    #[cfg(feature = "fips")]
    #[test]
    fn the_accept_rule() {
        use crate::transport::fips;
        let [a, b] = [1u8, 2].map(|i| crate::hash::h(&[i]).0);
        let devices = vec![(a, fips::address(&a).into())];
        assert_eq!(device(fips::address(&a).into(), &devices), Some(a));
        assert_eq!(device(fips::address(&b).into(), &devices), None);
        assert_eq!(device("::1".parse().unwrap(), &devices), None, "loopback");
        let v4: IpAddr = "192.168.1.9".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.168.1.9".parse().unwrap();
        assert_eq!(device(mapped, &[(a, v4)]), Some(a), "IPv4-mapped");
        assert_eq!(device(v4, &[(a, mapped)]), Some(a));
        assert_eq!(device(v4, &[]), None);
    }

    #[test]
    fn a_connection_from_an_unconfigured_source_is_refused() {
        let other = vec![([1; 32], "127.0.0.2".parse().unwrap())];
        let server = Server::start(loopback(), other, sample_served(&[], Hash::ZERO, VERSION));
        let server = server.unwrap();
        let mut s = Tcp::default().connect(&server.addr().to_string()).unwrap();
        let r = s.hello(&Hash::ZERO);
        assert!(matches!(r, Err(Error::Unavailable(_))), "{r:?}");
    }
}
