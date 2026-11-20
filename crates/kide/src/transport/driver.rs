//! The blocking drivers: a `Session` that runs the client state machine over
//! any byte stream, and the loop that runs a responder over one.

use super::live::Close;
use super::net_err;
use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::iface::{LiveEvents, Session};
use crate::protocol::{Client, Event, Responder};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::time::Duration;

const READ: usize = 64 << 10;

pub(super) struct Blocking<S> {
    stream: S,
    client: Client,
    events: VecDeque<Event>,
    buf: Vec<u8>,
}

impl<S: Read + Write> Blocking<S> {
    pub(super) fn new(stream: S) -> Blocking<S> {
        Blocking::with(stream, Client::new())
    }

    /// A session of `client` over `stream`.
    #[cfg_attr(not(feature = "quic"), allow(dead_code))]
    pub(super) fn with(stream: S, client: Client) -> Blocking<S> {
        Blocking {
            stream,
            client,
            events: VecDeque::new(),
            buf: vec![0; READ],
        }
    }

    /// Sends what the client queued.
    fn flush(&mut self) -> Result<()> {
        let out = self.client.transmit();
        if out.is_empty() {
            return Ok(());
        }
        self.stream.write_all(&out).map_err(net_err)?;
        self.stream.flush().map_err(net_err)
    }

    /// Reads until the client has an event; a read that fails or ends is a
    /// break.
    fn event(&mut self) -> Result<Event> {
        loop {
            if let Some(e) = self.events.pop_front() {
                return Ok(e);
            }
            let n = match self.stream.read(&mut self.buf) {
                Ok(0) => return self.client.broken("connection closed"),
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return self.client.broken(&format!("connection: {e}")),
            };
            match self.client.receive(&self.buf[..n]) {
                Ok(events) => self.events.extend(events),
                Err(e) => {
                    let _ = self.flush(); // a Bye, if the client queued one
                    return Err(e);
                }
            }
        }
    }

    fn exchange(&mut self, ids: &[Hash], children: bool) -> Result<Vec<Result<Vec<u8>>>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.client.request(ids, children)?;
        self.flush()?;
        match self.event()? {
            Event::Answer(a) => Ok(a),
            e => Err(unexpected(&e)),
        }
    }
}

impl<S: Read + Write + Send> Session for Blocking<S> {
    fn hello(&mut self, root: &Hash) -> Result<Hash> {
        self.client.hello(root)?;
        self.flush()?;
        match self.event()? {
            Event::Hello(root) => Ok(root),
            e => Err(unexpected(&e)),
        }
    }

    fn get(&mut self, ids: &[Hash]) -> Result<Vec<Result<Vec<u8>>>> {
        self.exchange(ids, false)
    }

    fn get_children(&mut self, nodes: &[Hash]) -> Result<Vec<Result<Vec<u8>>>> {
        self.exchange(nodes, true)
    }

    fn fetch(&mut self, want: &Hash, have: &[Hash], scope: &[u8]) -> Result<Vec<(Hash, Vec<u8>)>> {
        self.client.fetch(want, have, scope)?;
        self.flush()?;
        match self.event()? {
            Event::Pushed(p) => Ok(p),
            e => Err(unexpected(&e)),
        }
    }

    fn bye(&mut self) -> Result<()> {
        self.client.bye()?;
        self.flush()
    }
}

/// An event the session did not wait for.
fn unexpected(e: &Event) -> Error {
    let what = match e {
        Event::Hello(_) => "a second Hello",
        Event::Answer(_) => "an answer",
        Event::Pushed(_) => "pushed objects",
        Event::Live(_) => "a live message",
    };
    Error::Protocol(format!("{what} out of turn"))
}

/// Serves one connection until Bye or until the stream ends or fails, which
/// is no error; a protocol violation is. A session that turns live goes to
/// the live driver, whose events go to `live`; without it, live streams are
/// refused. `close` breaks the connection.
pub(super) fn serve(
    mut reader: impl Read,
    mut writer: impl Write + Send + 'static,
    mut r: Responder,
    live: Option<LiveEvents>,
    close: Close,
    ping: Duration,
) -> Result<()> {
    let mut buf = vec![0; READ];
    loop {
        while let Some(frame) = r.transmit()? {
            writer.write_all(&frame).map_err(net_err)?;
        }
        writer.flush().map_err(net_err)?;
        if r.is_done() {
            return Ok(());
        }
        let events = match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => r.receive(&buf[..n])?,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Ok(()),
        };
        match &live {
            Some(app) if r.is_live() => {
                super::live::serve(r, events, reader, writer, close, app, ping);
                return Ok(());
            }
            _ => super::refuse_live(&mut r, events)?,
        }
    }
}
