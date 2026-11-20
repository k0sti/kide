//! Frames and messages (§4.2): `length: u32 LE | type: u8 | payload`, where
//! `length` counts the type byte and the payload, at most `MAX_FRAME`.

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::iface::{MAX_FRAME, MAX_GET};

/// What a Hello says (§4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Greeting {
    /// The lowest and highest protocol versions the sender speaks.
    pub low: u8,
    pub high: u8,
    /// The optional features the sender supports; a session uses those
    /// both set, and ignores bits it does not know.
    pub features: u64,
    /// The folder format of the sender's root (§5.4); 0 in the Hello of
    /// protocol 7 and earlier, which did not say.
    pub format: u16,
    pub root: Hash,
    /// Zero unless the sender wants the peer's key.
    pub challenge: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    Hello(Greeting),
    Get(Vec<Hash>),
    Object(Hash, Vec<u8>),
    Missing(Hash),
    End,
    Bye,
    GetChildren(Vec<Hash>),
    Children(Hash, Vec<u8>),
    /// A key and its BIP-340 signature answering the peer's challenge.
    Auth {
        key: [u8; 32],
        sig: [u8; 64],
    },
    /// Asks the server to push what `want` reaches and the `have`s do not,
    /// within `scope`, an application's bytes.
    Fetch {
        want: Hash,
        have: Vec<Hash>,
        scope: Vec<u8>,
    },
    /// Opens the live stream `name` at `version`, the application's bytes;
    /// from the server, accepts it.
    LiveOpen {
        name: String,
        version: Vec<u8>,
    },
    /// An update on an open live stream, the application's bytes.
    LiveUpdate {
        name: String,
        bytes: Vec<u8>,
    },
    /// Closes a live stream; from the server, also refuses an open.
    LiveClose(String),
    /// Keeps an idle live session open.
    Ping,
}

fn protocol<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Protocol(msg.into()))
}

/// One frame. An empty or oversized request, or a frame above `MAX_FRAME`,
/// is `Invalid`.
pub fn encode(msg: &Msg) -> Result<Vec<u8>> {
    let (t, head, tail): (u8, &[u8], &[u8]) = match msg {
        Msg::Hello(g) => {
            let hello = [
                &[g.high, g.low][..],
                &g.features.to_le_bytes(),
                &g.format.to_le_bytes(),
                &g.root.0,
                &g.challenge,
            ]
            .concat();
            return frame(0x01, &hello, &[]);
        }
        Msg::Auth { key, sig } => (0x09, key, sig),
        Msg::Get(ids) => return frame(0x02, &id_list(ids)?, &[]),
        Msg::GetChildren(ids) => return frame(0x07, &id_list(ids)?, &[]),
        Msg::Object(id, bytes) => (0x03, &id.0, bytes),
        Msg::Children(id, bytes) => (0x08, &id.0, bytes),
        Msg::Missing(id) => (0x04, &id.0, &[]),
        Msg::End => (0x05, &[], &[]),
        Msg::Bye => (0x06, &[], &[]),
        Msg::Fetch { want, have, scope } => {
            if have.len() > MAX_GET {
                return Err(Error::Invalid(format!("a Fetch of {} haves", have.len())));
            }
            let mut head = want.0.to_vec();
            head.extend((have.len() as u32).to_le_bytes());
            head.extend(have.iter().flat_map(|h| h.0));
            return frame(0x0a, &head, scope);
        }
        Msg::LiveOpen { name, version } => return frame(0x10, &named(name)?, version),
        Msg::LiveUpdate { name, bytes } => return frame(0x11, &named(name)?, bytes),
        Msg::LiveClose(name) => (0x12, name.as_bytes(), &[]),
        Msg::Ping => (0x13, &[], &[]),
    };
    frame(t, head, tail)
}

fn frame(t: u8, head: &[u8], tail: &[u8]) -> Result<Vec<u8>> {
    let len = 1 + head.len() + tail.len();
    if len > MAX_FRAME {
        return Err(Error::Invalid(format!("a frame of {len} bytes")));
    }
    let mut out = Vec::with_capacity(4 + len);
    out.extend((len as u32).to_le_bytes());
    out.push(t);
    out.extend(head);
    out.extend(tail);
    Ok(out)
}

/// The payload of a Get or GetChildren: 1 to 4,096 ids.
fn id_list(ids: &[Hash]) -> Result<Vec<u8>> {
    if ids.is_empty() || ids.len() > MAX_GET {
        return Err(Error::Invalid(format!("a request of {} ids", ids.len())));
    }
    Ok(ids.iter().flat_map(|id| id.0).collect())
}

/// A live stream's name with its length in front: `name_len: u16 LE | name`.
fn named(name: &str) -> Result<Vec<u8>> {
    let Ok(len) = u16::try_from(name.len()) else {
        return Err(Error::Invalid(format!("a name of {} bytes", name.len())));
    };
    Ok([&len.to_le_bytes()[..], name.as_bytes()].concat())
}

/// Splits received bytes into messages, however they were cut in transit.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// Bytes of `buf` already decoded.
    at: usize,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        if self.at > 0 && self.at * 2 >= self.buf.len() {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole message, or `None` until one has arrived. A frame of a
    /// bad length, type or payload is `Protocol`.
    pub fn next_msg(&mut self) -> Result<Option<Msg>> {
        let rest = &self.buf[self.at..];
        let Some(len) = rest
            .first_chunk::<4>()
            .map(|l| u32::from_le_bytes(*l) as usize)
        else {
            return Ok(None);
        };
        if len == 0 || len > MAX_FRAME {
            return protocol(format!("frame length {len}"));
        }
        let Some(body) = rest.get(4..4 + len) else {
            return Ok(None);
        };
        let msg = parse(body[0], &body[1..])?;
        self.at += 4 + len;
        Ok(Some(msg))
    }

    /// Whether no part of a frame is waiting: a stream that ends otherwise
    /// ended inside a frame.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.at == self.buf.len()
    }
}

fn parse(t: u8, p: &[u8]) -> Result<Msg> {
    let ids = || p.as_chunks::<32>().0.iter().map(|&b| Hash(b)).collect();
    let request = p.len().is_multiple_of(32) && (1..=MAX_GET).contains(&(p.len() / 32));
    match (t, p.len()) {
        (0x01, 76) => Ok(Msg::Hello(Greeting {
            high: p[0],
            low: p[1],
            // The arm matched 76 bytes, so 2..10 is 8 of them.
            features: u64::from_le_bytes(p[2..10].try_into().expect("8 bytes")),
            format: u16::from_le_bytes([p[10], p[11]]),
            root: Hash::at(&p[12..]),
            challenge: Hash::at(&p[44..]).0,
        })),
        // The Hello of protocol 7 and earlier: one version, no format.
        (0x01, 65) => Ok(Msg::Hello(Greeting {
            high: p[0],
            low: p[0],
            features: 0,
            format: 0,
            root: Hash::at(&p[1..]),
            challenge: Hash::at(&p[33..]).0,
        })),
        (0x09, 96) => Ok(Msg::Auth {
            key: Hash::at(p).0,
            // The arm matched a payload of 96 bytes, so 64 follow the key.
            sig: p[32..].try_into().expect("96 - 32 = 64 bytes"),
        }),
        (0x02, _) if request => Ok(Msg::Get(ids())),
        (0x07, _) if request => Ok(Msg::GetChildren(ids())),
        (0x03, n) if n >= 32 => Ok(Msg::Object(Hash::at(p), p[32..].to_vec())),
        (0x08, n) if n >= 32 => Ok(Msg::Children(Hash::at(p), p[32..].to_vec())),
        (0x04, 32) => Ok(Msg::Missing(Hash::at(p))),
        (0x05, 0) => Ok(Msg::End),
        (0x06, 0) => Ok(Msg::Bye),
        (0x0a, n) if n >= 36 => fetch(p),
        (0x10, _) => {
            let (name, version) = split_name(p)?;
            Ok(Msg::LiveOpen { name, version })
        }
        (0x11, _) => {
            let (name, bytes) = split_name(p)?;
            Ok(Msg::LiveUpdate { name, bytes })
        }
        (0x12, _) => Ok(Msg::LiveClose(text(p)?)),
        (0x13, 0) => Ok(Msg::Ping),
        (t, n) => protocol(format!("message type {t:#04x} with {n} payload bytes")),
    }
}

/// A Fetch: `want`, a count of at most 4,096, that many ids, then the scope.
fn fetch(p: &[u8]) -> Result<Msg> {
    let (want, rest) = p.split_at(32);
    let (count, rest) = rest.split_at(4);
    // `count` is 4 bytes: the arm matched at least 36.
    let n = u32::from_le_bytes(count.try_into().expect("4 bytes")) as usize;
    if n > MAX_GET || rest.len() < 32 * n {
        return protocol(format!("a Fetch of {n} haves in {} bytes", rest.len()));
    }
    let (have, scope) = rest.split_at(32 * n);
    Ok(Msg::Fetch {
        want: Hash::at(want),
        have: have.as_chunks::<32>().0.iter().map(|&b| Hash(b)).collect(),
        scope: scope.to_vec(),
    })
}

/// A live stream's name, then the rest of the payload.
fn split_name(p: &[u8]) -> Result<(String, Vec<u8>)> {
    let Some((len, rest)) = p.split_first_chunk::<2>() else {
        return protocol("a live message without a name length");
    };
    let len = u16::from_le_bytes(*len) as usize;
    if rest.len() < len {
        return protocol(format!("a name of {len} bytes in {}", rest.len()));
    }
    let (name, tail) = rest.split_at(len);
    Ok((text(name)?, tail.to_vec()))
}

fn text(name: &[u8]) -> Result<String> {
    match std::str::from_utf8(name) {
        Ok(s) => Ok(s.to_string()),
        Err(_) => protocol("a name that is not UTF-8"),
    }
}

/// A message without its payload, for error text.
pub fn brief(m: &Msg) -> String {
    match m {
        Msg::Object(id, b) => format!("Object({id}, {} bytes)", b.len()),
        Msg::Children(id, b) => format!("Children({id}, {} bytes)", b.len()),
        Msg::Get(ids) => format!("Get({} ids)", ids.len()),
        Msg::GetChildren(ids) => format!("GetChildren({} ids)", ids.len()),
        Msg::Fetch { want, have, scope } => {
            format!(
                "Fetch({want}, {} haves, {} scope bytes)",
                have.len(),
                scope.len()
            )
        }
        Msg::LiveOpen { name, version } => {
            format!("LiveOpen({name:?}, {} version bytes)", version.len())
        }
        Msg::LiveUpdate { name, bytes } => format!("LiveUpdate({name:?}, {} bytes)", bytes.len()),
        m => format!("{m:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(bytes: &[u8]) -> Result<Vec<Msg>> {
        let mut d = Decoder::default();
        d.push(bytes);
        let mut out = Vec::new();
        while let Some(m) = d.next_msg()? {
            out.push(m);
        }
        assert!(d.is_empty(), "the whole input is consumed");
        Ok(out)
    }

    fn frame(t: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = ((payload.len() + 1) as u32).to_le_bytes().to_vec();
        f.push(t);
        f.extend(payload);
        f
    }

    #[test]
    fn every_message_round_trips() {
        let id = Hash([3; 32]);
        let msgs = [
            Msg::Hello(Greeting {
                low: 2,
                high: 9,
                features: 0x0102_0304_0506_0708,
                format: 0x0a0b,
                root: id,
                challenge: [6; 32],
            }),
            Msg::Auth {
                key: [1; 32],
                sig: [2; 64],
            },
            Msg::Get(vec![id]),
            Msg::Get((0..MAX_GET as u32).map(|i| Hash([i as u8; 32])).collect()),
            Msg::Object(id, vec![]),
            Msg::Object(id, vec![7; MAX_FRAME - 33]),
            Msg::Missing(id),
            Msg::End,
            Msg::Bye,
            Msg::GetChildren(vec![id, Hash([4; 32])]),
            Msg::Children(id, vec![8; 5000]),
            Msg::Fetch {
                want: id,
                have: vec![],
                scope: vec![],
            },
            Msg::Fetch {
                want: id,
                have: vec![Hash([4; 32]); MAX_GET],
                scope: b"area 7".to_vec(),
            },
            Msg::LiveOpen {
                name: "region/3".into(),
                version: vec![1, 2],
            },
            Msg::LiveOpen {
                name: String::new(),
                version: vec![],
            },
            Msg::LiveUpdate {
                name: "région".into(),
                bytes: vec![7; MAX_FRAME - 1 - 2 - 7],
            },
            Msg::LiveClose("region/3".into()),
            Msg::LiveClose(String::new()),
            Msg::Ping,
        ];
        let all: Vec<u8> = msgs.iter().flat_map(|m| encode(m).unwrap()).collect();
        assert_eq!(decode_all(&all).unwrap(), msgs);
    }

    #[test]
    fn frames_cut_anywhere_decode_the_same() {
        let msgs = [
            Msg::Get(vec![Hash([1; 32]), Hash([2; 32])]),
            Msg::Object(Hash([5; 32]), vec![9; 300]),
            Msg::End,
        ];
        let all: Vec<u8> = msgs.iter().flat_map(|m| encode(m).unwrap()).collect();
        for cut in [1, 3, 7, 64] {
            let (mut d, mut got) = (Decoder::default(), Vec::new());
            for piece in all.chunks(cut) {
                d.push(piece);
                while let Some(m) = d.next_msg().unwrap() {
                    got.push(m);
                }
            }
            assert_eq!(got, msgs, "pieces of {cut} bytes");
            assert!(d.is_empty());
        }
        let mut d = Decoder::default();
        d.push(&all[..10]);
        assert_eq!(d.next_msg().unwrap(), None, "a partial frame waits");
        assert!(!d.is_empty(), "and is reported as unfinished");
    }

    #[test]
    fn frame_layout() {
        assert_eq!(
            encode(&Msg::Missing(Hash([9; 32]))).unwrap(),
            frame(0x04, &[9; 32])
        );
        assert_eq!(encode(&Msg::Bye).unwrap(), [1, 0, 0, 0, 0x06]);
        let hello = Msg::Hello(Greeting {
            low: 2,
            high: 3,
            features: 1,
            format: 0x0107,
            root: Hash([5; 32]),
            challenge: [6; 32],
        });
        let payload = [
            &[3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 7, 1][..],
            &[5; 32],
            &[6; 32],
        ]
        .concat();
        assert_eq!(encode(&hello).unwrap(), frame(0x01, &payload));
        let mut d = Decoder::default();
        d.push(&frame(0x01, &[&[7][..], &[5; 32], &[6; 32]].concat()));
        let old = Msg::Hello(Greeting {
            low: 7,
            high: 7,
            features: 0,
            format: 0,
            root: Hash([5; 32]),
            challenge: [6; 32],
        });
        assert_eq!(d.next_msg().unwrap(), Some(old), "the Hello of protocol 7");
        let auth = Msg::Auth {
            key: [1; 32],
            sig: [2; 64],
        };
        assert_eq!(
            encode(&auth).unwrap(),
            frame(0x09, &[&[1; 32][..], &[2; 64]].concat())
        );
        let get = Msg::Get(vec![Hash([1; 32]), Hash([2; 32])]);
        assert_eq!(
            encode(&get).unwrap(),
            frame(0x02, &[[1; 32], [2; 32]].concat())
        );
        let fetch = Msg::Fetch {
            want: Hash([1; 32]),
            have: vec![Hash([2; 32])],
            scope: vec![3, 4],
        };
        let payload = [&[1; 32][..], &[1, 0, 0, 0], &[2; 32], &[3, 4]].concat();
        assert_eq!(encode(&fetch).unwrap(), frame(0x0a, &payload));
        let open = Msg::LiveOpen {
            name: "ab".into(),
            version: vec![9],
        };
        assert_eq!(encode(&open).unwrap(), frame(0x10, b"\x02\x00ab\x09"));
        let update = Msg::LiveUpdate {
            name: "ab".into(),
            bytes: vec![8, 7],
        };
        assert_eq!(encode(&update).unwrap(), frame(0x11, b"\x02\x00ab\x08\x07"));
        let close = Msg::LiveClose("ab".into());
        assert_eq!(encode(&close).unwrap(), frame(0x12, b"ab"));
        assert_eq!(encode(&Msg::Ping).unwrap(), [1, 0, 0, 0, 0x13]);
    }

    #[test]
    fn malformed_frames_are_protocol_errors() {
        let ids = |n: usize| vec![0; 32 * n];
        let fetch =
            |n: u32, ids: usize| [&[0; 32][..], &n.to_le_bytes(), &vec![0; 32 * ids]].concat();
        let bad: [(&str, Vec<u8>); 26] = [
            ("zero length", vec![0, 0, 0, 0]),
            ("oversized", ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec()),
            ("huge", [&u32::MAX.to_le_bytes()[..], &[0x03]].concat()),
            ("unknown type", frame(0x0b, &[])),
            ("type after Ping", frame(0x14, &[])),
            ("LiveOpen without a name length", frame(0x10, &[1])),
            (
                "LiveUpdate short of its name",
                frame(0x11, &[3, 0, b'a', b'b']),
            ),
            ("a name not UTF-8", frame(0x10, &[1, 0, 0xff])),
            ("LiveClose not UTF-8", frame(0x12, &[0xc3])),
            ("Ping with payload", frame(0x13, &[0])),
            ("short Fetch", frame(0x0a, &[0; 35])),
            ("Fetch short of its haves", frame(0x0a, &fetch(2, 1))),
            ("Fetch of 4,097 haves", frame(0x0a, &fetch(4097, 4097))),
            ("type 0", frame(0x00, &[])),
            ("short Missing", frame(0x04, &[1; 31])),
            ("End with payload", frame(0x05, &[0])),
            ("Bye with payload", frame(0x06, &[0])),
            ("short Hello", frame(0x01, &[1; 64])),
            ("long Hello", frame(0x01, &[1; 77])),
            ("Hello between the two forms", frame(0x01, &[1; 66])),
            ("Hello without a challenge", frame(0x01, &[1; 33])),
            ("short Auth", frame(0x09, &[1; 95])),
            ("Object without a whole id", frame(0x03, &[1; 31])),
            ("empty Get", frame(0x02, &[])),
            ("Get of a partial id", frame(0x07, &[0; 33])),
            ("Get of 4,097 ids", frame(0x02, &ids(MAX_GET + 1))),
        ];
        for (what, bytes) in bad {
            let mut d = Decoder::default();
            d.push(&bytes);
            let r = d.next_msg();
            assert!(matches!(r, Err(Error::Protocol(_))), "{what}: {r:?}");
        }
    }

    #[test]
    fn encoding_a_bad_request_or_frame_is_invalid() {
        for m in [
            Msg::Get(vec![]),
            Msg::GetChildren(vec![Hash::ZERO; MAX_GET + 1]),
            Msg::Object(Hash::ZERO, vec![0; MAX_FRAME - 32]),
            Msg::Fetch {
                want: Hash::ZERO,
                have: vec![Hash::ZERO; MAX_GET + 1],
                scope: vec![],
            },
            Msg::LiveOpen {
                name: "x".repeat(u16::MAX as usize + 1),
                version: vec![],
            },
            Msg::LiveUpdate {
                name: "x".into(),
                bytes: vec![0; MAX_FRAME - 3],
            },
        ] {
            assert!(
                matches!(encode(&m), Err(Error::Invalid(_))),
                "{}",
                brief(&m)
            );
        }
    }
}
