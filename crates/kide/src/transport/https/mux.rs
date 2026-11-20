//! Bounded byte streams over a WebSocket. Only the server opens streams
//! above zero, for reverse pulls. Every binary message has a nine-byte header.
use super::*;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::{
    MutexGuard,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, mpsc as asynchronous};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

pub(super) const CHUNK: usize = 64 << 10;
const STREAMS: usize = 64;
const QUEUE: usize = 16;
pub(super) const PATH: usize = 4096;
type Incoming = asynchronous::Sender<std::result::Result<Vec<u8>, String>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) struct Connection {
    streams: Mutex<HashMap<u64, Incoming>>,
    out: asynchronous::Sender<Vec<u8>>,
    stop: Notify,
    closed: AtomicBool,
    why: Mutex<String>,
    next: AtomicU64,
    opening: Mutex<()>,
    slots: Arc<tokio::sync::Semaphore>,
}

impl Connection {
    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        lock(&self.streams).clear();
        self.stop.notify_one();
    }
    pub(super) fn closer(self: &Arc<Self>) -> live::Close {
        let c = self.clone();
        Box::new(move || c.close())
    }
    pub(super) fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    fn send(&self, tag: u8, id: u64, bytes: &[u8]) -> io::Result<()> {
        let mut msg = vec![tag];
        msg.extend_from_slice(&id.to_le_bytes());
        msg.extend_from_slice(bytes);
        let start = std::time::Instant::now();
        loop {
            if self.closed() {
                return Err(io::Error::other(lock(&self.why).clone()));
            }
            match self.out.try_send(msg) {
                Ok(()) => return Ok(()),
                Err(asynchronous::error::TrySendError::Full(m)) if start.elapsed() < TIMEOUT => {
                    msg = m;
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                _ => {
                    self.close();
                    return Err(io::ErrorKind::TimedOut.into());
                }
            }
        }
    }
    fn stream(self: &Arc<Self>, id: u64, rt: Arc<Runtime>) -> Result<(Reader, Writer)> {
        let (send, recv) = asynchronous::channel(QUEUE);
        let mut streams = lock(&self.streams);
        if self.closed() || streams.len() >= STREAMS || streams.contains_key(&id) {
            return Err(Error::Unavailable(
                "HTTPS stream limit or closed connection".into(),
            ));
        }
        streams.insert(id, send);
        Ok((
            Reader {
                recv,
                pending: std::io::Cursor::new(Vec::new()),
                conn: self.clone(),
                id,
                _rt: rt.clone(),
            },
            Writer {
                conn: self.clone(),
                id,
                _rt: rt,
            },
        ))
    }
    pub(super) fn open(self: &Arc<Self>, path: &str, rt: Arc<Runtime>) -> Result<(Reader, Writer)> {
        if path.len() > PATH {
            return Err(Error::Invalid(
                "HTTPS reverse path exceeds 4096 bytes".into(),
            ));
        }
        let _opening = lock(&self.opening);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        if id == u64::MAX {
            self.close();
            return Err(Error::Unavailable(
                "HTTPS stream identifiers exhausted".into(),
            ));
        }
        let ends = self.stream(id, rt)?;
        self.send(0, id, path.as_bytes())
            .map_err(super::super::net_err)?;
        Ok(ends)
    }
}

pub(super) struct Reader {
    recv: asynchronous::Receiver<std::result::Result<Vec<u8>, String>>,
    pending: std::io::Cursor<Vec<u8>>,
    conn: Arc<Connection>,
    id: u64,
    _rt: Arc<Runtime>,
}
pub(super) struct Writer {
    conn: Arc<Connection>,
    id: u64,
    _rt: Arc<Runtime>,
}
pub(super) struct Bridge(pub Reader, pub Writer);
impl Read for Bridge {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}
impl Write for Bridge {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.1.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.1.flush()
    }
}
impl Read for Reader {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if b.is_empty() {
            return Ok(0);
        }
        if self.pending.position() as usize == self.pending.get_ref().len() {
            self.pending = std::io::Cursor::new(
                match self
                    ._rt
                    .block_on(async { timeout(TIMEOUT, self.recv.recv()).await })
                {
                    Ok(Some(Ok(bytes))) => bytes,
                    Ok(Some(Err(reason))) => return Err(io::Error::other(reason)),
                    Err(_) => return Err(io::ErrorKind::TimedOut.into()),
                    Ok(None) if self.conn.closed() => {
                        return Err(io::Error::other(lock(&self.conn.why).clone()));
                    }
                    Ok(None) => return Ok(0),
                },
            );
        }
        self.pending.read(b)
    }
}
impl Write for Writer {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = b.len().min(CHUNK);
        if n != 0 {
            self.conn.send(1, self.id, &b[..n])?;
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        lock(&self.conn.streams).remove(&self.id);
        if self.id == 0 {
            self.conn.close();
        }
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.conn.send(2, self.id, &[]);
    }
}

pub(super) fn start<S>(
    ws: WebSocketStream<S>,
    rt: &Arc<Runtime>,
    serve: Option<Route>,
) -> Result<(Arc<Connection>, Reader, Writer)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (tx, rx) = asynchronous::channel(QUEUE);
    let conn = Arc::new(Connection {
        streams: Mutex::new(HashMap::new()),
        out: tx,
        stop: Notify::new(),
        closed: AtomicBool::new(false),
        why: Mutex::new("HTTPS connection closed".into()),
        next: AtomicU64::new(1),
        opening: Mutex::new(()),
        slots: Arc::new(tokio::sync::Semaphore::new(STREAMS - 1)),
    });
    let (reader, writer) = conn.stream(0, rt.clone())?;
    // The tasks must not own the runtime: it may be dropped only outside an async task.
    let c = conn.clone();
    let weak_rt = Arc::downgrade(rt);
    rt.spawn(async move {
        let result = run(ws, rx, &c, serve, weak_rt).await;
        if let Err(e) = result {
            *lock(&c.why) = e.to_string();
        }
        c.close();
    });
    Ok((conn, reader, writer))
}

async fn run<S>(
    ws: WebSocketStream<S>,
    mut out: asynchronous::Receiver<Vec<u8>>,
    c: &Arc<Connection>,
    serve: Option<Route>,
    rt: std::sync::Weak<Runtime>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut input) = ws.split();
    let writer = async {
        while let Some(msg) = out.recv().await {
            timeout(TIMEOUT, sink.send(Message::Binary(msg.into())))
                .await
                .map_err(|e| unavailable("HTTPS write", e))?
                .map_err(|e| unavailable("HTTPS write", e))?;
        }
        Ok::<_, Error>(())
    };
    let reader = async {
        let mut last = 0;
        while let Some(msg) = input.next().await {
            let bytes = match msg.map_err(|e| unavailable("HTTPS read", e))? {
                Message::Binary(b) => b,
                Message::Close(_) => return Ok(()),
                Message::Ping(_) | Message::Pong(_) => continue,
                _ => return Err(Error::Protocol("HTTPS requires binary messages".into())),
            };
            if bytes.len() < 9 || bytes.len() > CHUNK + 9 {
                return Err(Error::Protocol("HTTPS envelope length".into()));
            }
            let mut id = [0; 8];
            id.copy_from_slice(&bytes[1..9]);
            let id = u64::from_le_bytes(id);
            let body = &bytes[9..];
            let known = if serve.is_some() {
                last
            } else {
                c.next.load(Ordering::Relaxed).saturating_sub(1)
            };
            if bytes[0] != 0 && id > known {
                return Err(Error::Protocol("HTTPS data on an unopened stream".into()));
            }
            match bytes[0] {
                0 => {
                    if id == 0 || id <= last || body.len() > PATH {
                        return Err(Error::Protocol(
                            "HTTPS reverse stream identifier/path".into(),
                        ));
                    }
                    last = id;
                    let Some(serve) = &serve else {
                        return Err(Error::Protocol("unsolicited HTTPS reverse stream".into()));
                    };
                    let path = std::str::from_utf8(body)
                        .map_err(|_| Error::Protocol("HTTPS path is not UTF-8".into()))?;
                    let Some(runtime) = rt.upgrade() else {
                        return Ok(());
                    };
                    let (r, w) = c.stream(id, runtime)?;
                    let slot =
                        c.slots.clone().try_acquire_owned().map_err(|_| {
                            Error::Unavailable("HTTPS reverse session limit".into())
                        })?;
                    let (serve, path) = (serve.clone(), path.to_owned());
                    std::thread::spawn(move || {
                        let _slot = slot;
                        if let Some(served) = serve(&path) {
                            super::server::serve(r, w, served, None);
                        }
                    });
                }
                1 if !body.is_empty() => {
                    let tx = lock(&c.streams).get(&id).cloned();
                    if let Some(tx) = tx {
                        timeout(TIMEOUT, tx.send(Ok(body.to_vec())))
                            .await
                            .map_err(|e| unavailable("HTTPS stream backpressure", e))?
                            .ok();
                    }
                }
                2 if body.is_empty() => {
                    lock(&c.streams).remove(&id);
                }
                3 if body.len() <= PATH => {
                    let reason = std::str::from_utf8(body)
                        .map_err(|_| Error::Protocol("HTTPS refusal is not UTF-8".into()))?;
                    let tx = lock(&c.streams).remove(&id);
                    if let Some(tx) = tx {
                        let _ = timeout(TIMEOUT, tx.send(Err(reason.to_owned()))).await;
                    }
                }
                _ => return Err(Error::Protocol("HTTPS envelope tag/payload".into())),
            }
        }
        Ok(())
    };
    let result =
        tokio::select! { r = writer => r, r = reader => r, _ = c.stop.notified() => Ok(()) };
    // Deliver queued refusals before the TLS socket is dropped.
    let _ = timeout(TIMEOUT, async {
        while let Ok(msg) = out.try_recv() {
            sink.send(Message::Binary(msg.into())).await?;
        }
        sink.close().await
    })
    .await;
    result
}

pub(super) fn refusal(w: &Writer, e: &Error) {
    let mut reason = e.to_string();
    let mut end = reason.len().min(PATH);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason.truncate(end);
    let _ = w
        .conn
        .send(3, w.id, &reason.as_bytes()[..reason.len().min(PATH)]);
}
pub(super) fn connection(r: &Reader) -> Arc<Connection> {
    r.conn.clone()
}

pub(super) fn runtime(r: &Reader) -> std::sync::Weak<Runtime> {
    Arc::downgrade(&r._rt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::protocol::Role;

    #[test]
    fn malformed_envelopes_and_unsolicited_reverse_streams_end_connections() {
        for bytes in [
            vec![1],
            vec![9; 9],
            vec![1; 9],
            vec![0; 9],
            vec![3, 0, 0, 0, 0, 0, 0, 0, 0, 255],
            vec![1, 0, 0, 0, 0, 0, 0, 0, 0],
        ] {
            let rt = super::super::runtime().unwrap();
            let (server, mut peer) = rt.block_on(async {
                let (a, b) = tokio::io::duplex(4096);
                (
                    WebSocketStream::from_raw_socket(a, Role::Server, Some(ws_config())).await,
                    WebSocketStream::from_raw_socket(b, Role::Client, Some(ws_config())).await,
                )
            });
            let (_conn, mut reader, _writer) = start(server, &rt, None).unwrap();
            rt.block_on(peer.send(Message::Binary(bytes.into())))
                .unwrap();
            let e = reader.read(&mut [0; 1]).unwrap_err();
            assert!(e.to_string().contains("protocol error:"), "{e}");
        }
    }
}
