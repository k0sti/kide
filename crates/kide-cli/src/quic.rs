//! The program's persistent device key and per-peer pinned QUIC clients.
use crate::network::Network;
use kide::auth::Secret;
use kide::error::{Result, invalid};
use kide::folder::Folder;
use kide::folder::config::Transport as Choice;
use kide::folder::npub::Key;
use kide::hash::{Hash, h};
use kide::iface::{Live, LiveEvents, Session, Transport};
use kide::protocol::Admission;
use kide::transport::{Quic, QuicServer, Served, Server, Tcp};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

/// Creates credentials once; refuses to replace any existing file.
pub fn init(state: &Path) -> Result<(Key, Hash)> {
    let secret = Secret::from_bytes(&kide::transport::nonce()?)?;
    let (cert, tls_key) = QuicServer::self_signed()?;
    private(&state.join("device.key"), &secret.to_bytes())?;
    private(&state.join("tls.key"), &tls_key)?;
    private(&state.join("tls.cert"), &cert)?;
    Ok((Key(secret.public()), h(&cert)))
}

/// Renews only TLS credentials, under the folder lock. A partial replacement
/// fails closed (TLS rejects mismatched keys); rerunning repairs it.
pub fn renew(folder: &Folder) -> Result<Hash> {
    let (cert, key) = QuicServer::self_signed()?;
    for (name, bytes) in [("tls.key", key.as_slice()), ("tls.cert", cert.as_slice())] {
        let temp = folder.kide().join(format!("{name}.new"));
        // A previous interrupted renewal is disposable while holding the lock.
        match fs::remove_file(&temp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        private(&temp, bytes)?;
        fs::rename(&temp, folder.kide().join(name))?;
    }
    Ok(h(&cert))
}

fn private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn secret(folder: &Folder) -> Result<Arc<Secret>> {
    let bytes = fs::read(folder.kide().join("device.key"))?;
    let bytes = bytes
        .try_into()
        .map_err(|_| kide::error::Error::Invalid("device.key must hold 32 bytes".into()))?;
    let key = Arc::new(Secret::from_bytes(&bytes)?);
    if !folder.config.this.is_some_and(|d| d.key.0 == key.public()) {
        return invalid("self does not match .kide/device.key");
    }
    Ok(key)
}

pub fn address(choice: Choice, addr: SocketAddr) -> String {
    match choice {
        Choice::Quic => format!("https://{addr}/"),
        _ => addr.to_string(),
    }
}

struct Peers(Vec<(String, Quic)>);
impl Peers {
    fn get(&self, peer: &str) -> Result<&Quic> {
        self.0
            .iter()
            .find(|(p, _)| p == peer)
            .map(|(_, q)| q)
            .ok_or_else(|| kide::error::Error::Invalid(format!("unconfigured QUIC peer: {peer}")))
    }
}
impl Transport for Peers {
    fn connect(&self, peer: &str) -> Result<Box<dyn Session>> {
        self.get(peer)?.connect(peer)
    }
    fn live(&self, peer: &str, events: LiveEvents) -> Result<Arc<dyn Live>> {
        self.get(peer)?.live(peer, events)
    }
}

pub fn client(folder: &Folder) -> Result<Arc<dyn Transport>> {
    if folder.config.transport != Choice::Quic {
        return Ok(Arc::new(Tcp::default()));
    }
    let key = secret(folder)?;
    let mut peers = Vec::new();
    for d in &folder.config.devices {
        let (Some(addr), Some(pin)) = (d.addr, d.certificate) else {
            return invalid("QUIC device needs address and certificate hash");
        };
        peers.push((address(Choice::Quic, addr), Quic::pinned(pin, key.clone())?));
    }
    Ok(Arc::new(Peers(peers)))
}

pub enum Serving {
    Quic(QuicServer),
    Tcp(Server),
}
impl Serving {
    pub fn start(folder: &Folder, net: &Network, mut served: Served) -> Result<Self> {
        if folder.config.transport != Choice::Quic {
            return Ok(Self::Tcp(Server::start(
                net.listen,
                net.devices.clone(),
                served,
            )?));
        }
        secret(folder)?;
        let devices: Vec<_> = folder.config.devices.iter().map(|d| d.key.0).collect();
        served.admission = Some(Arc::new(Admission {
            names: vec![net.listen.ip().to_string()],
            admits: Box::new(move |key| devices.contains(key)),
        }));
        let state = folder.kide();
        Ok(Self::Quic(QuicServer::from_der(
            net.listen,
            fs::read(state.join("tls.cert"))?,
            fs::read(state.join("tls.key"))?,
            move |p| (p == "/").then(|| served.clone()),
            None,
        )?))
    }
    pub fn addr(&self) -> SocketAddr {
        match self {
            Self::Quic(s) => s.addr(),
            Self::Tcp(s) => s.addr(),
        }
    }
    pub fn wait(&self) {
        loop {
            std::thread::park();
        }
    }
}
