use super::*;
use std::io::{Read, Write};
use std::net::SocketAddr;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
/// HTTPS/WebSocket server with dynamic service routing, stopped on drop.
/// Set each exposed service's device-key admission rule; without it the
/// service is public, as with QUIC.
pub struct HttpsServer {
    addr: SocketAddr,
    accepting: tokio::task::JoinHandle<()>,
    conns: Arc<Mutex<Vec<std::sync::Weak<Connection>>>>,
    stopping: tokio::sync::watch::Sender<bool>,
    _rt: Arc<Runtime>,
}
impl HttpsServer {
    /// Start with a persisted DER certificate and PKCS#8 private key.
    pub fn from_der(
        listen: SocketAddr,
        certificate: Vec<u8>,
        key: Vec<u8>,
        route: impl Fn(&str) -> Option<Served> + Send + Sync + 'static,
        backs: Option<Backs>,
    ) -> Result<Self> {
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Invalid(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.into()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key).into(),
        )
        .map_err(|e| Error::Invalid(format!("TLS credentials: {e}")))?;
        Self::start(listen, Arc::new(config), route, backs)
    }
    /// Start with an application's TLS identity. For a reverse proxy, relay
    /// the TLS connection, or terminate TLS only on a trusted local gateway.
    pub fn start(
        listen: SocketAddr,
        tls: Arc<rustls::ServerConfig>,
        route: impl Fn(&str) -> Option<Served> + Send + Sync + 'static,
        backs: Option<Backs>,
    ) -> Result<Self> {
        let mut tls = (*tls).clone();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tls = Arc::new(tls);
        let rt = runtime()?;
        let listener = rt.block_on(tokio::net::TcpListener::bind(listen))?;
        let addr = listener.local_addr()?;
        let route: Route = Arc::new(route);
        let conns = Arc::new(Mutex::new(Vec::<std::sync::Weak<Connection>>::new()));
        let connections = conns.clone();
        let runtime = Arc::downgrade(&rt);
        let slots = Arc::new(tokio::sync::Semaphore::new(super::super::MAX_SESSIONS));
        let (stopping, stop) = tokio::sync::watch::channel(false);
        let accepting = rt.spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                let (route, backs, connections, runtime) = (
                    route.clone(),
                    backs.clone(),
                    connections.clone(),
                    runtime.clone(),
                );
                let mut stop = stop.clone();
                let acceptor = TlsAcceptor::from(tls.clone());
                tokio::spawn(async move {
                    if *stop.borrow() {
                        return;
                    }
                    let mut served = None;
                    // tungstenite fixes this callback's error type to a full HTTP response.
                    #[allow(clippy::result_large_err)]
                    let open = async {
                        tcp.set_nodelay(true).map_err(|e| unavailable("HTTPS", e))?;
                        let tls = acceptor
                            .accept(tcp)
                            .await
                            .map_err(|e| unavailable("TLS", e))?;
                        tokio_tungstenite::accept_hdr_async_with_config(
                            tls,
                            |request: &Request, mut response: Response| {
                                if request.headers()
                                    .get("Sec-WebSocket-Protocol")
                                    .and_then(|h| h.to_str().ok()) != Some("kide-sync-v1")
                                {
                                    let mut response = tokio_tungstenite::tungstenite::http::Response::new(None);
                                    *response.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::BAD_REQUEST;
                                    return Err(response);
                                }
                                response.headers_mut().insert("Sec-WebSocket-Protocol",
                                    tokio_tungstenite::tungstenite::http::HeaderValue::from_static("kide-sync-v1"));
                                served = tokio::task::block_in_place(|| route(request.uri().path()));
                                if served.is_none() {
                                    let mut response =
                                        tokio_tungstenite::tungstenite::http::Response::new(None);
                                    *response.status_mut() =
                                        tokio_tungstenite::tungstenite::http::StatusCode::NOT_FOUND;
                                    return Err(response);
                                }
                                Ok(response)
                            },
                            Some(ws_config()),
                        )
                        .await
                        .map_err(|e| unavailable("WebSocket", e))
                    };
                    let opened = tokio::select! {
                        result = timeout(TIMEOUT,open) => result,
                        _ = stop.changed() => return,
                    };
                    let Ok(Ok(ws)) = opened else {
                        return;
                    };
                    let (Some(served), Some(rt)) = (served, runtime.upgrade()) else {
                        return;
                    };
                    let Ok((c, r, w)) = mux::start(ws, &rt, None) else {
                        return;
                    };
                    {
                        let mut known = connections.lock().unwrap_or_else(|e| e.into_inner());
                        known.retain(|c| c.strong_count() != 0);
                        if *stop.borrow() {
                            c.close();
                        } else {
                            known.push(Arc::downgrade(&c));
                        }
                    }
                    drop(rt); // streams keep the runtime alive outside async tasks
                    std::thread::spawn(move || {
                        let _slot = slot;
                        serve(r, w, served, backs);
                        c.close();
                    });
                });
            }
        });
        Ok(Self {
            addr,
            accepting,
            conns,
            stopping,
            _rt: rt,
        })
    }
    /// The bound address, with the selected port when listening on port zero.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
    /// Stop accepting and break all open connections.
    pub fn stop(&mut self) {
        self.stopping.send_replace(true);
        self.accepting.abort();
        for c in self
            .conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|c| c.upgrade())
        {
            c.close();
        }
    }
}
impl Drop for HttpsServer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) fn serve(mut reader: Reader, mut writer: Writer, served: Served, backs: Option<Backs>) {
    let c = mux::connection(&reader);
    let rt = mux::runtime(&reader);
    let live_events = served.live.clone();
    let result = (|| {
        let mut r = Responder::new(served, nonce()?);
        let mut buf = vec![0; mux::CHUNK];
        loop {
            while let Some(bytes) = r.transmit()? {
                writer.write_all(&bytes).map_err(super::super::net_err)?;
            }
            if r.is_done() {
                return Ok(None);
            }
            let n = reader.read(&mut buf).map_err(super::super::net_err)?;
            if n == 0 {
                return Ok(None);
            }
            let events = r.receive(&buf[..n])?;
            if live_events.is_some() && r.is_live() {
                return Ok(Some((r, events)));
            }
            super::super::refuse_live(&mut r, events)?;
        }
    })();
    match result {
        Ok(Some((r, events))) => {
            let back = backs.zip(r.peer().copied());
            if let Some((backs, key)) = &back {
                backs
                    .0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(*key, (c.clone(), rt));
            }
            if let Some(app) = live_events {
                live::serve(r, events, reader, writer, c.closer(), &app, live::PING);
            }
            if let Some((backs, key)) = back {
                backs.remove(&key, &c);
            }
        }
        Ok(None) => (),
        Err(e) => {
            mux::refusal(&writer, &e);
            crate::log!("HTTPS session: {e}");
        }
    }
}
