//! Authenticated HTTPS fallback over TCP, normally port 443. TLS authenticates
//! the server; the existing challenge authenticates the device. WebSocket
//! binary envelopes multiplex a primary session and bounded reverse pulls.
mod mux;
mod tls;
use super::{TIMEOUT, driver::Blocking, live, nonce, url_host};
use crate::auth::{PeerKey, Secret};
use crate::error::{Error, Result};
use crate::iface::{Live, LiveEvents, Session, Transport};
use crate::protocol::{Client, Responder, Served};
use mux::{Bridge, Connection, Reader, Writer};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
pub use tokio_rustls::rustls;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

type Route = Arc<dyn Fn(&str) -> Option<Served> + Send + Sync>;
fn unavailable(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Unavailable(format!("{what}: {e}"))
}
fn runtime() -> Result<Arc<Runtime>> {
    Ok(Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?,
    ))
}
fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(mux::CHUNK + 9))
        .max_frame_size(Some(mux::CHUNK + 9))
}

/// Blocking HTTPS transport. Supply a normal WebPKI-verifying TLS config
/// through `new`, or exchange an authenticated certificate pin for `pinned`.
/// No plaintext or unverified TLS retry is performed.
pub struct Https {
    tls: Arc<rustls::ClientConfig>,
    key: Option<Arc<Secret>>,
    rt: Arc<Runtime>,
}
impl Https {
    /// A client with an application's trust roots and optional device key.
    pub fn new(tls: Arc<rustls::ClientConfig>, key: Option<Arc<Secret>>) -> Result<Self> {
        let mut tls = (*tls).clone();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tls = Arc::new(tls);
        Ok(Self {
            tls,
            key,
            rt: runtime()?,
        })
    }
    /// Trust exactly the DER certificate's SHA-256 hash, exchanged securely.
    /// Pin lifetime/rotation is the application's responsibility.
    pub fn pinned(pin: crate::hash::Hash, key: Option<Arc<Secret>>) -> Result<Self> {
        Self::new(tls::pinned(pin)?, key)
    }
    fn open(
        &self,
        peer: &str,
        serve: Option<Route>,
    ) -> Result<(Arc<Connection>, Reader, Writer, Client)> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let host = url_host(peer)?.to_owned();
        let ws_url = format!("wss://{}", &peer[8..]);
        let mut request = ws_url
            .into_client_request()
            .map_err(|e| Error::Invalid(format!("HTTPS URL: {e}")))?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            "kide-sync-v1"
                .parse()
                .map_err(|e| Error::Invalid(format!("WebSocket protocol: {e}")))?,
        );
        let port = request.uri().port_u16().unwrap_or(443);
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|e| Error::Invalid(format!("HTTPS host: {e}")))?;
        let ws = self.rt.block_on(async {
            timeout(TIMEOUT, async {
                let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
                    .await
                    .map_err(|e| unavailable(peer, e))?;
                tcp.set_nodelay(true).map_err(|e| unavailable(peer, e))?;
                let tls = TlsConnector::from(self.tls.clone())
                    .connect(name, tcp)
                    .await
                    .map_err(|e| unavailable(peer, e))?;
                let (ws, _) =
                    tokio_tungstenite::client_async_with_config(request, tls, Some(ws_config()))
                        .await
                        .map_err(|e| unavailable(peer, e))?;
                Ok::<_, Error>(ws)
            })
            .await
            .map_err(|e| unavailable(peer, e))?
        })?;
        let (c, r, w) = mux::start(ws, &self.rt, serve)?;
        let client = match &self.key {
            Some(k) => Client::with_key(k.clone(), &host),
            None => Client::new(),
        };
        Ok((c, r, w, client))
    }
    /// Keep authenticated live control and reverse pulls on one outbound
    /// connection, including behind NAT. Paths not served are refused.
    pub fn live_serving(
        &self,
        peer: &str,
        events: LiveEvents,
        serve: impl Fn(&str) -> Option<Served> + Send + Sync + 'static,
    ) -> Result<Arc<dyn Live>> {
        let (c, r, w, client) = self.open(peer, Some(Arc::new(serve)))?;
        live::connect(r, w, client, c.closer(), events, live::PING)
    }
}
impl Transport for Https {
    fn connect(&self, peer: &str) -> Result<Box<dyn Session>> {
        let (_, r, w, client) = self.open(peer, None)?;
        Ok(Box::new(Blocking::with(Bridge(r, w), client)))
    }
    fn live(&self, peer: &str, events: LiveEvents) -> Result<Arc<dyn Live>> {
        let (c, r, w, client) = self.open(peer, None)?;
        live::connect(r, w, client, c.closer(), events, live::PING)
    }
}

type BackConnections = HashMap<PeerKey, (Arc<Connection>, std::sync::Weak<Runtime>)>;
/// Authenticated live device connections, addressed as `<key hex>/<path>`.
/// Replacements are removed only when the same connection ends.
#[derive(Clone, Default)]
pub struct Backs(Arc<Mutex<BackConnections>>);
impl Backs {
    /// The device keys whose authenticated live sessions remain connected.
    pub fn keys(&self) -> Vec<PeerKey> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, (c, _))| !c.closed())
            .map(|(k, _)| *k)
            .collect()
    }
    fn remove(&self, key: &PeerKey, conn: &Arc<Connection>) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if map.get(key).is_some_and(|(c, _)| Arc::ptr_eq(c, conn)) {
            map.remove(key);
        }
    }
}
impl Transport for Backs {
    fn connect(&self, peer: &str) -> Result<Box<dyn Session>> {
        let (key, path) = peer
            .split_once('/')
            .ok_or_else(|| Error::Invalid("not <key>/<path>".into()))?;
        let key = crate::hash::Hash::from_hex(key)?.0;
        let (conn, rt) = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
            .ok_or_else(|| unavailable(peer, "not connected"))?;
        let rt = rt
            .upgrade()
            .ok_or_else(|| unavailable(peer, "server stopped"))?;
        let path = format!("/{}", path.trim_start_matches('/'));
        let (r, w) = conn.open(&path, rt)?;
        Ok(Box::new(Blocking::new(Bridge(r, w))))
    }
    fn live(&self, _: &str, _: LiveEvents) -> Result<Arc<dyn Live>> {
        Err(Error::Invalid("no live sessions back".into()))
    }
}

mod server;
pub use server::HttpsServer;

#[cfg(test)]
mod tests;
