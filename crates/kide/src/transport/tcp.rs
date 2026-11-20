//! `Tcp`, the client: one TCP connection per session (§4.2) to a peer given
//! as `address:port`, and a 30-second limit on every read and write.

use super::driver::Blocking;
use super::live::{Close, PING, connect};
use super::net_err;
use crate::error::{Error, Result};
use crate::iface::{Live, LiveEvents, Session, Transport};
use crate::protocol::Client;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// The longest a read may wait (§4.2).
pub const TIMEOUT: Duration = Duration::from_secs(30);

pub struct Tcp {
    pub timeout: Duration,
}

impl Default for Tcp {
    fn default() -> Tcp {
        Tcp { timeout: TIMEOUT }
    }
}

impl Tcp {
    fn open(&self, peer: &str) -> Result<TcpStream> {
        let addr: SocketAddr = peer
            .parse()
            .map_err(|_| Error::Invalid(format!("not an address and port: {peer:?}")))?;
        let stream = TcpStream::connect_timeout(&addr, self.timeout)
            .map_err(|e| Error::Unavailable(format!("{peer}: {e}")))?;
        prepare(&stream, self.timeout).map_err(net_err)?;
        Ok(stream)
    }
}

impl Transport for Tcp {
    fn connect(&self, peer: &str) -> Result<Box<dyn Session>> {
        Ok(Box::new(Blocking::new(self.open(peer)?)))
    }

    fn live(&self, peer: &str, events: LiveEvents) -> Result<Arc<dyn Live>> {
        let stream = self.open(peer)?;
        let reader = stream.try_clone().map_err(net_err)?;
        let close = closer(&stream)?;
        connect(reader, stream, Client::new(), close, events, PING)
    }
}

/// Breaks `stream`, from any thread.
pub(super) fn closer(stream: &TcpStream) -> Result<Close> {
    let s = stream.try_clone().map_err(net_err)?;
    Ok(Box::new(move || {
        let _ = s.shutdown(Shutdown::Both);
    }))
}

/// Sets the limits of §4.2 on a new connection, either side.
pub(super) fn prepare(stream: &TcpStream, timeout: Duration) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_nodelay(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::FORMAT;
    use crate::hash::Hash;
    use crate::protocol::PROTOCOL as VERSION;
    use crate::protocol::frame::Greeting;
    use crate::protocol::frame::{Decoder, Msg, encode};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn write_msg(c: &mut TcpStream, m: &Msg) -> std::io::Result<()> {
        c.write_all(&encode(m).unwrap())
    }

    /// Reads one message a byte at a time, so nothing after it is consumed.
    fn read_msg(c: &mut TcpStream) -> std::io::Result<Msg> {
        let mut d = Decoder::default();
        let mut b = [0];
        loop {
            c.read_exact(&mut b)?;
            d.push(&b);
            if let Some(m) = d.next_msg().unwrap() {
                return Ok(m);
            }
        }
    }

    /// A hand-written server: answers Hello, then runs `script` on the stream.
    fn scripted(protocol: u8, script: fn(&mut TcpStream)) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            read_msg(&mut c).unwrap();
            let hello = Msg::Hello(Greeting {
                low: protocol,
                high: protocol,
                ..Greeting::new(Hash::ZERO, FORMAT, [0; 32])
            });
            write_msg(&mut c, &hello).unwrap();
            script(&mut c);
        });
        addr
    }

    fn session(addr: &str, timeout: Duration) -> Box<dyn Session> {
        Tcp { timeout }.connect(addr).unwrap()
    }

    #[test]
    fn another_protocol_is_incompatible_and_says_bye() {
        let addr = scripted(VERSION + 1, |c| {
            assert_eq!(read_msg(c).unwrap(), Msg::Bye);
        });
        let r = session(&addr, TIMEOUT).hello(&Hash::ZERO);
        assert!(matches!(r, Err(Error::Incompatible(_))), "{r:?}");
    }

    #[test]
    fn a_silent_server_times_out_as_unavailable() {
        let addr = scripted(VERSION, |c| {
            let _ = read_msg(c);
            thread::sleep(Duration::from_millis(600));
        });
        let mut s = session(&addr, Duration::from_millis(150));
        s.hello(&Hash::ZERO).unwrap();
        let r = s.get(&[Hash([1; 32])]);
        assert!(matches!(r, Err(Error::Unavailable(_))), "{r:?}");
    }

    #[test]
    fn a_wrong_answer_is_a_protocol_error() {
        let addr = scripted(VERSION, |c| {
            read_msg(c).unwrap();
            write_msg(c, &Msg::Object(Hash([9; 32]), vec![1])).unwrap();
            write_msg(c, &Msg::End).unwrap();
        });
        let mut s = session(&addr, TIMEOUT);
        s.hello(&Hash::ZERO).unwrap();
        assert!(matches!(s.get(&[Hash([1; 32])]), Err(Error::Protocol(_))));
    }

    #[test]
    fn answers_before_a_break_are_kept() {
        let addr = scripted(VERSION, |c| {
            let Msg::Get(ids) = read_msg(c).unwrap() else {
                panic!("expected Get");
            };
            write_msg(c, &Msg::Object(ids[0], vec![1])).unwrap();
            write_msg(c, &Msg::Missing(ids[1])).unwrap();
        }); // the connection closes
        let mut s = session(&addr, TIMEOUT);
        s.hello(&Hash::ZERO).unwrap();
        let ids = [Hash([1; 32]), Hash([2; 32]), Hash([3; 32]), Hash([4; 32])];
        let got = s.get(&ids).unwrap();
        assert_eq!(got[0].as_ref().ok(), Some(&vec![1]));
        assert!(matches!(got[1], Err(Error::NotFound(x)) if x == ids[1]));
        assert!(
            got[2..]
                .iter()
                .all(|r| matches!(r, Err(Error::Unavailable(_))))
        );
    }

    #[test]
    fn bad_addresses_are_invalid() {
        for peer in ["nowhere", "[::1]", "1.2.3.4:x"] {
            let r = Tcp::default().connect(peer);
            assert!(matches!(r, Err(Error::Invalid(_))), "{peer}");
        }
    }
}
