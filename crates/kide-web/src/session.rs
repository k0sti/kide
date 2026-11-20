//! A session over the browser's WebTransport (§4.2): one WebTransport
//! session with one bidirectional stream, opened with Hello. Pulls and live
//! sessions both start here.

use crate::js::{BidirectionalStream, Reader, Timer, WebTransport, Writer, describe};
use js_sys::{Array, Object, Reflect, Uint8Array};
use kide::auth::Secret;
use kide::error::{Error, Result};
use kide::hash::Hash;
use kide::protocol::{Client, Event};
use kide::transport::{TIMEOUT, url_host};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// A browser API's failure: the connection's, so `Unavailable` (§4.4).
pub fn unavailable(what: &str, e: &JsValue) -> Error {
    Error::Unavailable(format!("{what}: {}", describe(e)))
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

async fn wait(promise: js_sys::Promise, what: &str) -> Result<JsValue> {
    JsFuture::from(promise)
        .await
        .map_err(|e| unavailable(what, &e))
}

/// An open session, past Hello.
pub struct Stream {
    pub wt: WebTransport,
    pub reader: Reader,
    pub writer: Writer,
    pub client: Client,
    /// Events received and not yet taken.
    events: VecDeque<Event>,
}

/// Opens a session to `url` (`https://host:port`), trusting a server
/// certificate with SHA-256 `cert_hash` if given (a self-signed one, valid
/// for at most two weeks), otherwise the browser's roots; returns it with
/// the server's root. With `key`, it answers the server's challenge,
/// signing the URL's host as the server's name; a challenging server fails
/// a session without one with `Invalid`. Every read waits at most `TIMEOUT`.
pub async fn connect(
    url: &str,
    cert_hash: Option<&[u8]>,
    key: Option<Arc<Secret>>,
) -> Result<(Stream, Hash)> {
    let client = match key {
        Some(k) => Client::with_key(k, url_host(url)?),
        None => Client::new(),
    };
    let wt = WebTransport::new(url, &options(cert_hash)).map_err(|e| unavailable(url, &e))?;
    let close = wt.clone();
    let _limit = Timer::once(ms(TIMEOUT), move || {
        let _ = close.close();
    });
    let opened = open(&wt, url).await;
    if opened.is_err() {
        let _ = wt.close();
    }
    let (reader, writer) = opened?;
    let mut s = Stream {
        wt,
        reader,
        writer,
        client,
        events: VecDeque::new(),
    };
    let root = s.hello().await;
    if root.is_err() {
        let _ = s.wt.close();
    }
    Ok((s, root?))
}

/// The WebTransport options: the certificate hash, if any.
fn options(cert_hash: Option<&[u8]>) -> JsValue {
    let options = Object::new();
    if let Some(hash) = cert_hash {
        let h = Object::new();
        let _ = Reflect::set(&h, &"algorithm".into(), &"sha-256".into());
        let _ = Reflect::set(&h, &"value".into(), &Uint8Array::from(hash));
        let hashes = Array::of1(&h);
        let _ = Reflect::set(&options, &"serverCertificateHashes".into(), &hashes);
    }
    options.into()
}

/// Waits for the session and opens its stream.
async fn open(wt: &WebTransport, url: &str) -> Result<(Reader, Writer)> {
    wait(wt.ready(), url).await?;
    let bidi = wait(wt.create_bidirectional_stream(), url).await?;
    let bidi: BidirectionalStream = bidi.unchecked_into();
    Ok((bidi.readable().get_reader(), bidi.writable().get_writer()))
}

pub async fn send(writer: &Writer, bytes: &[u8]) -> Result<()> {
    wait(writer.write(&Uint8Array::from(bytes)), "send").await?;
    Ok(())
}

/// The next chunk of the stream; its end is a break.
pub async fn receive(reader: &Reader) -> Result<Vec<u8>> {
    let chunk = wait(reader.read(), "connection").await?;
    let done = Reflect::get(&chunk, &"done".into()).map_err(|e| unavailable("read", &e))?;
    if done.is_truthy() {
        return Err(Error::Unavailable("connection closed".into()));
    }
    let value = Reflect::get(&chunk, &"value".into()).map_err(|e| unavailable("read", &e))?;
    Ok(value.unchecked_into::<Uint8Array>().to_vec())
}

impl Stream {
    /// Sends what the client queued.
    pub async fn flush(&mut self) -> Result<()> {
        let out = self.client.transmit();
        match out.is_empty() {
            true => Ok(()),
            false => send(&self.writer, &out).await,
        }
    }

    /// The next event; a read that fails, ends or waits longer than
    /// `TIMEOUT` is a break, which keeps what arrived before it.
    pub async fn event(&mut self) -> Result<Event> {
        loop {
            if let Some(e) = self.events.pop_front() {
                return Ok(e);
            }
            let close = self.wt.clone();
            let limit = Timer::once(ms(TIMEOUT), move || {
                let _ = close.close();
            });
            let got = receive(&self.reader).await;
            drop(limit);
            let bytes = match got {
                Ok(bytes) => bytes,
                Err(e) => return self.client.broken(&e.to_string()),
            };
            match self.client.receive(&bytes) {
                Ok(events) => self.events.extend(events),
                Err(e) => {
                    let _ = self.flush().await; // a Bye, if the client queued one
                    return Err(e);
                }
            }
        }
    }

    /// Exchanges Hello, and Auth if challenged; the server's root.
    async fn hello(&mut self) -> Result<Hash> {
        self.client.hello(&Hash::ZERO)?;
        self.flush().await?;
        match self.event().await? {
            Event::Hello(root) => {
                self.flush().await?; // Auth, if challenged
                Ok(root)
            }
            _ => Err(Error::Protocol("an answer before Hello".into())),
        }
    }

    /// Says Bye and closes the transport.
    pub async fn bye(mut self) {
        if self.client.bye().is_ok() {
            let _ = self.flush().await;
        }
        let _ = self.wt.close();
    }
}
