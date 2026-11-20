//! The pulling end of a session (§4.2) as a state machine: calls queue
//! bytes to send, received bytes yield events. Requests may be pipelined;
//! answers come in request order.

use super::agree;
use super::frame::{Decoder, Greeting, Msg, brief, encode};
use super::live::{Kind, Streams, is_live, settle};
use crate::auth::Secret;
use crate::error::{Error, Result};
use crate::format::FORMAT;
use crate::hash::Hash;
use crate::iface::{LiveEvent, MAX_PUSH};
use std::collections::VecDeque;
use std::sync::Arc;

/// What received bytes completed.
#[derive(Debug)]
pub enum Event {
    /// The server's Hello: its root.
    Hello(Hash),
    /// The answer to the oldest open request: one result per id, in request
    /// order, the bytes as sent (unchecked) or `NotFound` for Missing.
    Answer(Vec<Result<Vec<u8>>>),
    /// The answer to a Fetch: the objects pushed, each with the id it was
    /// sent under (unchecked).
    Pushed(Vec<(Hash, Vec<u8>)>),
    /// A message on a live stream.
    Live(LiveEvent),
}

#[derive(PartialEq)]
enum Phase {
    New,
    Greeting,
    Ready,
    Closed,
}

/// An open request and the answers so far.
enum Asked {
    Ids {
        ids: Vec<Hash>,
        children: bool,
        got: Vec<Result<Vec<u8>>>,
    },
    /// The objects pushed so far, and their bytes.
    Fetch(Vec<(Hash, Vec<u8>)>, u64),
}

impl Asked {
    /// The event of a finished answer.
    fn event(self) -> Event {
        match self {
            Asked::Ids { got, .. } => Event::Answer(got),
            Asked::Fetch(pushed, _) => Event::Pushed(pushed),
        }
    }
}

pub struct Client {
    /// The key that answers a challenge, and the server's name as signed.
    key: Option<(Arc<Secret>, String)>,
    phase: Phase,
    decoder: Decoder,
    out: Vec<u8>,
    asked: VecDeque<Asked>,
    /// A pull or a live session, once the first request or live call says.
    kind: Option<Kind>,
    streams: Streams,
    /// The folder format this client expects of the server's root.
    format: u16,
    /// The protocol version and features agreed with the server.
    agreed: Option<(u8, u64)>,
}

impl Default for Client {
    fn default() -> Self {
        Client::new()
    }
}

impl Client {
    /// A client without a key: a server that asks for one refuses it.
    pub fn new() -> Client {
        Client {
            key: None,
            phase: Phase::New,
            decoder: Decoder::default(),
            out: Vec::new(),
            asked: VecDeque::new(),
            format: FORMAT,
            agreed: None,
            kind: None,
            streams: Streams::new(false),
        }
    }

    /// A client that answers a challenge with `secret`, for the server it
    /// knows as `server` (its host name, or address and port).
    pub fn with_key(secret: Arc<Secret>, server: &str) -> Client {
        Client {
            key: Some((secret, server.to_string())),
            ..Client::new()
        }
    }

    /// Expects the server's root to be of folder `format` (§5.4), not
    /// `FORMAT`; before Hello.
    #[cfg(test)]
    pub(crate) fn with_format(mut self, format: u16) -> Client {
        self.format = format;
        self
    }

    /// The protocol version and the features agreed in Hello.
    #[cfg(test)]
    pub(crate) fn agreed(&self) -> Option<(u8, u64)> {
        self.agreed
    }

    /// Sends Hello with this device's root; once, first.
    pub fn hello(&mut self, root: &Hash) -> Result<()> {
        if self.phase != Phase::New {
            return protocol("a second Hello");
        }
        self.send(&Msg::Hello(Greeting::new(*root, self.format, [0; 32])))?;
        self.phase = Phase::Greeting;
        Ok(())
    }

    /// Sends a Get, or a GetChildren of level-1 nodes, of 1 to 4,096 ids
    /// (`Invalid` otherwise). `Protocol` before the server's Hello.
    pub fn request(&mut self, ids: &[Hash], children: bool) -> Result<()> {
        self.ready(Kind::Pull)?;
        self.send(&match children {
            true => Msg::GetChildren(ids.to_vec()),
            false => Msg::Get(ids.to_vec()),
        })?;
        self.asked.push_back(Asked::Ids {
            ids: ids.to_vec(),
            children,
            got: Vec::with_capacity(ids.len()),
        });
        Ok(())
    }

    /// Sends a Fetch of what `want` reaches and the `have`s (at most
    /// 4,096, else `Invalid`) do not, within `scope`. `Protocol` before the
    /// server's Hello.
    pub fn fetch(&mut self, want: &Hash, have: &[Hash], scope: &[u8]) -> Result<()> {
        self.ready(Kind::Pull)?;
        self.send(&Msg::Fetch {
            want: *want,
            have: have.to_vec(),
            scope: scope.to_vec(),
        })?;
        self.asked.push_back(Asked::Fetch(Vec::new(), 0));
        Ok(())
    }

    /// Opens the live stream `name` at `version`; the server's LiveOpen or
    /// LiveClose comes as an event. The first live call makes the session a
    /// live one; a call that breaks a stream's state (§4.2), or any live
    /// call in a pull session, is `Protocol`.
    pub fn live_open(&mut self, name: &str, version: &[u8]) -> Result<()> {
        self.ready(Kind::Live)?;
        let m = self.streams.open(name, version)?;
        self.send(&m)
    }

    /// Sends an update on the open stream `name`.
    pub fn live_update(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        self.ready(Kind::Live)?;
        let m = self.streams.update(name, bytes)?;
        self.send(&m)
    }

    /// Closes the open stream `name`; the server's close comes as an event.
    pub fn live_close(&mut self, name: &str) -> Result<()> {
        self.ready(Kind::Live)?;
        let m = self.streams.close(name)?;
        self.send(&m)
    }

    /// Sends Ping, which keeps an idle live session open.
    pub fn ping(&mut self) -> Result<()> {
        self.ready(Kind::Live)?;
        self.send(&Msg::Ping)
    }

    /// `Protocol` before the server's Hello or in a session of another kind.
    fn ready(&mut self, kind: Kind) -> Result<()> {
        if self.phase != Phase::Ready {
            return protocol("a request before Hello");
        }
        settle(&mut self.kind, kind)
    }

    /// Sends Bye; the session is over.
    pub fn bye(&mut self) -> Result<()> {
        if self.phase != Phase::Closed {
            self.send(&Msg::Bye)?;
            self.phase = Phase::Closed;
        }
        Ok(())
    }

    /// The bytes to send now; the caller sends them in order.
    pub fn transmit(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// Takes received bytes. A server's challenge is answered with Auth.
    /// No common protocol version or another folder format is
    /// `Incompatible`, and a challenge without a key `Invalid`; both queue
    /// Bye. An unexpected or malformed message is `Protocol`.
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Vec<Event>> {
        self.decoder.push(bytes);
        let mut events = Vec::new();
        while let Some(m) = self.decoder.next_msg()? {
            events.extend(self.handle(m)?);
        }
        Ok(events)
    }

    /// The connection broke (`why`). The answer under way keeps what arrived
    /// and every later id carries `Unavailable`; with nothing arrived, the
    /// break itself is `Unavailable`.
    pub fn broken(&mut self, why: &str) -> Result<Event> {
        self.phase = Phase::Closed;
        let gone = || Error::Unavailable(why.to_string());
        match self.asked.pop_front() {
            Some(Asked::Ids { ids, mut got, .. }) if !got.is_empty() => {
                let rest = ids.len() - got.len();
                got.extend((0..rest).map(|_| Err(gone())));
                Ok(Event::Answer(got))
            }
            Some(Asked::Fetch(pushed, _)) if !pushed.is_empty() => Ok(Event::Pushed(pushed)),
            _ => Err(gone()),
        }
    }

    fn send(&mut self, m: &Msg) -> Result<()> {
        self.out.extend(encode(m)?);
        Ok(())
    }

    fn answer(&mut self, challenge: &[u8; 32]) -> Result<()> {
        let Some((secret, server)) = &self.key else {
            self.bye()?;
            return Err(Error::Invalid(
                "the server asks for a key; none is set".into(),
            ));
        };
        let auth = Msg::Auth {
            key: secret.public(),
            sig: secret.answer(server, challenge)?,
        };
        self.send(&auth)
    }

    fn handle(&mut self, m: Msg) -> Result<Option<Event>> {
        if self.phase == Phase::Greeting {
            return match m {
                Msg::Hello(theirs) => {
                    let mine = Greeting::new(Hash::ZERO, self.format, [0; 32]);
                    let agreed = agree(&mine, &theirs);
                    if agreed.is_err() {
                        self.bye()?;
                    }
                    self.agreed = Some(agreed?);
                    if theirs.challenge != [0; 32] {
                        self.answer(&theirs.challenge)?;
                    }
                    self.phase = Phase::Ready;
                    Ok(Some(Event::Hello(theirs.root)))
                }
                m => protocol(format!("expected Hello, got {}", brief(&m))),
            };
        }
        if is_live(&m) {
            if self.phase != Phase::Ready || self.kind != Some(Kind::Live) {
                return protocol(format!("unexpected {}", brief(&m)));
            }
            let (event, reply) = self.streams.received(m)?;
            if let Some(r) = reply {
                self.send(&r)?;
            }
            return Ok(event.map(Event::Live));
        }
        let Some(a) = self
            .asked
            .front_mut()
            .filter(|_| self.phase == Phase::Ready)
        else {
            return protocol(format!("unexpected {}", brief(&m)));
        };
        let (ids, children, got) = match a {
            Asked::Ids { ids, children, got } => (ids, *children, got),
            Asked::Fetch(pushed, total) => {
                return match m {
                    Msg::Object(id, bytes) => {
                        // §4.3: past 16 MiB only with the first object.
                        *total += bytes.len() as u64;
                        if *total > MAX_PUSH && !pushed.is_empty() {
                            return protocol(format!("a push of more than {MAX_PUSH} bytes"));
                        }
                        pushed.push((id, bytes));
                        Ok(None)
                    }
                    Msg::End => Ok(self.asked.pop_front().map(Asked::event)),
                    m => protocol(format!("expected Object or End, got {}", brief(&m))),
                };
            }
        };
        let Some(id) = ids.get(got.len()).copied() else {
            return match m {
                Msg::End => Ok(self.asked.pop_front().map(Asked::event)),
                m => protocol(format!("expected End, got {}", brief(&m))),
            };
        };
        let r = match m {
            Msg::Object(x, bytes) if x == id && !children => Ok(bytes),
            Msg::Children(x, bytes) if x == id && children => Ok(bytes),
            Msg::Missing(x) if x == id => Err(Error::NotFound(x)),
            m => return protocol(format!("expected {id}, got {}", brief(&m))),
        };
        got.push(r);
        Ok(None)
    }
}

fn protocol<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Protocol(msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{FEATURES, LOWEST, PROTOCOL};

    fn frames(msgs: &[Msg]) -> Vec<u8> {
        msgs.iter().flat_map(|m| encode(m).unwrap()).collect()
    }

    fn hello(protocol: u8) -> Msg {
        challenged(protocol, [0; 32])
    }

    fn challenged(protocol: u8, challenge: [u8; 32]) -> Msg {
        Msg::Hello(Greeting {
            low: protocol,
            high: protocol,
            ..Greeting::new(Hash([7; 32]), FORMAT, challenge)
        })
    }

    /// A client past Hello.
    fn ready() -> Client {
        let mut c = Client::new();
        c.hello(&Hash::ZERO).unwrap();
        c.transmit();
        let e = c.receive(&frames(&[hello(PROTOCOL)])).unwrap();
        assert!(matches!(e[..], [Event::Hello(r)] if r == Hash([7; 32])));
        c
    }

    fn answer(e: &[Event]) -> &[Result<Vec<u8>>] {
        match e {
            [Event::Answer(a)] => a,
            e => panic!("expected one answer, got {e:?}"),
        }
    }

    #[test]
    fn hello_is_sent_first() {
        let mut c = Client::new();
        assert!(matches!(
            c.request(&[Hash::ZERO], false),
            Err(Error::Protocol(_))
        ));
        c.hello(&Hash([1; 32])).unwrap();
        let want = frames(&[Msg::Hello(Greeting::new(Hash([1; 32]), FORMAT, [0; 32]))]);
        assert_eq!(c.transmit(), want);
        assert!(c.transmit().is_empty(), "taken once");
        assert!(matches!(
            c.request(&[Hash::ZERO], false),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn no_common_protocol_is_incompatible_and_says_bye() {
        for theirs in [PROTOCOL + 1, LOWEST - 1] {
            let mut c = Client::new();
            c.hello(&Hash::ZERO).unwrap();
            c.transmit();
            let r = c.receive(&frames(&[hello(theirs)]));
            assert!(matches!(r, Err(Error::Incompatible(_))), "{r:?}");
            assert_eq!(c.transmit(), frames(&[Msg::Bye]));
        }
    }

    #[test]
    fn the_highest_common_protocol_is_agreed() {
        let mut c = Client::new();
        c.hello(&Hash::ZERO).unwrap();
        let theirs = Greeting {
            low: LOWEST,
            high: PROTOCOL + 3,
            features: u64::MAX,
            ..Greeting::new(Hash::ZERO, FORMAT, [0; 32])
        };
        c.receive(&frames(&[Msg::Hello(theirs)])).unwrap();
        assert_eq!(c.agreed(), Some((PROTOCOL, FEATURES)));
    }

    #[test]
    fn another_folder_format_is_incompatible() {
        let mut c = Client::new().with_format(FORMAT + 1);
        c.hello(&Hash::ZERO).unwrap();
        c.transmit();
        let r = c.receive(&frames(&[hello(PROTOCOL)]));
        assert!(matches!(r, Err(Error::Incompatible(m)) if m.contains("format")));
        assert_eq!(c.transmit(), frames(&[Msg::Bye]));
    }

    #[test]
    fn the_hello_of_protocol_7_is_read_and_refused() {
        let mut c = Client::new();
        c.hello(&Hash::ZERO).unwrap();
        c.transmit();
        let old = [&[7][..], &[1; 32], &[0; 32]].concat();
        let mut f = (old.len() as u32 + 1).to_le_bytes().to_vec();
        f.push(0x01);
        f.extend(old);
        let r = c.receive(&f);
        assert!(matches!(r, Err(Error::Incompatible(m)) if m.contains("update the peer")));
    }

    #[test]
    fn a_challenge_is_answered_with_the_key() {
        let secret = Arc::new(Secret::from_bytes(&[5; 32]).unwrap());
        let mut c = Client::with_key(secret.clone(), "edge.example");
        c.hello(&Hash::ZERO).unwrap();
        c.transmit();
        let e = c
            .receive(&frames(&[challenged(PROTOCOL, [9; 32])]))
            .unwrap();
        assert!(matches!(e[..], [Event::Hello(_)]));
        let mut d = Decoder::default();
        d.push(&c.transmit());
        let Some(Msg::Auth { key, sig }) = d.next_msg().unwrap() else {
            panic!("expected Auth");
        };
        assert_eq!(key, secret.public());
        assert!(crate::auth::verify(&key, "edge.example", &[9; 32], &sig));
        c.request(&[Hash::ZERO], false).unwrap();

        let mut keyless = Client::new();
        keyless.hello(&Hash::ZERO).unwrap();
        keyless.transmit();
        let r = keyless.receive(&frames(&[challenged(PROTOCOL, [9; 32])]));
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        assert_eq!(keyless.transmit(), frames(&[Msg::Bye]));
    }

    #[test]
    fn a_push_past_its_limit_is_a_protocol_error() {
        let mut c = ready();
        c.fetch(&Hash::ZERO, &[], &[]).unwrap();
        let big = vec![0; crate::iface::MAX_FRAME - 33];
        let object = |i: u8| Msg::Object(Hash([i; 32]), big.clone());
        let n = MAX_PUSH as usize / big.len();
        for i in 0..n {
            assert!(
                c.receive(&frames(&[object(i as u8)])).is_ok(),
                "within the limit"
            );
        }
        let r = c.receive(&frames(&[object(n as u8)]));
        assert!(matches!(r, Err(Error::Protocol(_))), "{r:?}");
    }

    #[test]
    fn pipelined_answers_arrive_in_order() {
        let mut c = ready();
        let [a, b, x] = [1, 2, 3].map(|i| Hash([i; 32]));
        c.request(&[a, b], false).unwrap();
        c.request(&[x], true).unwrap();
        let sent = frames(&[Msg::Get(vec![a, b]), Msg::GetChildren(vec![x])]);
        assert_eq!(c.transmit(), sent);
        let back = frames(&[
            Msg::Object(a, vec![1]),
            Msg::Missing(b),
            Msg::End,
            Msg::Children(x, vec![2, 3]),
            Msg::End,
        ]);
        let (first, second) = back.split_at(back.len() - 10);
        let e = c.receive(first).unwrap();
        let got = answer(&e);
        assert_eq!(got[0].as_ref().ok(), Some(&vec![1]));
        assert!(matches!(got[1], Err(Error::NotFound(id)) if id == b));
        let e = c.receive(second).unwrap();
        assert_eq!(answer(&e)[0].as_ref().ok(), Some(&vec![2, 3]));
    }

    #[test]
    fn wrong_answers_are_protocol_errors() {
        let [a, b] = [1, 2].map(|i| Hash([i; 32]));
        let wrong = [
            ("another id", Msg::Object(b, vec![])),
            ("Children for a Get", Msg::Children(a, vec![])),
            ("End too early", Msg::End),
            ("a second Hello", hello(PROTOCOL)),
        ];
        for (what, m) in wrong {
            let mut c = ready();
            c.request(&[a], false).unwrap();
            let r = c.receive(&frames(&[m]));
            assert!(matches!(r, Err(Error::Protocol(_))), "{what}: {r:?}");
        }
        let mut c = ready();
        let r = c.receive(&frames(&[Msg::End]));
        assert!(matches!(r, Err(Error::Protocol(_))), "nothing asked");
        let mut c = ready();
        c.request(&[a], false).unwrap();
        let r = c.receive(&frames(&[Msg::Missing(a), Msg::Missing(a)]));
        assert!(matches!(r, Err(Error::Protocol(_))), "End expected");
    }

    #[test]
    fn a_break_keeps_what_arrived() {
        let ids = [1, 2, 3].map(|i| Hash([i; 32]));
        let mut c = ready();
        c.request(&ids, false).unwrap();
        c.receive(&frames(&[Msg::Object(ids[0], vec![5])])).unwrap();
        let Ok(Event::Answer(got)) = c.broken("reset") else {
            panic!("expected the partial answer");
        };
        assert_eq!(got[0].as_ref().ok(), Some(&vec![5]));
        assert!(
            got[1..]
                .iter()
                .all(|r| matches!(r, Err(Error::Unavailable(_))))
        );
        let mut c = ready();
        c.request(&ids, false).unwrap();
        assert!(matches!(c.broken("reset"), Err(Error::Unavailable(_))));
        assert!(matches!(
            Client::new().broken("x"),
            Err(Error::Unavailable(_))
        ));
    }

    #[test]
    fn bad_requests_are_invalid() {
        let mut c = ready();
        assert!(matches!(c.request(&[], false), Err(Error::Invalid(_))));
        let many = vec![Hash::ZERO; crate::iface::MAX_GET + 1];
        assert!(matches!(c.request(&many, true), Err(Error::Invalid(_))));
        assert!(c.transmit().is_empty(), "nothing sent");
    }
}
