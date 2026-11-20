//! Transports (§4.2): blocking drivers of the protocol state machines, an
//! in-process transport for tests, TCP with its trust rule, QUIC and the
//! gateway that relays QUIC sessions to TCP servers.

mod driver;
#[cfg(feature = "fips")]
pub mod fips;
#[cfg(feature = "quic")]
mod gateway;
#[cfg(feature = "https")]
mod https;
#[cfg(feature = "https")]
pub use https::{Backs as HttpsBacks, Https, HttpsServer, rustls as https_tls};
mod inproc;
pub mod live;
mod pipe;
#[cfg(feature = "quic")]
mod quic;
mod server;
mod tcp;

#[cfg(feature = "quic")]
pub use gateway::Gateway;
pub use inproc::InProcess;
pub use live::PING;
#[cfg(feature = "quic")]
pub use quic::{Backs, Quic, QuicServer};
pub use server::{Server, device};
pub use tcp::{TIMEOUT, Tcp};

pub use crate::protocol::Served;

use crate::error::{Error, Result};
use crate::iface::LiveEvent;
use crate::protocol::Responder;

/// The most sessions a server serves at once (§4.2); it refuses more.
pub const MAX_SESSIONS: usize = 1024;

/// A broken, reset or timed-out connection is `Unavailable` (§4.4).
pub fn net_err(e: std::io::Error) -> Error {
    Error::Unavailable(format!("connection: {e}"))
}

/// 32 bytes from the operating system's random source: a challenge (§4.2).
pub fn nonce() -> std::io::Result<[u8; 32]> {
    let mut n = [0; 32];
    getrandom::fill(&mut n).map_err(std::io::Error::other)?;
    Ok(n)
}

/// The host of an `https://host:port/...` URL: the name a client signs in
/// the challenge (§4.2), over QUIC and from the browser.
pub fn url_host(url: &str) -> Result<&str> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(Error::Invalid(format!("not an https URL: {url:?}")));
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    match host.is_empty() {
        true => Err(Error::Invalid(format!("no host in {url:?}"))),
        false => Ok(host),
    }
}

/// Refuses every live stream a client opens, where no application takes them.
fn refuse_live(r: &mut Responder, events: Vec<LiveEvent>) -> Result<()> {
    for e in events {
        if let LiveEvent::Open { name, .. } = e {
            r.live_close(&name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use crate::protocol::PROTOCOL as VERSION;
    use crate::protocol::{Client, Event, sample_served};

    #[test]
    fn hosts_come_from_urls() {
        assert_eq!(
            url_host("https://edge.example:4433/kide").unwrap(),
            "edge.example"
        );
        assert_eq!(url_host("https://[::1]:4433").unwrap(), "::1");
        assert!(url_host("http://edge.example").is_err());
        assert!(url_host("https://:4433").is_err());
    }

    #[test]
    fn pull_drivers_refuse_live_streams() {
        let mut r = Responder::new(sample_served(&[], Hash::ZERO, VERSION), [1; 32]);
        let mut c = Client::new();
        let drain = |r: &mut Responder| -> Vec<u8> {
            std::iter::from_fn(|| r.transmit().unwrap())
                .flatten()
                .collect()
        };
        c.hello(&Hash::ZERO).unwrap();
        r.receive(&c.transmit()).unwrap();
        c.receive(&drain(&mut r)).unwrap();
        c.live_open("doc", b"v").unwrap();
        let events = r.receive(&c.transmit()).unwrap();
        refuse_live(&mut r, events).unwrap();
        let e = c.receive(&drain(&mut r)).unwrap();
        assert!(
            matches!(&e[..], [Event::Live(LiveEvent::Close { name })] if name == "doc"),
            "{e:?}"
        );
    }
}
