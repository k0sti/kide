//! Live sessions (§4.2): named streams of application bytes, full duplex.
//! The client opens a name, the server accepts or refuses it, and then both
//! send updates until either closes it; a close is answered with a close.

use super::frame::{Msg, brief};
use crate::error::{Error, Result};
use crate::iface::LiveEvent;
use std::collections::HashMap;

/// The most streams one session may hold at a server, in any state but
/// closed (§4.2).
pub const MAX_STREAMS: usize = 1024;

/// What a session is, decided by its first message after Hello and Auth.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Kind {
    Pull,
    Live,
}

/// Fixes a session's kind; `Protocol` if it is already the other one.
pub(super) fn settle(kind: &mut Option<Kind>, k: Kind) -> Result<()> {
    match *kind.get_or_insert(k) == k {
        true => Ok(()),
        false => protocol(match k {
            Kind::Pull => "a request in a live session",
            Kind::Live => "a live message in a pull session",
        }),
    }
}

/// Whether a message belongs to a live session.
pub(super) fn is_live(m: &Msg) -> bool {
    matches!(
        m,
        Msg::LiveOpen { .. } | Msg::LiveUpdate { .. } | Msg::LiveClose(_) | Msg::Ping
    )
}

#[derive(Clone, Copy)]
enum State {
    /// At the client: opened, waiting for the server's answer.
    Opening,
    /// At the server: opened by the client, waiting for the application.
    Asked,
    Open,
    /// Closed by this side, waiting for the peer's close.
    Closing,
}

/// The streams of one end of a live session and the rules they follow.
pub(super) struct Streams {
    server: bool,
    names: HashMap<String, State>,
}

impl Streams {
    pub(super) fn new(server: bool) -> Streams {
        Streams {
            server,
            names: HashMap::new(),
        }
    }

    /// Whether `name` is in any state but closed.
    pub(super) fn knows(&self, name: &str) -> bool {
        self.names.contains_key(name)
    }

    /// The LiveOpen that opens `name` (client) or accepts it (server).
    pub(super) fn open(&mut self, name: &str, version: &[u8]) -> Result<Msg> {
        let next = match (self.server, self.names.get(name)) {
            (false, None) => State::Opening,
            (true, Some(State::Asked)) => State::Open,
            _ => return protocol(format!("opening {name:?}, which is not closed or asked")),
        };
        self.names.insert(name.into(), next);
        Ok(Msg::LiveOpen {
            name: name.into(),
            version: version.to_vec(),
        })
    }

    /// The LiveUpdate of `bytes` on the open stream `name`.
    pub(super) fn update(&self, name: &str, bytes: &[u8]) -> Result<Msg> {
        match self.names.get(name) {
            Some(State::Open) => Ok(Msg::LiveUpdate {
                name: name.into(),
                bytes: bytes.to_vec(),
            }),
            _ => protocol(format!("an update on {name:?}, which is not open")),
        }
    }

    /// The LiveClose that closes the open stream `name`, or refuses it at
    /// the server.
    pub(super) fn close(&mut self, name: &str) -> Result<Msg> {
        match self.names.get(name) {
            Some(State::Open) => self.names.insert(name.into(), State::Closing),
            Some(State::Asked) => self.names.remove(name),
            _ => return protocol(format!("closing {name:?}, which is not open")),
        };
        Ok(Msg::LiveClose(name.into()))
    }

    /// Takes a received live message: the event for the application and the
    /// reply to send, if any. Updates on a stream this side is closing are
    /// dropped; anything else that breaks the rules is `Protocol`.
    pub(super) fn received(&mut self, m: Msg) -> Result<(Option<LiveEvent>, Option<Msg>)> {
        let state = match &m {
            Msg::LiveOpen { name, .. } | Msg::LiveUpdate { name, .. } | Msg::LiveClose(name) => {
                self.names.get(name).copied()
            }
            _ => None,
        };
        let event = match (self.server, state, m) {
            (_, _, Msg::Ping) => None,
            (true, None, Msg::LiveOpen { .. }) if self.names.len() >= MAX_STREAMS => {
                return protocol(format!("more than {MAX_STREAMS} live streams"));
            }
            (true, None, Msg::LiveOpen { name, version })
            | (false, Some(State::Opening), Msg::LiveOpen { name, version }) => {
                let next = match self.server {
                    true => State::Asked,
                    false => State::Open,
                };
                self.names.insert(name.clone(), next);
                Some(LiveEvent::Open { name, version })
            }
            (_, Some(State::Open), Msg::LiveUpdate { name, bytes }) => {
                Some(LiveEvent::Update { name, bytes })
            }
            (_, Some(State::Closing), Msg::LiveUpdate { .. }) => None,
            (_, Some(State::Open), Msg::LiveClose(name)) => {
                self.names.remove(&name);
                let echo = Msg::LiveClose(name.clone());
                return Ok((Some(LiveEvent::Close { name }), Some(echo)));
            }
            (false, Some(State::Opening), Msg::LiveClose(name))
            | (_, Some(State::Closing), Msg::LiveClose(name)) => {
                self.names.remove(&name);
                Some(LiveEvent::Close { name })
            }
            (_, _, m) => return protocol(format!("unexpected {}", brief(&m))),
        };
        Ok((event, None))
    }
}

fn protocol<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Protocol(msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PeerKey;
    use crate::hash::Hash;
    use crate::protocol::PROTOCOL as VERSION;
    use crate::protocol::frame::encode;
    use crate::protocol::{Client, Event, Responder, Served, sample_served};
    use std::sync::Arc;

    #[test]
    fn a_server_holds_at_most_max_streams() {
        let mut s = Streams::new(true);
        let open = |i: usize| Msg::LiveOpen {
            name: format!("s{i}"),
            version: vec![],
        };
        for i in 0..MAX_STREAMS {
            s.received(open(i)).unwrap();
        }
        assert!(matches!(
            s.received(open(MAX_STREAMS)),
            Err(Error::Protocol(_))
        ));
    }

    /// Events compare by what they print: `Ended` holds an error.
    impl PartialEq for LiveEvent {
        fn eq(&self, other: &Self) -> bool {
            format!("{self:?}") == format!("{other:?}")
        }
    }

    fn served() -> Served {
        sample_served(&[], Hash([2; 32]), VERSION)
    }

    /// A client and a responder past Hello.
    fn pair(served: Served) -> (Client, Responder) {
        let mut c = Client::new();
        let mut r = Responder::new(served, [1; 32]);
        c.hello(&Hash::ZERO).unwrap();
        r.receive(&c.transmit()).unwrap();
        c.receive(&drain(&mut r)).unwrap();
        (c, r)
    }

    fn drain(r: &mut Responder) -> Vec<u8> {
        std::iter::from_fn(|| r.transmit().unwrap())
            .flatten()
            .collect()
    }

    /// Delivers what the client queued to the responder.
    fn up(c: &mut Client, r: &mut Responder) -> Vec<LiveEvent> {
        r.receive(&c.transmit()).unwrap()
    }

    /// Delivers what the responder queued to the client.
    fn down(r: &mut Responder, c: &mut Client) -> Vec<LiveEvent> {
        let events = c.receive(&drain(r)).unwrap();
        events
            .into_iter()
            .map(|e| match e {
                Event::Live(e) => e,
                e => panic!("expected a live event, got {e:?}"),
            })
            .collect()
    }

    fn open(name: &str, version: &[u8]) -> LiveEvent {
        LiveEvent::Open {
            name: name.into(),
            version: version.to_vec(),
        }
    }

    fn update(name: &str, bytes: &[u8]) -> LiveEvent {
        LiveEvent::Update {
            name: name.into(),
            bytes: bytes.to_vec(),
        }
    }

    fn close(name: &str) -> LiveEvent {
        LiveEvent::Close { name: name.into() }
    }

    #[test]
    fn a_stream_opens_carries_both_ways_and_closes() {
        let (mut c, mut r) = pair(served());
        c.live_open("doc", b"v1").unwrap();
        assert_eq!(up(&mut c, &mut r), [open("doc", b"v1")]);
        r.live_open("doc", b"v2").unwrap();
        r.live_update("doc", b"catch-up").unwrap();
        assert_eq!(
            down(&mut r, &mut c),
            [open("doc", b"v2"), update("doc", b"catch-up")]
        );
        c.live_update("doc", b"edit 1").unwrap();
        c.live_update("doc", b"edit 2").unwrap();
        r.live_update("doc", b"relayed").unwrap();
        assert_eq!(
            up(&mut c, &mut r),
            [update("doc", b"edit 1"), update("doc", b"edit 2")]
        );
        assert_eq!(down(&mut r, &mut c), [update("doc", b"relayed")]);

        c.live_close("doc").unwrap();
        assert_eq!(up(&mut c, &mut r), [close("doc")]);
        assert_eq!(down(&mut r, &mut c), [close("doc")], "the echo");
        assert!(r.live_update("doc", b"late").is_err());
        c.live_open("doc", b"v3").unwrap();
        assert_eq!(up(&mut c, &mut r), [open("doc", b"v3")], "reopened");
    }

    #[test]
    fn the_server_refuses_by_read_rights_or_by_the_application() {
        let may = |_: Option<&PeerKey>, name: &str| name.starts_with("region/");
        let served = Served {
            reads: Some(Arc::new(may)),
            ..served()
        };
        let (mut c, mut r) = pair(served);
        c.live_open("secret", b"").unwrap();
        assert_eq!(up(&mut c, &mut r), [], "the application never sees it");
        assert_eq!(down(&mut r, &mut c), [close("secret")]);
        assert!(c.live_update("secret", b"x").is_err(), "refused");

        c.live_open("region/1", b"").unwrap();
        assert_eq!(up(&mut c, &mut r), [open("region/1", b"")]);
        r.live_close("region/1").unwrap();
        assert_eq!(down(&mut r, &mut c), [close("region/1")]);
        assert_eq!(up(&mut c, &mut r), [], "a refusal needs no echo");
        c.live_open("region/1", b"").unwrap();
        assert_eq!(up(&mut c, &mut r), [open("region/1", b"")]);
    }

    #[test]
    fn closes_that_cross_updates_or_each_other_are_no_error() {
        let (mut c, mut r) = pair(served());
        for name in ["a", "b"] {
            c.live_open(name, b"").unwrap();
            up(&mut c, &mut r);
            r.live_open(name, b"").unwrap();
        }
        down(&mut r, &mut c);
        // "a": the server closes while the client updates; "b": both close.
        c.live_update("a", b"in flight").unwrap();
        c.live_close("b").unwrap();
        r.live_close("a").unwrap();
        r.live_close("b").unwrap();
        assert_eq!(up(&mut c, &mut r), [close("b")], "the update is dropped");
        assert_eq!(down(&mut r, &mut c), [close("a"), close("b")]);
        assert_eq!(up(&mut c, &mut r), [close("a")], "the client's echo");
        assert_eq!(down(&mut r, &mut c), [], "nothing more");
    }

    #[test]
    fn pings_keep_quiet() {
        let (mut c, mut r) = pair(served());
        c.ping().unwrap();
        assert_eq!(up(&mut c, &mut r), []);
        r.ping().unwrap();
        assert_eq!(down(&mut r, &mut c), []);
        c.live_open("x", b"").unwrap();
        assert_eq!(up(&mut c, &mut r), [open("x", b"")], "the session is live");
    }

    #[test]
    fn pulls_and_live_streams_do_not_share_a_session() {
        let (mut c, _) = pair(served());
        c.request(&[Hash::ZERO], false).unwrap();
        assert!(matches!(c.live_open("x", b""), Err(Error::Protocol(_))));
        assert!(matches!(c.ping(), Err(Error::Protocol(_))));
        let (mut c, _) = pair(served());
        c.ping().unwrap();
        assert!(matches!(
            c.request(&[Hash::ZERO], false),
            Err(Error::Protocol(_))
        ));
        assert!(matches!(
            c.fetch(&Hash::ZERO, &[], b""),
            Err(Error::Protocol(_))
        ));

        let get = encode(&Msg::Get(vec![Hash::ZERO])).unwrap();
        let ping = encode(&Msg::Ping).unwrap();
        let (_, mut r) = pair(served());
        r.receive(&ping).unwrap();
        assert!(matches!(r.receive(&get), Err(Error::Protocol(_))));
        let (_, mut r) = pair(served());
        r.receive(&get).unwrap();
        assert!(matches!(r.receive(&ping), Err(Error::Protocol(_))));
        assert!(matches!(r.ping(), Err(Error::Protocol(_))));

        let (mut c, _) = pair(served());
        c.request(&[Hash::ZERO], false).unwrap();
        assert!(matches!(c.receive(&ping), Err(Error::Protocol(_))));
    }

    #[test]
    fn streams_follow_their_states() {
        let (mut c, mut r) = pair(served());
        let calls: [(&str, Result<()>); 3] = [
            ("an update before opening", c.live_update("x", b"")),
            ("closing before opening", c.live_close("x")),
            ("accepting what was not asked", r.live_open("x", b"")),
        ];
        for (what, res) in calls {
            assert!(matches!(res, Err(Error::Protocol(_))), "{what}");
        }
        c.live_open("x", b"").unwrap();
        let calls: [(&str, Result<()>); 3] = [
            ("an update while opening", c.live_update("x", b"")),
            ("closing while opening", c.live_close("x")),
            ("opening twice", c.live_open("x", b"")),
        ];
        for (what, res) in calls {
            assert!(matches!(res, Err(Error::Protocol(_))), "{what}");
        }
        up(&mut c, &mut r);
        assert!(r.live_update("x", b"").is_err(), "an update while asked");

        let msg = |m: Msg| encode(&m).unwrap();
        let wrong = [
            (
                "an update on no stream",
                msg(Msg::LiveUpdate {
                    name: "y".into(),
                    bytes: vec![],
                }),
            ),
            ("a close of no stream", msg(Msg::LiveClose("y".into()))),
            (
                "opening an asked stream",
                msg(Msg::LiveOpen {
                    name: "x".into(),
                    version: vec![],
                }),
            ),
        ];
        for (what, bytes) in wrong {
            let (mut c, mut r) = pair(served());
            c.live_open("x", b"").unwrap();
            up(&mut c, &mut r);
            let res = r.receive(&bytes);
            assert!(matches!(res, Err(Error::Protocol(_))), "server, {what}");
        }
        let (mut c, _) = pair(served());
        c.ping().unwrap();
        let accept = msg(Msg::LiveOpen {
            name: "z".into(),
            version: vec![],
        });
        let res = c.receive(&accept);
        assert!(matches!(res, Err(Error::Protocol(_))), "an accept unasked");
    }
}
