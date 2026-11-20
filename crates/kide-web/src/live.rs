//! Live sessions over the browser's WebTransport (§4.2), driven by two tasks
//! on the page's event loop. The reader hands events to a callback; the
//! writer sends the outbox, or Ping after `PING` without sending. Both share
//! the session of `kide::transport::live`, so calls never wait and a full
//! queue ends the session as on every other transport.

use crate::js::{Reader, Timer, WebTransport, Writer};
use crate::session::{self, Stream, ms, receive, unavailable};
use crate::signal::Signal;
use js_sys::{Date, Uint8Array};
use kide::auth::Secret;
use kide::error::{Error, Result};
use kide::iface::LiveEvent;
use kide::transport::TIMEOUT;
use kide::transport::live::{Next, PING, Session};
use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use wasm_bindgen_futures::{JsFuture, spawn_local};

/// Opens a live session to `url`, trusting a server certificate and
/// answering a challenge with `key` as `session::connect` does. `on_event` gets every event, `Ended` last.
pub async fn connect(
    url: &str,
    cert_hash: Option<&[u8]>,
    key: Option<Arc<Secret>>,
    on_event: impl FnMut(LiveEvent) + 'static,
) -> Result<Arc<Session>> {
    let (stream, _) = session::connect(url, cert_hash, key).await?;
    let Stream {
        wt,
        reader,
        writer,
        client,
        ..
    } = stream;
    let signal = Arc::new(Signal::default());
    let (close, wake) = (signal.clone(), signal.clone());
    let s = Session::client(
        client,
        Box::new(move || close.notify()),
        Some(Box::new(move || wake.notify())),
    );
    let s = match s {
        Ok(s) => s,
        Err(e) => {
            let _ = wt.close();
            return Err(e);
        }
    };
    spawn_local(write(s.clone(), signal, writer, wt));
    spawn_local(read(s.clone(), reader, on_event));
    Ok(s)
}

/// Sends the outbox as it fills, and Ping after `PING` without sending; once
/// the session ends, closes the transport, which ends the reader.
async fn write(s: Arc<Session>, signal: Arc<Signal>, writer: Writer, wt: WebTransport) {
    let mut sent = Date::now();
    loop {
        let idle = Duration::from_secs_f64((Date::now() - sent).max(0.0) / 1000.0);
        match s.next(idle, PING) {
            Next::Stop => break,
            Next::Wait(d) => {
                let wake = signal.clone();
                let _timer = Timer::once(ms(d), move || wake.notify());
                signal.wait().await;
            }
            Next::Send(out) => {
                // Waits for the write, unless the session ends meanwhile:
                // a peer that stops reading stalls it for good.
                let mut written = pin!(JsFuture::from(writer.write(&Uint8Array::from(&out[..]))));
                let r = poll_fn(|cx| {
                    if let Poll::Ready(r) = written.as_mut().poll(cx) {
                        return Poll::Ready(Some(r));
                    }
                    while signal.poll(cx) {
                        if s.has_ended() {
                            return Poll::Ready(None);
                        }
                    }
                    Poll::Pending
                })
                .await;
                match r {
                    Some(Ok(_)) => sent = Date::now(),
                    Some(Err(e)) => {
                        s.fail(unavailable("send", &e));
                        break;
                    }
                    None => break,
                }
            }
        }
    }
    let _ = wt.close();
}

/// Reads until the session ends, handing every event to `on_event`, and
/// `Ended` last. A read that waits more than `TIMEOUT` ends the session.
async fn read(s: Arc<Session>, reader: Reader, mut on_event: impl FnMut(LiveEvent)) {
    let mut got = Some(Vec::new());
    while let Some(events) = got {
        events.into_iter().for_each(&mut on_event);
        let late = s.clone();
        let limit = Timer::once(ms(TIMEOUT), move || {
            late.fail(Error::Unavailable("the read limit passed".into()));
        });
        let bytes = receive(&reader).await;
        drop(limit);
        got = s.received(bytes.as_deref().map_err(Clone::clone));
    }
    on_event(LiveEvent::Ended(s.why()));
}
