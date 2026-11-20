//! QUIC through WebTransport (§4.2): one session per pull or live session,
//! its frames on one bidirectional stream. TLS authenticates the server; a
//! server that sets `Served::admission` admits clients by the challenge;
//! `QuicServer` serves several services on one port by path. The
//! tokio runtime stays inside: `Quic` is a blocking `Transport`, its live
//! sessions run the blocking live driver, and `QuicServer` runs async
//! drivers, one task per session and a second for a live session's writer.

use super::driver::Blocking;
use super::live::{self, Next, PING};
use super::{TIMEOUT, nonce, url_host};
use crate::auth::{PeerKey, Secret};
use crate::error::{Error, Result};
use crate::iface::{Live, LiveEvent, LiveEvents, Session, Transport};
use crate::log;
use crate::protocol::{Client, Responder, Served};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::runtime::Runtime;
use tokio::sync::{Notify, Semaphore};
use tokio::task::{JoinHandle, block_in_place};
use tokio::time::timeout;
use wtransport::{
    ClientConfig, Connection, Endpoint, Identity, RecvStream, SendStream, ServerConfig,
};

const READ: usize = 64 << 10;

pub(super) fn unavailable(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Unavailable(format!("{what}: {e}"))
}

pub(super) fn runtime() -> Result<Arc<Runtime>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    Ok(Arc::new(rt))
}

/// The client: connects to peers given as `https://host:port` URLs.
pub struct Quic {
    endpoint: Endpoint<wtransport::endpoint::endpoint_side::Client>,
    key: Option<Arc<Secret>>,
    /// Declared last: it is dropped after what runs on it.
    rt: Arc<Runtime>,
}

impl Quic {
    /// A client that trusts only `certificate` (a SHA-256 certificate pin)
    /// and answers challenges with the device key. Pins must be exchanged
    /// over an authenticated channel, never learned from the connection.
    pub fn pinned(certificate: crate::hash::Hash, key: Arc<Secret>) -> Result<Quic> {
        let config = ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([wtransport::tls::Sha256Digest::new(certificate.0)])
            .build();
        Self::new(config, Some(key))
    }

    /// A client with `config` (which says how servers are verified) that
    /// answers challenges with `key`, if given.
    pub fn new(config: ClientConfig, key: Option<Arc<Secret>>) -> Result<Quic> {
        let rt = runtime()?;
        let endpoint = {
            let _in = rt.enter();
            Endpoint::client(config)?
        };
        Ok(Quic { endpoint, key, rt })
    }

    /// A new WebTransport session to `peer` with its stream, as the two
    /// halves of a blocking stream, and the client that signs its host.
    fn open(&self, peer: &str) -> Result<(Reader, Writer, Client)> {
        let host = url_host(peer)?;
        let (conn, (send, recv)) = self.rt.block_on(async {
            let open = async {
                let c = self
                    .endpoint
                    .connect(peer)
                    .await
                    .map_err(|e| unavailable(peer, e))?;
                let opening = c.open_bi().await.map_err(|e| unavailable(peer, e))?;
                let streams = opening.await.map_err(|e| unavailable(peer, e))?;
                Ok::<_, Error>((c, streams))
            };
            timeout(TIMEOUT, open)
                .await
                .map_err(|e| unavailable(peer, e))?
        })?;
        let client = match &self.key {
            Some(k) => Client::with_key(k.clone(), host),
            None => Client::new(),
        };
        let reader = Reader {
            recv,
            conn: conn.clone(),
            rt: self.rt.clone(),
        };
        let writer = Writer {
            send,
            _conn: conn,
            rt: self.rt.clone(),
        };
        Ok((reader, writer, client))
    }
}

impl Quic {
    /// A live session to `peer`, as `live`, on a connection that also
    /// serves pulls back (§4.2): each stream the server opens on it names
    /// a path, and is served as a pull of `serve(path)`'s folder; a path it
    /// answers `None` for is refused. The connection ends with the session.
    pub fn live_serving(
        &self,
        peer: &str,
        events: LiveEvents,
        serve: impl Fn(&str) -> Option<Served> + Send + Sync + 'static,
    ) -> Result<Arc<dyn Live>> {
        let (reader, writer, client) = self.open(peer)?;
        let conn = reader.conn.clone();
        let serve = Arc::new(serve);
        self.rt.spawn(async move {
            while let Ok((send, mut recv)) = conn.accept_bi().await {
                let (serve, conn) = (serve.clone(), conn.clone());
                tokio::spawn(async move {
                    let path = match read_path(&mut recv).await {
                        Ok(p) => p,
                        Err(e) => return log!("pull back: {e}"),
                    };
                    let Some(served) = serve(&path) else {
                        return log!("pull back: nothing served at {path:?}");
                    };
                    let r = match nonce() {
                        Ok(n) => Responder::new(served, n),
                        Err(e) => return log!("pull back: {e}"),
                    };
                    if let Err(e) = serve_stream(send, recv, r, None, conn, None).await {
                        log!("pull back {path:?}: {e}");
                    }
                });
            }
        });
        let close = closer(reader.conn.clone());
        live::connect(reader, writer, client, close, events, PING)
    }
}

/// The longest path a pull back may name.
const BACK_PATH: usize = 4096;

/// Reads the path a server-opened stream starts with: its length as a
/// little-endian u16, then that many bytes of UTF-8.
async fn read_path(recv: &mut RecvStream) -> Result<String> {
    let mut len = [0u8; 2];
    recv.read_exact(&mut len)
        .await
        .map_err(|e| unavailable("pull back", e))?;
    let len = u16::from_le_bytes(len) as usize;
    if len > BACK_PATH {
        return Err(Error::Protocol(format!("pull back: a path of {len} bytes")));
    }
    let mut path = vec![0u8; len];
    recv.read_exact(&mut path)
        .await
        .map_err(|e| unavailable("pull back", e))?;
    String::from_utf8(path).map_err(|_| Error::Protocol("pull back: a path not UTF-8".into()))
}

/// The connections that devices keep open to a server through live
/// sessions in which they proved a key, by that key: a `Transport` whose
/// peers are `<key hex>/<path>`, pulling from the device's folder at path
/// through its own connection (§4.2, pulls back). Give it to
/// `QuicServer::start`.
#[derive(Clone, Default)]
pub struct Backs(Arc<Mutex<BackState>>);

#[derive(Default)]
struct BackState {
    rt: Option<Arc<Runtime>>,
    conns: HashMap<PeerKey, Connection>,
}

impl Backs {
    /// The keys whose devices are connected now.
    pub fn keys(&self) -> Vec<PeerKey> {
        self.lock().conns.keys().copied().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BackState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn add(&self, key: PeerKey, conn: Connection) {
        self.lock().conns.insert(key, conn);
    }

    /// Forgets `key`'s connection if it is still `conn`.
    fn remove(&self, key: &PeerKey, conn: &Connection) {
        let mut s = self.lock();
        if s.conns
            .get(key)
            .is_some_and(|c| c.stable_id() == conn.stable_id())
        {
            s.conns.remove(key);
        }
    }
}

impl Transport for Backs {
    fn connect(&self, peer: &str) -> Result<Box<dyn Session>> {
        let (key, path) = peer
            .split_once('/')
            .ok_or_else(|| Error::Invalid(format!("not <key>/<path>: {peer}")))?;
        let key: PeerKey = crate::hash::Hash::from_hex(key)
            .map_err(|_| Error::Invalid(format!("not a key: {key}")))?
            .0;
        let (rt, conn) = {
            let s = self.lock();
            let conn = s.conns.get(&key).cloned();
            match (s.rt.clone(), conn) {
                (Some(rt), Some(conn)) => (rt, conn),
                _ => return Err(Error::Unavailable(format!("{peer}: not connected"))),
            }
        };
        let path = format!("/{}", path.trim_start_matches('/'));
        if path.len() > BACK_PATH {
            return Err(Error::Invalid(format!("a path of {} bytes", path.len())));
        }
        let (send, recv) = rt.block_on(async {
            let open = async {
                let opening = conn.open_bi().await.map_err(|e| unavailable(peer, e))?;
                let (mut send, recv) = opening.await.map_err(|e| unavailable(peer, e))?;
                let mut head = (path.len() as u16).to_le_bytes().to_vec();
                head.extend_from_slice(path.as_bytes());
                send.write_all(&head)
                    .await
                    .map_err(|e| unavailable(peer, e))?;
                Ok::<_, Error>((send, recv))
            };
            timeout(TIMEOUT, open)
                .await
                .map_err(|e| unavailable(peer, e))?
        })?;
        let reader = Reader {
            recv,
            conn: conn.clone(),
            rt: rt.clone(),
        };
        let writer = Writer {
            send,
            _conn: conn,
            rt,
        };
        Ok(Box::new(Blocking::with(
            Bridge(reader, writer),
            Client::new(),
        )))
    }

    fn live(&self, peer: &str, _: LiveEvents) -> Result<Arc<dyn Live>> {
        Err(Error::Invalid(format!("{peer}: no live sessions back")))
    }
}

impl Transport for Quic {
    fn connect(&self, peer: &str) -> Result<Box<dyn Session>> {
        let (reader, writer, client) = self.open(peer)?;
        Ok(Box::new(Blocking::with(Bridge(reader, writer), client)))
    }

    fn live(&self, peer: &str, events: LiveEvents) -> Result<Arc<dyn Live>> {
        let (reader, writer, client) = self.open(peer)?;
        let close = closer(reader.conn.clone());
        live::connect(reader, writer, client, close, events, PING)
    }
}

/// Closes `conn`, from any thread.
fn closer(conn: Connection) -> live::Close {
    Box::new(move || conn.close(0u32.into(), b"ended"))
}

/// The receiving half of a WebTransport stream as a blocking `Read`, each
/// call run on the transport's runtime with the read limit of §4.2.
struct Reader {
    recv: RecvStream,
    /// Kept open: dropping the connection closes the session.
    conn: Connection,
    rt: Arc<Runtime>,
}

/// The sending half, as a blocking `Write`.
struct Writer {
    send: SendStream,
    _conn: Connection,
    rt: Arc<Runtime>,
}

/// Both halves, for a pull.
struct Bridge(Reader, Writer);

impl Read for Bridge {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Bridge {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.1.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let r = self
            .rt
            .block_on(async { timeout(TIMEOUT, self.recv.read(buf)).await });
        match r {
            Ok(Ok(n)) => Ok(n.unwrap_or(0)),
            Ok(Err(wtransport::error::StreamReadError::NotConnected)) => {
                Err(io::Error::other(self.rt.block_on(self.conn.closed())))
            }
            Ok(Err(e)) => Err(io::Error::other(e)),
            Err(_) => Err(io::ErrorKind::TimedOut.into()),
        }
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let r = self
            .rt
            .block_on(async { timeout(TIMEOUT, self.send.write_all(buf)).await });
        match r {
            Ok(Ok(())) => Ok(buf.len()),
            Ok(Err(e)) => Err(io::Error::other(e)),
            Err(_) => Err(io::ErrorKind::TimedOut.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A running server; stopped when dropped.
pub struct QuicServer {
    endpoint: Arc<Endpoint<wtransport::endpoint::endpoint_side::Server>>,
    addr: SocketAddr,
    /// Aborted before the endpoint closes: wtransport's `accept` panics on
    /// a closed endpoint.
    accepting: JoinHandle<()>,
    /// Runs the accept loop and the sessions; declared last, so it is
    /// dropped after the endpoint.
    _rt: Arc<Runtime>,
}

/// The `Served` of a session's request path, if the server serves it.
type Route = Arc<dyn Fn(&str) -> Option<Served> + Send + Sync>;

impl QuicServer {
    /// A self-signed TLS identity as certificate DER and PKCS#8 key DER.
    /// WebTransport pins require certificates valid for at most 14 days.
    pub fn self_signed() -> Result<(Vec<u8>, Vec<u8>)> {
        let id = Identity::self_signed(["localhost"])
            .map_err(|e| Error::Invalid(format!("TLS identity: {e}")))?;
        Ok((
            id.certificate_chain().as_slice()[0].der().to_vec(),
            id.private_key().secret_der().to_vec(),
        ))
    }

    /// Starts with a persisted DER certificate and PKCS#8 private key.
    pub fn from_der(
        listen: SocketAddr,
        certificate: Vec<u8>,
        key: Vec<u8>,
        route: impl Fn(&str) -> Option<Served> + Send + Sync + 'static,
        backs: Option<Backs>,
    ) -> Result<QuicServer> {
        use wtransport::tls::rustls::{self, pki_types};
        // wtransport's identity constructor does not validate the key, and
        // its server builder panics on invalid or mismatched credentials.
        // Validate both here before passing them to that builder.
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::Invalid(format!("TLS versions: {e}")))?
        .with_no_client_auth()
        .with_single_cert(
            vec![pki_types::CertificateDer::from(certificate.clone())],
            pki_types::PrivatePkcs8KeyDer::from(key.clone()).into(),
        )
        .map_err(|e| Error::Invalid(format!("TLS credentials: {e}")))?;
        use wtransport::tls::{Certificate, CertificateChain, PrivateKey};
        let cert = Certificate::from_der(certificate)
            .map_err(|e| Error::Invalid(format!("TLS certificate: {e}")))?;
        let id = Identity::new(
            CertificateChain::single(cert),
            PrivateKey::from_der_pkcs8(key),
        );
        Self::start(listen, id, route, backs)
    }

    /// Listens on `listen` with the TLS `identity`. `route` gets each
    /// session's URL path, without the query, and answers its `Served`, or
    /// `None` to refuse the session (404): one service for every path
    /// (`|_| Some(served.clone())`), a fixed list, or a set that changes
    /// while serving. Set `Served::admission` to admit clients by key. With
    /// `backs`, every live session whose client proved a key keeps its
    /// connection there while it lasts, for pulls back.
    pub fn start(
        listen: SocketAddr,
        identity: Identity,
        route: impl Fn(&str) -> Option<Served> + Send + Sync + 'static,
        backs: Option<Backs>,
    ) -> Result<QuicServer> {
        let route = move |path: &str| route(path.split('?').next().unwrap_or_default());
        Self::listen_with(listen, identity, Arc::new(route), backs)
    }

    fn listen_with(
        listen: SocketAddr,
        identity: Identity,
        route: Route,
        backs: Option<Backs>,
    ) -> Result<QuicServer> {
        let rt = runtime()?;
        if let Some(b) = &backs {
            b.lock().rt = Some(rt.clone());
        }
        let config = ServerConfig::builder()
            .with_bind_address(listen)
            .with_identity(identity)
            .build();
        let endpoint = {
            let _in = rt.enter();
            Arc::new(Endpoint::server(config)?)
        };
        let addr = endpoint.local_addr()?;
        let listener = endpoint.clone();
        let slots = Arc::new(Semaphore::new(super::MAX_SESSIONS));
        let accepting = rt.spawn(async move {
            loop {
                let incoming = listener.accept().await;
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    log!("quic: refused a session: {} open", super::MAX_SESSIONS);
                    continue; // dropping it refuses the connection
                };
                let (route, backs) = (route.clone(), backs.clone());
                tokio::spawn(async move {
                    // Released when the task ends, even by a panic.
                    let _slot = slot;
                    if let Err(e) = session(incoming, route, backs).await {
                        log!("quic session: {e}");
                    }
                });
            }
        });
        Ok(QuicServer {
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

impl Drop for QuicServer {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn session(
    incoming: wtransport::endpoint::IncomingSession,
    route: Route,
    backs: Option<Backs>,
) -> Result<()> {
    // A client that never gets as far as its stream holds its slot only
    // for the read limit.
    let opened = timeout(TIMEOUT, open(incoming, route))
        .await
        .map_err(|e| unavailable("session", e))??;
    let Some((served, conn, send, recv)) = opened else {
        return Ok(());
    };
    let live = served.live.clone();
    let r = Responder::new(served, nonce()?);
    let result = serve_stream(send, recv, r, live, conn.clone(), backs).await;
    if let Err(e) = &result {
        // Dropping streams alone loses the refusal and can leave a peer's
        // live reader waiting. An application close carries the reason.
        conn.close(1u32.into(), e.to_string().as_bytes());
    }
    result
}

/// A session's service, connection and stream; `None` if it has no service.
async fn open(
    incoming: wtransport::endpoint::IncomingSession,
    route: Route,
) -> Result<Option<(Served, Connection, SendStream, RecvStream)>> {
    let request = incoming.await.map_err(|e| unavailable("incoming", e))?;
    let Some(served) = route(request.path()) else {
        log!("quic session: no service at {:?}", request.path());
        request.not_found().await;
        return Ok(None);
    };
    let conn = request
        .accept()
        .await
        .map_err(|e| unavailable("accept", e))?;
    let (send, recv) = conn
        .accept_bi()
        .await
        .map_err(|e| unavailable("stream", e))?;
    Ok(Some((served, conn, send, recv)))
}

/// The async driver: as the blocking `serve`, over a WebTransport stream. A
/// session that turns live goes to `serve_live`.
async fn serve_stream(
    mut send: SendStream,
    mut recv: RecvStream,
    mut r: Responder,
    live: Option<LiveEvents>,
    conn: Connection,
    backs: Option<Backs>,
) -> Result<()> {
    let mut buf = vec![0; READ];
    loop {
        // The responder reads the store and walks trees: blocking work,
        // kept off the runtime's few workers.
        while let Some(frame) = block_in_place(|| r.transmit())? {
            send.write_all(&frame)
                .await
                .map_err(|e| unavailable("send", e))?;
        }
        if r.is_done() {
            let _ = send.finish().await;
            return Ok(());
        }
        let events = match timeout(TIMEOUT, recv.read(&mut buf)).await {
            Ok(Ok(Some(n))) => match block_in_place(|| r.receive(&buf[..n])) {
                Ok(events) => events,
                Err(e) => {
                    // Close before dropping the stream, which could send FIN
                    // before the peer learns why admission was refused.
                    conn.close(1u32.into(), e.to_string().as_bytes());
                    return Err(e);
                }
            },
            _ => return Ok(()), // ended, failed or timed out
        };
        match live {
            Some(app) if r.is_live() => {
                let back = backs.zip(r.peer().copied());
                if let Some((b, key)) = &back {
                    b.add(*key, conn.clone());
                }
                serve_live(r, events, send, recv, conn.clone(), app).await;
                if let Some((b, key)) = back {
                    b.remove(&key, &conn);
                }
                return Ok(());
            }
            _ => super::refuse_live(&mut r, events)?,
        }
    }
}

/// Serves a session that has just turned live, `first` its events so far:
/// the writer in a new task, the reader in this one, until it ends. Sending
/// to the application waits outside the runtime's scheduling when its
/// channel is full, so a slow application stops the reading.
async fn serve_live(
    r: Responder,
    first: Vec<LiveEvent>,
    send: SendStream,
    mut recv: RecvStream,
    conn: Connection,
    events: LiveEvents,
) {
    let notify = Arc::new(Notify::new());
    let wake = notify.clone();
    let wake: live::Wake = Box::new(move || wake.notify_one());
    let s = live::Session::server(r, closer(conn), Some(wake));
    tokio::spawn(write(s.clone(), notify, send));
    let me: Arc<dyn Live> = s.clone();
    let mut buf = vec![0; READ];
    let mut got = Some(first);
    while let Some(new) = got {
        if !new.into_iter().all(|e| deliver(&events, (me.clone(), e))) {
            s.deaf();
        }
        let n = match timeout(TIMEOUT, recv.read(&mut buf)).await {
            Ok(Ok(Some(n))) => Ok(n),
            Ok(Ok(None)) => Err(Error::Unavailable("connection closed".into())),
            Ok(Err(e)) => Err(unavailable("connection", e)),
            Err(e) => Err(unavailable("connection", e)),
        };
        got = s.received(n.map(|n| &buf[..n]));
    }
    deliver(&events, (me, LiveEvent::Ended(s.why())));
}

/// Hands one event to the application; false if no one takes it.
fn deliver(events: &LiveEvents, e: (Arc<dyn Live>, LiveEvent)) -> bool {
    match events.try_send(e) {
        Ok(()) => true,
        Err(TrySendError::Full(e)) => block_in_place(|| events.send(e).is_ok()),
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// Sends the outbox as it fills, and Ping after `PING` without sending,
/// until the session ends.
async fn write(s: Arc<live::Session>, notify: Arc<Notify>, mut send: SendStream) {
    let mut sent = Instant::now();
    loop {
        let next = s.next(sent.elapsed(), PING);
        match next {
            Next::Stop => return,
            Next::Wait(wait) => {
                let _ = timeout(wait, notify.notified()).await;
            }
            Next::Send(out) => {
                if let Err(e) = send.write_all(&out).await {
                    s.fail(unavailable("send", e));
                    return;
                }
                sent = Instant::now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PeerKey;
    use crate::conformance::{TransportFixture, live, transport};
    use crate::hash::Hash;
    use crate::protocol::PROTOCOL as VERSION;
    use crate::protocol::{Admission, sample_served};
    use std::sync::Mutex;

    fn identity() -> Identity {
        Identity::self_signed(["localhost"]).unwrap()
    }

    /// A client that trusts exactly the certificate of `id`.
    fn client(id: &Identity, key: Option<Arc<Secret>>) -> Quic {
        let hash = id.certificate_chain().as_slice()[0].hash();
        let config = ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([hash])
            .build();
        Quic::new(config, key).unwrap()
    }

    fn start(served: Served) -> (QuicServer, Identity) {
        let id = identity();
        let listen = "127.0.0.1:0".parse().unwrap();
        (
            QuicServer::start(
                listen,
                id.clone_identity(),
                move |_| Some(served.clone()),
                None,
            )
            .unwrap(),
            id,
        )
    }

    fn url(s: &QuicServer) -> String {
        format!("https://127.0.0.1:{}", s.addr().port())
    }

    fn fixture(objects: &[Vec<u8>], root: Hash, protocol: u8) -> TransportFixture {
        let (server, id) = start(sample_served(objects, root, protocol));
        let peer = url(&server);
        let server = Mutex::new(server);
        TransportFixture {
            transport: Box::new(client(&id, None)),
            peer,
            stop: Box::new(move || server.lock().unwrap().stop()),
        }
    }

    #[test]
    fn persisted_credentials_reject_a_broken_private_key() {
        let (cert, _) = QuicServer::self_signed().unwrap();
        let result = QuicServer::from_der(
            "127.0.0.1:0".parse().unwrap(),
            cert,
            vec![0; 32],
            |_| None,
            None,
        );
        assert!(matches!(result, Err(Error::Invalid(_))));
    }

    #[test]
    fn conformance() {
        transport(&fixture);
    }

    #[test]
    fn live_conformance() {
        live(&fixture);
    }

    #[test]
    fn a_routed_server_serves_each_path_its_own() {
        let key = Arc::new(Secret::from_bytes(&[1; 32]).unwrap());
        let admitted: PeerKey = key.public();
        let admission = Admission {
            names: vec!["127.0.0.1".into()],
            admits: Box::new(move |k| *k == admitted),
        };
        let open = sample_served(&[], Hash([5; 32]), VERSION);
        let keyed = Served {
            admission: Some(Arc::new(admission)),
            ..sample_served(&[], Hash([6; 32]), VERSION)
        };
        let id = identity();
        let listen = "127.0.0.1:0".parse().unwrap();
        let paths: Vec<(String, Served)> = vec![("/open".into(), open), ("/keyed".into(), keyed)];
        let route = move |p: &str| paths.iter().find(|(q, _)| q == p).map(|(_, s)| s.clone());
        let server = QuicServer::start(listen, id.clone_identity(), route, None).unwrap();
        let at = |path: &str| format!("{}{path}", url(&server));
        let hello = |c: &Quic, path: &str| c.connect(&at(path))?.hello(&Hash::ZERO);

        let anyone = client(&id, None);
        assert_eq!(hello(&anyone, "/open").unwrap(), Hash([5; 32]));
        assert_eq!(hello(&anyone, "/open?x=1").unwrap(), Hash([5; 32]));
        let r = hello(&anyone, "/keyed");
        assert!(matches!(r, Err(Error::Invalid(_))), "no key: {r:?}");
        let keyed = client(&id, Some(key));
        assert_eq!(hello(&keyed, "/keyed").unwrap(), Hash([6; 32]));
        for path in ["", "/", "/other", "/open/x"] {
            let r = anyone.connect(&at(path)).err();
            assert!(matches!(r, Some(Error::Unavailable(_))), "{path:?}: {r:?}");
        }
    }

    #[test]
    fn a_dynamic_server_decides_its_paths_while_serving() {
        let roots = Arc::new(Mutex::new(vec![("/v/a".to_string(), Hash([7; 32]))]));
        let known = roots.clone();
        let route = move |path: &str| {
            let roots = known.lock().unwrap();
            let (_, root) = roots.iter().find(|(p, _)| p == path)?;
            Some(sample_served(&[], *root, VERSION))
        };
        let id = identity();
        let listen = "127.0.0.1:0".parse().unwrap();
        let server = QuicServer::start(listen, id.clone_identity(), route, None).unwrap();
        let at = |path: &str| format!("{}{path}", url(&server));
        let c = client(&id, None);
        let hello = |path: &str| c.connect(&at(path))?.hello(&Hash::ZERO);
        assert_eq!(hello("/v/a?x").unwrap(), Hash([7; 32]));
        assert!(matches!(hello("/v/b"), Err(Error::Unavailable(_))));
        roots.lock().unwrap().push(("/v/b".into(), Hash([8; 32])));
        assert_eq!(hello("/v/b").unwrap(), Hash([8; 32]));
    }

    #[test]
    fn a_server_pulls_back_through_a_device_connection() {
        let key = Arc::new(Secret::from_bytes(&[1; 32]).unwrap());
        let device: PeerKey = key.public();
        let (events, _server_events) = std::sync::mpsc::sync_channel(16);
        let served = Served {
            admission: Some(Arc::new(Admission {
                names: vec!["127.0.0.1".into()],
                admits: Box::new(move |k| *k == device),
            })),
            live: Some(events),
            ..sample_served(&[], Hash([5; 32]), VERSION)
        };
        let id = identity();
        let listen = "127.0.0.1:0".parse().unwrap();
        let backs = Backs::default();
        let route = move |_: &str| Some(served.clone());
        let server =
            QuicServer::start(listen, id.clone_identity(), route, Some(backs.clone())).unwrap();
        let peer = format!("{}/v/a", Hash(device).to_hex());
        assert!(matches!(
            backs.connect(&peer).err(),
            Some(Error::Unavailable(_))
        ));

        // The device's folder: one object at root 9.
        let x = b"on the device".to_vec();
        let mine = sample_served(std::slice::from_ref(&x), Hash([9; 32]), VERSION);
        let c = client(&id, Some(key));
        let (events, _device_events) = std::sync::mpsc::sync_channel(16);
        let serve = move |path: &str| (path == "/v/a").then(|| mine.clone());
        let _live = c.live_serving(&url(&server), events, serve).unwrap();
        let start = Instant::now();
        while backs.keys().is_empty() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "never registered"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut s = backs.connect(&peer).unwrap();
        assert_eq!(s.hello(&Hash::ZERO).unwrap(), Hash([9; 32]));
        let got = s.get(&[crate::hash::h(&x)]).unwrap();
        assert_eq!(got[0].as_ref().ok(), Some(&x));
        s.bye().unwrap();
        // A path the device does not serve.
        let other = format!("{}/v/b", Hash(device).to_hex());
        let r = backs.connect(&other).and_then(|mut s| s.hello(&Hash::ZERO));
        assert!(r.is_err(), "{r:?}");
    }

    #[test]
    fn a_refused_live_session_receives_the_reason_promptly() {
        let key = Arc::new(Secret::from_bytes(&[2; 32]).unwrap());
        let (events, _server_events) = std::sync::mpsc::sync_channel(16);
        let served = Served {
            admission: Some(Arc::new(Admission {
                names: vec!["127.0.0.1".into()],
                admits: Box::new(|_| false),
            })),
            live: Some(events),
            ..sample_served(&[], Hash::ZERO, VERSION)
        };
        let (server, id) = start(served);
        let quic = client(&id, Some(key));
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let live = quic.live_serving(&url(&server), tx, |_| None).unwrap();
        let _ = live.open("kota", b"");
        let (_, event) = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert!(
            matches!(event, LiveEvent::Ended(ref e) if e.to_string().contains("a key this server does not admit")),
            "{event:?}"
        );
    }

    #[test]
    fn a_challenging_server_admits_only_its_keys() {
        let key = |i: u8| Arc::new(Secret::from_bytes(&[i; 32]).unwrap());
        let admitted: PeerKey = key(1).public();
        let admission = Admission {
            names: vec!["127.0.0.1".into()],
            admits: Box::new(move |k| *k == admitted),
        };
        let x = b"x".to_vec();
        let served = Served {
            admission: Some(Arc::new(admission)),
            ..sample_served(std::slice::from_ref(&x), Hash([5; 32]), VERSION)
        };
        let (server, id) = start(served);
        let mut s = client(&id, Some(key(1))).connect(&url(&server)).unwrap();
        assert_eq!(s.hello(&Hash::ZERO).unwrap(), Hash([5; 32]));
        let got = s.get(&[crate::hash::h(&x)]).unwrap();
        assert_eq!(got[0].as_ref().ok(), Some(&x));

        let mut s = client(&id, None).connect(&url(&server)).unwrap();
        let r = s.hello(&Hash::ZERO);
        assert!(matches!(r, Err(Error::Invalid(_))), "no key: {r:?}");
        let mut s = client(&id, Some(key(2))).connect(&url(&server)).unwrap();
        s.hello(&Hash::ZERO).unwrap();
        let r = s.get(&[crate::hash::h(&x)]);
        assert!(
            matches!(&r, Err(Error::Unavailable(m)) if m.contains("a key this server does not admit")),
            "another key must receive the close reason: {r:?}"
        );
    }
}
