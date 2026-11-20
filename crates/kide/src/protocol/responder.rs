//! The serving end of a session (§4.2) as a state machine: received bytes
//! queue answers, which are read from the store and encoded one frame at a
//! time as the driver asks for them, so a large answer never sits in memory.
//! With read rights, only granted ids are served (§4.2).

use super::Served;
use super::frame::{Decoder, Greeting, Msg, brief, encode};
use super::grants::Grants;
use super::live::{Kind, Streams, is_live, settle};
use super::push::Push;
use crate::auth::{PeerKey, verify};
use crate::error::{Error, Result};
use crate::format::node::Node;
use crate::hash::Hash;
use crate::iface::LiveEvent;
use crate::load::split_children;
use std::collections::VecDeque;

/// One frame still to send; a push sends one frame per object, then End.
enum Reply {
    Hello,
    Object(Hash),
    Children(Hash),
    Push(Box<Push>),
    End,
    /// A live message, sent as it is.
    Live(Msg),
}

#[derive(PartialEq)]
enum Phase {
    Greeting,
    /// Waiting for the answer to the challenge.
    Proving,
    Ready,
    Done,
}

pub struct Responder {
    served: Served,
    /// Zero unless the client must prove a key.
    challenge: [u8; 32],
    peer: Option<PeerKey>,
    phase: Phase,
    decoder: Decoder,
    replies: VecDeque<Reply>,
    /// The root sent in Hello.
    root: Option<Hash>,
    /// With read rights: what this session may be served.
    grants: Option<Grants>,
    /// A pull or a live session, once the first request or live message says.
    kind: Option<Kind>,
    streams: Streams,
}

impl Responder {
    /// A responder; `nonce` is fresh randomness from the driver, sent as the
    /// challenge if the server admits clients by key.
    pub fn new(served: Served, nonce: [u8; 32]) -> Responder {
        let challenge = match served.admission {
            Some(_) => nonce,
            None => [0; 32],
        };
        Responder {
            served,
            challenge,
            peer: None,
            phase: Phase::Greeting,
            decoder: Decoder::default(),
            replies: VecDeque::new(),
            root: None,
            grants: None,
            kind: None,
            streams: Streams::new(true),
        }
    }

    /// A responder whose transport identified the client as `peer` (a
    /// configured device, by its address); a challenge still overrides it.
    pub fn with_peer(self, peer: PeerKey) -> Responder {
        Responder {
            peer: Some(peer),
            ..self
        }
    }

    /// The client's key: proved by the challenge, or given by the transport.
    pub fn peer(&self) -> Option<&PeerKey> {
        self.peer.as_ref()
    }

    /// Takes received bytes: Hello first, then Auth if challenged, then
    /// either requests or live messages until Bye. Anything else, or anything
    /// after Bye, is `Protocol`; a wrong answer or a key not admitted is
    /// `Invalid`. Returns what live streams received, for the application,
    /// which answers each Open with `live_open` or `live_close`; an open the
    /// read rights refuse is closed here and never returned.
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Vec<LiveEvent>> {
        self.decoder.push(bytes);
        let mut events = Vec::new();
        while let Some(m) = self.decoder.next_msg()? {
            events.extend(self.handle(m)?);
        }
        Ok(events)
    }

    /// Accepts the stream `name` the client opened, at the server's
    /// `version`. Each live call is `Protocol` outside a live session or
    /// where it breaks a stream's state (§4.2).
    pub fn live_open(&mut self, name: &str, version: &[u8]) -> Result<()> {
        self.live_ready()?;
        let m = self.streams.open(name, version)?;
        self.replies.push_back(Reply::Live(m));
        Ok(())
    }

    /// Sends an update on the open stream `name`.
    pub fn live_update(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        self.live_ready()?;
        let m = self.streams.update(name, bytes)?;
        self.replies.push_back(Reply::Live(m));
        Ok(())
    }

    /// Closes the open stream `name`, or refuses one the client opened.
    pub fn live_close(&mut self, name: &str) -> Result<()> {
        self.live_ready()?;
        let m = self.streams.close(name)?;
        self.replies.push_back(Reply::Live(m));
        Ok(())
    }

    /// Sends Ping, which keeps an idle live session open.
    pub fn ping(&mut self) -> Result<()> {
        self.live_ready()?;
        self.replies.push_back(Reply::Live(Msg::Ping));
        Ok(())
    }

    fn live_ready(&self) -> Result<()> {
        match self.phase == Phase::Ready && self.kind == Some(Kind::Live) {
            true => Ok(()),
            false => Err(Error::Protocol(
                "a live message outside a live session".into(),
            )),
        }
    }

    /// Whether the peer may open the live stream `name`: whether it may read
    /// that path (§4.2).
    fn may_open(&self, name: &str) -> bool {
        self.served
            .reads
            .as_ref()
            .is_none_or(|may| may(self.peer.as_ref(), name))
    }

    /// The next frame to send, or `None` until more requests arrive.
    pub fn transmit(&mut self) -> Result<Option<Vec<u8>>> {
        let Some(reply) = self.replies.pop_front() else {
            return Ok(None);
        };
        let msg = match reply {
            Reply::Hello => {
                let root = self.served.root();
                self.root = Some(root);
                let (low, high) = self.served.protocols;
                Msg::Hello(Greeting {
                    low,
                    high,
                    ..Greeting::new(root, self.served.format, self.challenge)
                })
            }
            Reply::Object(id) => self
                .object(&id)
                .map_or(Msg::Missing(id), |b| Msg::Object(id, b)),
            Reply::Children(id) => self
                .children(&id)
                .map_or(Msg::Missing(id), |b| Msg::Children(id, b)),
            Reply::Push(mut p) => {
                let store = self.served.store.clone();
                match p.next(&*store, self.grants()) {
                    Some((id, bytes)) => {
                        self.replies.push_front(Reply::Push(p));
                        Msg::Object(id, bytes)
                    }
                    None => Msg::End,
                }
            }
            Reply::End => Msg::End,
            Reply::Live(m) => m,
        };
        encode(&msg).map(Some)
    }

    /// An object, if this session may read it; granted, its readable
    /// children are granted too.
    fn object(&mut self, id: &Hash) -> Option<Vec<u8>> {
        if self.served.reads.is_none() {
            return self.served.object(id);
        }
        let s = self.served.clone();
        let g = self.grants()?;
        if !g.allows(&*s.store, id) {
            return None;
        }
        let bytes = s.object(id)?;
        g.served(id, &bytes);
        Some(bytes)
    }

    /// The chunks of a level-1 node, if this session may read the node.
    fn children(&mut self, id: &Hash) -> Option<Vec<u8>> {
        if self.served.reads.is_none() {
            return self.served.children(id);
        }
        let s = self.served.clone();
        let g = self.grants()?;
        if !g.allows(&*s.store, id) {
            return None;
        }
        let payload = s.children(id)?;
        let node = s.store.get(id).ok()?;
        g.served(id, &node);
        let n = Node::decode(&node).ok()?;
        let chunks = split_children(id, &n, &payload).ok()?;
        for (c, chunk) in n.children.iter().zip(chunks) {
            g.served(&c.id, chunk);
        }
        Some(payload)
    }

    /// The session's grants, made at its first request, once the peer is
    /// known; `None` without read rights, or before Hello.
    fn grants(&mut self) -> Option<&mut Grants> {
        let may = self.served.reads.clone()?;
        let root = self.root?;
        let (profile, peer) = (self.served.profile.clone(), self.peer);
        Some(
            self.grants
                .get_or_insert_with(|| Grants::new(profile, may, peer, root)),
        )
    }

    /// Whether this is a live session (§4.2).
    pub fn is_live(&self) -> bool {
        self.kind == Some(Kind::Live)
    }

    /// Whether the client said Bye and every answer has been taken.
    pub fn is_done(&self) -> bool {
        self.phase == Phase::Done && self.replies.is_empty()
    }

    fn handle(&mut self, m: Msg) -> Result<Option<LiveEvent>> {
        if self.phase == Phase::Ready && is_live(&m) {
            return self.live(m);
        }
        if self.phase == Phase::Ready && !matches!(m, Msg::Bye) {
            settle(&mut self.kind, Kind::Pull)?;
        }
        match (&self.phase, m) {
            (Phase::Greeting, Msg::Hello { .. }) => {
                self.replies.push_back(Reply::Hello);
                self.phase = match self.challenge == [0; 32] {
                    true => Phase::Ready,
                    false => Phase::Proving,
                };
            }
            (Phase::Proving, Msg::Auth { key, sig }) => {
                self.admit(&key, &sig)?;
                self.phase = Phase::Ready;
            }
            (Phase::Proving, m) => {
                return Err(Error::Protocol(format!("expected Auth, got {}", brief(&m))));
            }
            (Phase::Ready, Msg::Get(ids)) => self.answer(ids, Reply::Object),
            (Phase::Ready, Msg::GetChildren(ids)) => self.answer(ids, Reply::Children),
            (Phase::Ready, Msg::Fetch { want, have, scope }) => {
                let s = &self.served;
                self.replies.push_back(match &s.profile {
                    Some(p) => {
                        let push = Push::new(p.clone(), s.scope.clone(), want, have, scope);
                        Reply::Push(Box::new(push))
                    }
                    None => Reply::End,
                });
            }
            (Phase::Ready, Msg::Bye) => self.phase = Phase::Done,
            (Phase::Greeting, m) => {
                return Err(Error::Protocol(format!(
                    "expected Hello, got {}",
                    brief(&m)
                )));
            }
            (_, m) => return Err(Error::Protocol(format!("unexpected {}", brief(&m)))),
        }
        Ok(None)
    }

    /// A live message; an open the read rights refuse is closed at once.
    fn live(&mut self, m: Msg) -> Result<Option<LiveEvent>> {
        settle(&mut self.kind, Kind::Live)?;
        if let Msg::LiveOpen { name, .. } = &m
            && !self.streams.knows(name)
            && !self.may_open(name)
        {
            let refusal = Msg::LiveClose(name.clone());
            self.replies.push_back(Reply::Live(refusal));
            return Ok(None);
        }
        let (event, reply) = self.streams.received(m)?;
        self.replies.extend(reply.map(Reply::Live));
        Ok(event)
    }

    fn admit(&mut self, key: &PeerKey, sig: &[u8; 64]) -> Result<()> {
        let Some(a) = &self.served.admission else {
            return Err(Error::Protocol("an Auth without a challenge".into()));
        };
        if !a.names.iter().any(|n| verify(key, n, &self.challenge, sig)) {
            return Err(Error::Invalid("a wrong answer to the challenge".into()));
        }
        if !(a.admits)(key) {
            return Err(Error::Invalid("a key this server does not admit".into()));
        }
        self.peer = Some(*key);
        Ok(())
    }

    fn answer(&mut self, ids: Vec<Hash>, each: fn(Hash) -> Reply) {
        self.replies.extend(ids.into_iter().map(each));
        self.replies.push_back(Reply::End);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Secret;
    use crate::format::FORMAT;
    use crate::protocol::PROTOCOL as VERSION;
    use crate::protocol::{Client, Event, sample_served};
    use std::sync::Arc;

    fn frames(msgs: &[Msg]) -> Vec<u8> {
        msgs.iter().flat_map(|m| encode(m).unwrap()).collect()
    }

    fn drain(r: &mut Responder) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| r.transmit().unwrap()).collect()
    }

    #[test]
    fn answers_frame_by_frame_in_order() {
        let [a, b] = [b"one".to_vec(), b"two".to_vec()];
        let ida = crate::hash::h(&a);
        let missing = Hash([9; 32]);
        let mut r = Responder::new(
            sample_served(&[a.clone(), b], Hash([4; 32]), VERSION),
            [1; 32],
        );
        assert!(r.transmit().unwrap().is_none(), "nothing before Hello");
        let hello = Msg::Hello(Greeting::new(Hash::ZERO, FORMAT, [0; 32]));
        r.receive(&frames(&[hello, Msg::Get(vec![ida, missing])]))
            .unwrap();
        let sent = drain(&mut r);
        let want = [
            Msg::Hello(Greeting::new(Hash([4; 32]), FORMAT, [0; 32])),
            Msg::Object(ida, a),
            Msg::Missing(missing),
            Msg::End,
        ];
        assert_eq!(
            sent,
            want.iter().map(|m| encode(m).unwrap()).collect::<Vec<_>>()
        );
        assert!(!r.is_done());
        r.receive(&frames(&[Msg::Bye])).unwrap();
        assert!(r.is_done());
        assert!(matches!(
            r.receive(&frames(&[Msg::End])),
            Err(Error::Protocol(_))
        ));
    }

    fn challenging(admitted: PeerKey) -> Responder {
        let admission = super::super::Admission {
            names: vec!["a.example".into(), "edge.example".into()],
            admits: Box::new(move |k| *k == admitted),
        };
        let served = Served {
            admission: Some(std::sync::Arc::new(admission)),
            ..sample_served(&[b"x".to_vec()], Hash([3; 32]), VERSION)
        };
        Responder::new(served, [8; 32])
    }

    #[test]
    fn a_challenged_client_must_prove_an_admitted_key() {
        let secret = std::sync::Arc::new(Secret::from_bytes(&[5; 32]).unwrap());
        let other = std::sync::Arc::new(Secret::from_bytes(&[6; 32]).unwrap());
        let x = crate::hash::h(b"x");
        let talk = |r: &mut Responder, c: &mut Client| -> Result<Vec<Event>> {
            r.receive(&c.transmit())?;
            c.receive(&drain(r).concat())
        };
        let mut r = challenging(secret.public());
        let mut c = Client::with_key(secret.clone(), "edge.example");
        c.hello(&Hash::ZERO).unwrap();
        talk(&mut r, &mut c).unwrap();
        c.request(&[x], false).unwrap();
        let e = talk(&mut r, &mut c).unwrap();
        assert!(
            matches!(&e[..], [Event::Answer(a)] if a[0].is_ok()),
            "{e:?}"
        );
        assert_eq!(r.peer(), Some(&secret.public()));

        let cases: [(&str, Arc<Secret>, &str); 2] = [
            ("a key not admitted", other, "edge.example"),
            ("another server's name", secret.clone(), "elsewhere.example"),
        ];
        for (what, key, name) in cases {
            let mut r = challenging(secret.public());
            let mut c = Client::with_key(key, name);
            c.hello(&Hash::ZERO).unwrap();
            let e = talk(&mut r, &mut c);
            assert!(e.is_ok(), "{what}: Hello");
            c.request(&[x], false).unwrap();
            let r = r.receive(&c.transmit());
            assert!(matches!(r, Err(Error::Invalid(_))), "{what}: {r:?}");
        }

        let mut r = challenging(secret.public());
        let mut c = Client::new();
        c.hello(&Hash::ZERO).unwrap();
        r.receive(&c.transmit()).unwrap();
        let get = frames(&[Msg::Get(vec![x])]);
        assert!(
            matches!(r.receive(&get), Err(Error::Protocol(_))),
            "no Auth"
        );
        let mut r = Responder::new(sample_served(&[], Hash::ZERO, VERSION), [8; 32]);
        let hello = frames(&[Msg::Hello(Greeting::new(Hash::ZERO, FORMAT, [0; 32]))]);
        r.receive(&hello).unwrap();
        let auth = frames(&[Msg::Auth {
            key: [1; 32],
            sig: [2; 64],
        }]);
        assert!(
            matches!(r.receive(&auth), Err(Error::Protocol(_))),
            "unasked"
        );
    }

    #[test]
    fn read_rights_see_the_proved_key() {
        use crate::conformance::Plain;
        use crate::iface::Want;
        let x = b"below".to_vec();
        let root = Plain::object(&[(Want::Object, crate::hash::h(&x))]);
        let reader = Arc::new(Secret::from_bytes(&[5; 32]).unwrap());
        let other = Arc::new(Secret::from_bytes(&[6; 32]).unwrap());
        let key = reader.public();
        let may = move |peer: Option<&PeerKey>, path: &str| path == "0" && peer == Some(&key);
        let admission = super::super::Admission {
            names: vec!["edge.example".into()],
            admits: Box::new(|_| true),
        };
        let objects = [root.clone(), x.clone()];
        let served = Served {
            admission: Some(Arc::new(admission)),
            profile: Some(Arc::new(Plain)),
            reads: Some(Arc::new(may)),
            ..sample_served(&objects, crate::hash::h(&root), VERSION)
        };
        for (secret, readable) in [(reader, true), (other, false)] {
            let mut r = Responder::new(served.clone(), [8; 32]);
            let mut c = Client::with_key(secret, "edge.example");
            c.hello(&Hash::ZERO).unwrap();
            r.receive(&c.transmit()).unwrap();
            c.receive(&drain(&mut r).concat()).unwrap();
            c.request(&[crate::hash::h(&root)], false).unwrap();
            c.request(&[crate::hash::h(&x)], false).unwrap();
            r.receive(&c.transmit()).unwrap();
            let e = c.receive(&drain(&mut r).concat()).unwrap();
            let [Event::Answer(top), Event::Answer(below)] = &e[..] else {
                panic!("two answers: {e:?}");
            };
            assert!(top[0].is_ok(), "the root is always readable");
            assert_eq!(below[0].is_ok(), readable);
        }
    }

    #[test]
    fn hello_comes_first() {
        let mut r = Responder::new(sample_served(&[], Hash::ZERO, VERSION), [1; 32]);
        let get = frames(&[Msg::Get(vec![Hash::ZERO])]);
        assert!(matches!(r.receive(&get), Err(Error::Protocol(_))));
    }

    #[test]
    fn a_client_and_a_responder_talk_through_bytes() {
        let objects = [b"x".to_vec(), b"y".to_vec()];
        let ids: Vec<Hash> = objects.iter().map(|o| crate::hash::h(o)).collect();
        let mut r = Responder::new(sample_served(&objects, Hash([2; 32]), VERSION), [1; 32]);
        let mut c = Client::new();
        let pass = |c: &mut Client, r: &mut Responder| {
            r.receive(&c.transmit()).unwrap();
            let back: Vec<u8> = drain(r).concat();
            c.receive(&back).unwrap()
        };
        c.hello(&Hash::ZERO).unwrap();
        let e = pass(&mut c, &mut r);
        assert!(matches!(e[..], [Event::Hello(root)] if root == Hash([2; 32])));
        c.request(&ids, false).unwrap();
        c.request(&ids[..1], false).unwrap();
        let e = pass(&mut c, &mut r);
        assert_eq!(e.len(), 2, "both pipelined answers");
        let Event::Answer(got) = &e[0] else {
            panic!("expected an answer");
        };
        assert_eq!(got[1].as_ref().ok(), Some(&objects[1]));
        c.bye().unwrap();
        pass(&mut c, &mut r);
        assert!(r.is_done());
    }
}
