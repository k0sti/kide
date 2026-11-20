//! The gateway (§4.2): owns a QUIC port and its certificate, and relays
//! each WebTransport session by its URL path to an application's own server
//! over TCP (`Server::challenged`, on loopback), splicing the stream's bytes
//! both ways. Kide's frames are the same bytes over either transport, so
//! the gateway reads none of them: Hello, the challenge and everything after
//! run end to end between the client and the application, which admits by
//! the challenge. Applications behind one gateway share its port and
//! certificate but run, and are deployed, as separate processes.

use super::TIMEOUT;
use super::quic::{runtime, unavailable};
use crate::error::Result;
use crate::log;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, copy};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use wtransport::endpoint::IncomingSession;
use wtransport::{Endpoint, Identity, ServerConfig};

/// A running gateway; stopped when dropped.
pub struct Gateway {
    endpoint: Arc<Endpoint<wtransport::endpoint::endpoint_side::Server>>,
    addr: SocketAddr,
    accepting: JoinHandle<()>,
    /// Declared last: it is dropped after what runs on it.
    _rt: Arc<Runtime>,
}

type Routes = Arc<Vec<(String, SocketAddr)>>;

impl Gateway {
    /// Listens on `listen` with the TLS `identity` and relays a session to
    /// the URL path of a route (`/identity`, say; the query is ignored) to
    /// that route's TCP address; a session to any other path is refused
    /// (404), as `QuicServer` refuses a path it has no route for.
    pub fn start(
        listen: SocketAddr,
        identity: Identity,
        routes: Vec<(String, SocketAddr)>,
    ) -> Result<Gateway> {
        let rt = runtime()?;
        let config = ServerConfig::builder()
            .with_bind_address(listen)
            .with_identity(identity)
            .build();
        let endpoint = {
            let _in = rt.enter();
            Arc::new(Endpoint::server(config)?)
        };
        let addr = endpoint.local_addr()?;
        let (listener, routes): (_, Routes) = (endpoint.clone(), Arc::new(routes));
        let slots = Arc::new(Semaphore::new(super::MAX_SESSIONS));
        let accepting = rt.spawn(async move {
            loop {
                let incoming = listener.accept().await;
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    log!("gateway: refused a session: {} open", super::MAX_SESSIONS);
                    continue; // dropping it refuses the connection
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let _slot = slot; // released when the task ends
                    if let Err(e) = relay(incoming, routes).await {
                        log!("gateway session: {e}");
                    }
                });
            }
        });
        Ok(Gateway {
            endpoint,
            addr,
            accepting,
            _rt: rt,
        })
    }

    /// The address listened on, with the port the system chose for port 0.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Closes every session and stops listening.
    pub fn stop(&mut self) {
        self.accepting.abort();
        self.endpoint.close(0u32.into(), b"stopped");
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Relays one session to its route's server until either side ends.
async fn relay(incoming: IncomingSession, routes: Routes) -> Result<()> {
    let request = timeout(TIMEOUT, incoming)
        .await
        .map_err(|e| unavailable("incoming", e))?
        .map_err(|e| unavailable("incoming", e))?;
    let path = request.path().split('?').next().unwrap_or_default();
    let Some(&(_, to)) = routes.iter().find(|(p, _)| p == path) else {
        log!("gateway: no route for {path:?}");
        request.not_found().await;
        return Ok(());
    };
    // Reach the application before accepting, so a client of an
    // application that is down sees its session refused, not a silence.
    let tcp = match timeout(TIMEOUT, TcpStream::connect(to)).await {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(e)) => {
            request.not_found().await;
            return Err(unavailable(&to.to_string(), e));
        }
        Err(e) => {
            request.not_found().await;
            return Err(unavailable(&to.to_string(), e));
        }
    };
    tcp.set_nodelay(true).map_err(|e| unavailable("tcp", e))?;
    let accepted = timeout(TIMEOUT, async {
        let conn = request
            .accept()
            .await
            .map_err(|e| unavailable("accept", e))?;
        let streams = conn
            .accept_bi()
            .await
            .map_err(|e| unavailable("stream", e))?;
        Ok::<_, crate::error::Error>((conn, streams))
    });
    let (conn, (mut send, mut recv)) = accepted.await.map_err(|e| unavailable("stream", e))??;
    let (mut from_app, mut to_app) = tcp.into_split();
    // Each direction passes on its end: the application closes its
    // connection after Bye (or on an error), the client closes its stream;
    // the session is over when both have. The read limit of §4.2 stays with
    // the two ends, which also break a connection whose peer is gone; once
    // one direction has ended, the other gets that limit to end too, so a
    // client that keeps its connection open holds nothing.
    let up = async {
        let r = copy(&mut recv, &mut to_app).await;
        let _ = to_app.shutdown().await;
        r
    };
    let down = async {
        let r = copy(&mut from_app, &mut send).await;
        let _ = send.finish().await;
        r
    };
    tokio::pin!(up, down);
    let rest = |r: std::result::Result<std::io::Result<u64>, _>| r.unwrap_or(Ok(0));
    let (up, down) = tokio::select! {
        u = &mut up => (u, rest(timeout(TIMEOUT, &mut down).await)),
        d = &mut down => (rest(timeout(TIMEOUT, &mut up).await), d),
    };
    drop(conn);
    up.map_err(|e| unavailable("to the application", e))?;
    down.map_err(|e| unavailable("from the application", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{PeerKey, Secret};
    use crate::conformance::{TransportFixture, live, transport};
    use crate::error::Error;
    use crate::hash::Hash;
    use crate::iface::Transport;
    use crate::protocol::PROTOCOL as VERSION;
    use crate::protocol::{Admission, Served, sample_served};
    use crate::transport::{Quic, Server};
    use std::sync::Mutex;
    use wtransport::ClientConfig;

    fn key(i: u8) -> Arc<Secret> {
        Arc::new(Secret::from_bytes(&[i; 32]).unwrap())
    }

    /// Admits `keys`, known by the name clients sign: the host of the
    /// gateway's URL, which the application's server never sees itself.
    fn admitting(keys: Vec<PeerKey>) -> Option<Arc<Admission>> {
        Some(Arc::new(Admission {
            names: vec!["127.0.0.1".into()],
            admits: Box::new(move |k| keys.contains(k)),
        }))
    }

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    /// A gateway in front of `apps`, each a path and a challenged server.
    fn gateway(apps: Vec<(&str, Served)>) -> (Gateway, Vec<Server>, Identity) {
        let id = Identity::self_signed(["127.0.0.1"]).unwrap();
        let servers: Vec<Server> = apps
            .iter()
            .map(|(_, s)| Server::challenged(loopback(), s.clone()).unwrap())
            .collect();
        let routes = apps
            .iter()
            .zip(&servers)
            .map(|((p, _), s)| (p.to_string(), s.addr()))
            .collect();
        let g = Gateway::start(loopback(), id.clone_identity(), routes).unwrap();
        (g, servers, id)
    }

    fn client(id: &Identity, key: Option<Arc<Secret>>) -> Quic {
        let hash = id.certificate_chain().as_slice()[0].hash();
        let config = ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([hash])
            .build();
        Quic::new(config, key).unwrap()
    }

    fn fixture(objects: &[Vec<u8>], root: Hash, protocol: u8) -> TransportFixture {
        let served = Served {
            admission: admitting(vec![key(1).public()]),
            ..sample_served(objects, root, protocol)
        };
        let (g, servers, id) = gateway(vec![("/app", served)]);
        let peer = format!("https://127.0.0.1:{}/app", g.addr().port());
        let parts = Mutex::new((g, servers));
        TransportFixture {
            transport: Box::new(client(&id, Some(key(1)))),
            peer,
            stop: Box::new(move || {
                let mut p = parts.lock().unwrap();
                p.1.iter_mut().for_each(Server::stop);
                p.0.stop();
            }),
        }
    }

    #[test]
    fn conformance_through_the_gateway() {
        transport(&fixture);
    }

    #[test]
    fn live_conformance_through_the_gateway() {
        live(&fixture);
    }

    #[test]
    fn each_path_reaches_its_application_and_its_admission() {
        let a = Served {
            admission: admitting(vec![key(1).public()]),
            ..sample_served(&[], Hash([5; 32]), VERSION)
        };
        let b = Served {
            admission: admitting(vec![key(2).public()]),
            ..sample_served(&[], Hash([6; 32]), VERSION)
        };
        let (g, mut servers, id) = gateway(vec![("/a", a), ("/b", b)]);
        let at = |path: &str| format!("https://127.0.0.1:{}{path}", g.addr().port());
        let hello = |c: &Quic, path: &str| c.connect(&at(path))?.hello(&Hash::ZERO);
        let (one, two) = (client(&id, Some(key(1))), client(&id, Some(key(2))));
        assert_eq!(hello(&one, "/a").unwrap(), Hash([5; 32]));
        assert_eq!(hello(&one, "/a?x=1").unwrap(), Hash([5; 32]));
        assert_eq!(hello(&two, "/b").unwrap(), Hash([6; 32]));
        // The challenge runs end to end: a key one application does not
        // admit is turned away by that application.
        // (Its Hello passes; the server ends the session at the answer.)
        let mut s = one.connect(&at("/b")).unwrap();
        s.hello(&Hash::ZERO).unwrap();
        let r = s.get(&[Hash([7; 32])]);
        assert!(matches!(r, Err(Error::Unavailable(_))), "{r:?}");
        let r = hello(&client(&id, None), "/a");
        assert!(matches!(r, Err(Error::Invalid(_))), "no key: {r:?}");
        for path in ["", "/", "/c", "/a/x"] {
            let r = one.connect(&at(path)).err();
            assert!(matches!(r, Some(Error::Unavailable(_))), "{path:?}: {r:?}");
        }
        // An application that is down: its sessions are refused.
        servers[0].stop();
        let r = one.connect(&at("/a")).err();
        assert!(matches!(r, Some(Error::Unavailable(_))), "{r:?}");
        assert_eq!(hello(&two, "/b").unwrap(), Hash([6; 32]));
    }

    #[test]
    fn the_application_sees_the_key_that_answered() {
        use crate::iface::LiveEvent;
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        let served = Served {
            admission: admitting(vec![key(3).public()]),
            live: Some(tx),
            ..sample_served(&[], Hash([5; 32]), VERSION)
        };
        let (g, _servers, id) = gateway(vec![("/app", served)]);
        let (ctx, _crx) = std::sync::mpsc::sync_channel(8);
        let url = format!("https://127.0.0.1:{}/app", g.addr().port());
        let live = client(&id, Some(key(3))).live(&url, ctx).unwrap();
        live.open("s", b"").unwrap();
        let wait = std::time::Duration::from_secs(10);
        let (at_server, event) = rx.recv_timeout(wait).unwrap();
        assert!(matches!(event, LiveEvent::Open { .. }), "{event:?}");
        assert_eq!(at_server.peer(), Some(key(3).public()));
    }

    #[test]
    fn a_server_that_trusts_the_challenge_needs_an_admission_rule() {
        let served = sample_served(&[], Hash([5; 32]), VERSION);
        let r = Server::challenged(loopback(), served);
        assert!(matches!(r, Err(Error::Invalid(_))));
    }
}
