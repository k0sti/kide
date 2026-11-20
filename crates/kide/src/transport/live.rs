//! The blocking live driver (§4.2): one end of a live session over a duplex
//! byte stream, with two threads. The reader turns received bytes into
//! events for the application; the writer sends what calls queued, or Ping
//! after `PING` without sending. Calls never wait, and a queue over
//! `LIVE_QUEUE` bytes ends the session. An async driver, such as the
//! browser's, uses the same `Session` through `client` or `server`, `next`
//! and `received`, with its own clock and waiting.

use super::net_err;
use crate::auth::PeerKey;
use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::iface::{LIVE_QUEUE, Live, LiveEvent, LiveEvents};
use crate::log;
use crate::protocol::{Client, Event, Responder};
use std::io::{ErrorKind, Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// The longest a live session's side stays silent (§4.2).
pub const PING: Duration = Duration::from_secs(10);

const READ: usize = 64 << 10;

/// Breaks a connection, so that its reader and writer stop waiting.
pub type Close = Box<dyn Fn() + Send + Sync>;

/// Wakes an async writer: called whenever the outbox fills or the session
/// ends.
pub type Wake = Box<dyn Fn() + Send + Sync>;

/// What a writer does next.
pub enum Next {
    /// Writes these bytes.
    Send(Vec<u8>),
    /// Waits this long, or until woken.
    Wait(Duration),
    /// Stops: the session has ended.
    Stop,
}

/// The state machine of either end.
enum End {
    Client(Client),
    Server(Box<Responder>),
}

impl End {
    fn receive(&mut self, bytes: &[u8]) -> Result<Vec<LiveEvent>> {
        let events = match self {
            End::Server(r) => return r.receive(bytes),
            End::Client(c) => c.receive(bytes)?,
        };
        let live = |e| match e {
            Event::Live(e) => Ok(e),
            e => Err(Error::Protocol(format!("{e:?} in a live session"))),
        };
        events.into_iter().map(live).collect()
    }

    /// Appends every frame the end has queued to `out`.
    fn transmit(&mut self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            End::Client(c) => out.extend(c.transmit()),
            End::Server(r) => {
                while let Some(f) = r.transmit()? {
                    out.extend(f);
                }
            }
        }
        Ok(())
    }

    fn open(&mut self, name: &str, version: &[u8]) -> Result<()> {
        match self {
            End::Client(c) => c.live_open(name, version),
            End::Server(r) => r.live_open(name, version),
        }
    }

    fn update(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        match self {
            End::Client(c) => c.live_update(name, bytes),
            End::Server(r) => r.live_update(name, bytes),
        }
    }

    fn close(&mut self, name: &str) -> Result<()> {
        match self {
            End::Client(c) => c.live_close(name),
            End::Server(r) => r.live_close(name),
        }
    }

    fn ping(&mut self) -> Result<()> {
        match self {
            End::Client(c) => c.ping(),
            End::Server(r) => r.ping(),
        }
    }

    /// Whether the peer said Bye.
    fn is_done(&self) -> bool {
        matches!(self, End::Server(r) if r.is_done())
    }
}

struct Inner {
    end: End,
    /// Frames queued and not yet taken by the writer.
    outbox: Vec<u8>,
    /// Why the session ended, once it has.
    ended: Option<Error>,
    /// Whether this side said Bye: the writer ends the session once the
    /// outbox is sent.
    bye: bool,
}

/// One end of a live session: its state machine, the outbox and why it
/// ended. Calls never wait.
pub struct Session {
    inner: Mutex<Inner>,
    /// Signalled when the outbox fills or the session ends.
    changed: Condvar,
    /// Wakes the writer as well, where it is async.
    wake: Option<Wake>,
    close: Close,
    peer: Option<PeerKey>,
}

impl Session {
    fn new(end: End, close: Close, wake: Option<Wake>, peer: Option<PeerKey>) -> Arc<Session> {
        Arc::new(Session {
            inner: Mutex::new(Inner {
                end,
                outbox: Vec::new(),
                ended: None,
                bye: false,
            }),
            changed: Condvar::new(),
            wake,
            close,
            peer,
        })
    }

    /// The client's end, once Hello (and Auth) are done: sends Ping, which
    /// makes the session live.
    pub fn client(mut client: Client, close: Close, wake: Option<Wake>) -> Result<Arc<Session>> {
        client.ping()?;
        let s = Session::new(End::Client(client), close, wake, None);
        s.queue(&mut s.lock())?;
        Ok(s)
    }

    /// The server's end of a session that has just turned live, with what
    /// the responder has queued so far; a failure to queue ends it.
    pub fn server(responder: Responder, close: Close, wake: Option<Wake>) -> Arc<Session> {
        let peer = responder.peer().copied();
        let s = Session::new(End::Server(Box::new(responder)), close, wake, peer);
        let _ = s.queue(&mut s.lock());
        s
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lock only means another thread panicked; the state is intact.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ends the session for `why`, once, and breaks the connection.
    fn end(&self, g: &mut Inner, why: Error) {
        if g.ended.is_none() {
            log!("live session: {why}");
            g.ended = Some(why);
            (self.close)();
            self.signal();
        }
    }

    fn signal(&self) {
        self.changed.notify_all();
        if let Some(wake) = &self.wake {
            wake();
        }
    }

    /// Ends the session for `why`, if it has not ended.
    pub fn fail(&self, why: Error) {
        self.end(&mut self.lock(), why);
    }

    /// Ends the session because the application no longer takes events.
    pub fn deaf(&self) {
        self.fail(Error::Unavailable("no one takes the events".into()));
    }

    pub fn has_ended(&self) -> bool {
        self.lock().ended.is_some()
    }

    /// Why the session ended.
    pub fn why(&self) -> Error {
        let why = self.lock().ended.clone();
        why.unwrap_or_else(|| Error::Unavailable("ended".into()))
    }

    /// Takes the outbox, after queueing a Ping once the writer has been
    /// `idle` (not sent) for `ping`; otherwise says how long to wait.
    pub fn next(&self, idle: Duration, ping: Duration) -> Next {
        let mut g = self.lock();
        loop {
            if !g.outbox.is_empty() {
                return Next::Send(std::mem::take(&mut g.outbox));
            }
            if g.ended.is_some() {
                return Next::Stop;
            }
            if g.bye {
                self.end(&mut g, Error::Unavailable("ended by this side".into()));
                return Next::Stop;
            }
            let wait = ping.saturating_sub(idle);
            if !wait.is_zero() {
                return Next::Wait(wait);
            }
            match g.end.ping() {
                Ok(()) => self.queue(&mut g).unwrap_or(()),
                Err(e) => self.end(&mut g, e),
            }
        }
    }

    /// Takes what the reader got, received bytes or why it could not read,
    /// and returns the events for the application; `None` once the session
    /// has ended.
    pub fn received(&self, got: Result<&[u8]>) -> Option<Vec<LiveEvent>> {
        let mut g = self.lock();
        if g.ended.is_some() {
            return None;
        }
        match got.and_then(|bytes| g.end.receive(bytes)) {
            Ok(events) => {
                if self.queue(&mut g).is_ok() && g.end.is_done() {
                    self.end(&mut g, Error::Unavailable("the peer said Bye".into()));
                }
                Some(events)
            }
            Err(e) => {
                self.end(&mut g, e);
                None
            }
        }
    }

    /// Moves what the end queued into the outbox; over `LIVE_QUEUE` bytes,
    /// or a failure to encode, ends the session.
    fn queue(&self, g: &mut Inner) -> Result<()> {
        let Inner { end, outbox, .. } = g;
        let r = end
            .transmit(outbox)
            .and_then(|()| match outbox.len() > LIVE_QUEUE {
                true => Err(Error::Unavailable(format!(
                    "more than {LIVE_QUEUE} bytes wait to be sent"
                ))),
                false => Ok(()),
            });
        if let Err(e) = &r {
            self.end(g, e.clone());
        }
        self.signal();
        r
    }

    /// Runs one call on the end and queues what it sent.
    fn call(&self, f: impl FnOnce(&mut End) -> Result<()>) -> Result<()> {
        let mut g = self.lock();
        if let Some(e) = &g.ended {
            return Err(e.clone());
        }
        f(&mut g.end)?;
        self.queue(&mut g)
    }
}

impl Live for Session {
    fn open(&self, name: &str, version: &[u8]) -> Result<()> {
        self.call(|e| e.open(name, version))
    }

    fn update(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.call(|e| e.update(name, bytes))
    }

    fn close(&self, name: &str) -> Result<()> {
        self.call(|e| e.close(name))
    }

    fn bye(&self) -> Result<()> {
        let mut g = self.lock();
        if g.ended.is_some() || g.bye {
            return Ok(());
        }
        if let End::Client(c) = &mut g.end {
            c.bye()?;
        }
        g.bye = true;
        self.queue(&mut g)
    }

    fn peer(&self) -> Option<PeerKey> {
        self.peer
    }

    fn queued(&self) -> usize {
        self.lock().outbox.len()
    }
}

/// Opens a live session as `client` over `reader` and `writer`: Hello, then
/// Ping, which makes it live; then the driver's threads take over.
pub(super) fn connect(
    mut reader: impl Read + Send + 'static,
    mut writer: impl Write + Send + 'static,
    mut client: Client,
    close: Close,
    events: LiveEvents,
    ping: Duration,
) -> Result<Arc<dyn Live>> {
    let hello = hello(&mut reader, &mut writer, &mut client);
    if hello.is_err() {
        close();
    }
    hello?;
    let s = Session::client(client, close, None)?;
    let w = s.clone();
    thread::spawn(move || write(&w, writer, ping));
    let r = s.clone();
    thread::spawn(move || read(&r, reader, &events, Vec::new()));
    Ok(s)
}

/// Exchanges Hello, and sends Auth if challenged.
fn hello(reader: &mut impl Read, writer: &mut impl Write, client: &mut Client) -> Result<()> {
    let mut send = |c: &mut Client| -> Result<()> {
        writer.write_all(&c.transmit()).map_err(net_err)?;
        writer.flush().map_err(net_err)
    };
    client.hello(&Hash::ZERO)?;
    send(client)?;
    let mut buf = vec![0; READ];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return Err(Error::Unavailable("connection closed".into())),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(net_err(e)),
        };
        match client.receive(&buf[..n]) {
            Ok(events) if events.is_empty() => {}
            Ok(_) => return send(client), // Hello, the only answer so far
            Err(e) => {
                let _ = send(client); // a Bye, if the client queued one
                return Err(e);
            }
        }
    }
}

/// Serves a session that has just turned live, `first` its events so far:
/// the writer on a new thread, the reader on this one, until it ends.
pub(super) fn serve(
    responder: Responder,
    first: Vec<LiveEvent>,
    reader: impl Read,
    writer: impl Write + Send + 'static,
    close: Close,
    events: &LiveEvents,
    ping: Duration,
) {
    let s = Session::server(responder, close, None);
    let w = s.clone();
    thread::spawn(move || write(&w, writer, ping));
    read(&s, reader, events, first);
}

/// Sends the outbox as it fills, and Ping after `ping` without sending,
/// until the session ends.
fn write(s: &Session, mut writer: impl Write, ping: Duration) {
    let mut sent = Instant::now();
    loop {
        match s.next(sent.elapsed(), ping) {
            Next::Stop => return,
            Next::Wait(wait) => {
                let g = s.lock();
                if g.outbox.is_empty() && g.ended.is_none() && !g.bye {
                    let _ = s.changed.wait_timeout(g, wait);
                }
            }
            Next::Send(out) => {
                if let Err(e) = writer.write_all(&out).and_then(|()| writer.flush()) {
                    s.fail(net_err(e));
                    return;
                }
                sent = Instant::now();
            }
        }
    }
}

/// Reads until the session ends, handing `first` and then every received
/// event to the application, and `Ended` last.
fn read(s: &Arc<Session>, mut reader: impl Read, events: &LiveEvents, first: Vec<LiveEvent>) {
    let me: Arc<dyn Live> = s.clone();
    let mut buf = vec![0; READ];
    let mut got = Some(first);
    while let Some(new) = got {
        if !new
            .into_iter()
            .all(|e| events.send((me.clone(), e)).is_ok())
        {
            s.deaf();
        }
        let n = loop {
            match reader.read(&mut buf) {
                Ok(0) => break Err(Error::Unavailable("connection closed".into())),
                Ok(n) => break Ok(n),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => break Err(net_err(e)),
            }
        };
        got = s.received(n.map(|n| &buf[..n]));
    }
    let _ = events.send((me, LiveEvent::Ended(s.why())));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PROTOCOL as VERSION;
    use crate::protocol::sample_served;
    use crate::transport::driver;
    use crate::transport::pipe::connection;
    use std::sync::mpsc::{Receiver, sync_channel};

    type Events = Receiver<(Arc<dyn Live>, LiveEvent)>;

    /// A live session to a server running `echo`, over a connection whose
    /// reads wait at most `timeout`, both sides pinging after `ping`.
    fn session(timeout: Duration, ping: Duration) -> (Arc<dyn Live>, Events) {
        let ((cr, cw), (sr, sw), chans) = connection(timeout);
        let chans = Arc::new(chans);
        let close = || -> Close {
            let c = chans.clone();
            Box::new(move || c.iter().for_each(|c| c.break_off()))
        };
        let served = sample_served(&[], Hash::ZERO, VERSION);
        let live = served.live.clone();
        let r = Responder::new(served, [1; 32]);
        let server_close = close();
        thread::spawn(move || driver::serve(sr, sw, r, live, server_close, ping));
        let (tx, rx) = sync_channel(16);
        (
            connect(cr, cw, Client::new(), close(), tx, ping).unwrap(),
            rx,
        )
    }

    #[test]
    fn pings_keep_an_idle_session_open() {
        let ms = Duration::from_millis;
        let (live, rx) = session(ms(200), ms(40));
        thread::sleep(ms(600));
        live.open("x", b"").unwrap();
        let (_, e) = rx.recv_timeout(ms(2000)).unwrap();
        assert!(matches!(e, LiveEvent::Open { .. }), "{e:?}");

        let (_, rx) = session(ms(200), Duration::from_secs(10));
        let (_, e) = rx.recv_timeout(ms(2000)).unwrap();
        assert!(
            matches!(e, LiveEvent::Ended(Error::Unavailable(_))),
            "without pings the read limit ends it: {e:?}"
        );
    }
}
