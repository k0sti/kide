//! The addresses of §4.2 and §5.8, taken from a folder's configuration.

use kide::auth::PeerKey;
use kide::error::{Result, invalid};
use kide::folder::config::{Config, Transport};
use kide::folder::npub::Key;
use std::net::{IpAddr, SocketAddr};

/// Where this device listens, whom it accepts, and where its devices are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub listen: SocketAddr,
    /// The devices a connection is accepted from: each key and its source
    /// address.
    pub devices: Vec<(PeerKey, IpAddr)>,
    /// Every device, and the address a pull connects to.
    pub peers: Vec<(Key, SocketAddr)>,
}

impl Network {
    /// In the FIPS transport, every address is a key's FIPS address with the
    /// configured port; in QUIC/plain, the configured address. `Invalid`
    /// without `self`.
    pub fn of(c: &Config) -> Result<Network> {
        let Some(this) = c.this else {
            return invalid("the configuration names no self device");
        };
        let at = |key: &Key, addr: Option<SocketAddr>| match c.transport {
            Transport::Fips => fips_at(key, c.port),
            Transport::Quic | Transport::Plain => {
                addr.map_or_else(|| invalid(format!("{key} has no address")), Ok)
            }
        };
        let peers = c.devices.iter().map(|d| Ok((d.key, at(&d.key, d.addr)?)));
        let peers = peers.collect::<Result<Vec<_>>>()?;
        Ok(Network {
            listen: at(&this.key, this.addr)?,
            devices: peers.iter().map(|p| (p.0.0, p.1.ip())).collect(),
            peers,
        })
    }
}

#[cfg(feature = "fips")]
fn fips_at(key: &Key, port: u16) -> Result<SocketAddr> {
    let addr = kide::transport::fips::address(&key.0);
    Ok(SocketAddr::new(addr.into(), port))
}

#[cfg(not(feature = "fips"))]
fn fips_at(_: &Key, _: u16) -> Result<SocketAddr> {
    invalid("transport fips needs a build with the Cargo feature fips")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kide::transport::device;

    fn key(i: u8) -> Key {
        Key(kide::hash::h(&[i]).0)
    }

    #[cfg(feature = "fips")]
    #[test]
    fn fips_addresses_come_from_the_keys() {
        let text = format!(
            "transport = fips\nself = {}\ndevice = {}\nport = 9000\n",
            key(0),
            key(1)
        );
        let n = Network::of(&Config::parse(&text).unwrap()).unwrap();
        let fips = |i| IpAddr::from(kide::transport::fips::address(&key(i).0));
        assert_eq!(n.listen, SocketAddr::new(fips(0), 9000));
        assert_eq!(n.peers, [(key(1), SocketAddr::new(fips(1), 9000))]);
        assert_eq!(device(fips(1), &n.devices), Some(key(1).0), "a device");
        for refused in [fips(0), fips(2), "::1".parse().unwrap()] {
            assert_eq!(device(refused, &n.devices), None, "{refused}");
        }
    }

    #[test]
    fn plain_addresses_are_configured() {
        let text = format!(
            "transport = plain\nself = {} [::1]:7840\ndevice = {} 127.0.0.1:7841\n",
            key(0),
            key(1)
        );
        let n = Network::of(&Config::parse(&text).unwrap()).unwrap();
        assert_eq!(n.listen, "[::1]:7840".parse().unwrap());
        assert_eq!(n.peers, [(key(1), "127.0.0.1:7841".parse().unwrap())]);
        let at = |a: &str| device(a.parse().unwrap(), &n.devices);
        assert_eq!(at("127.0.0.1"), Some(key(1).0));
        assert_eq!(at("::1"), None);
    }

    #[test]
    fn without_self_there_is_no_network() {
        assert!(Network::of(&Config::default()).is_err());
    }
}
