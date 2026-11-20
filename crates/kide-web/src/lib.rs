//! Kide in the browser (built for `wasm32-unknown-unknown`): live sessions
//! over WebTransport, for JavaScript through `live` and `LiveSession`; pulls
//! over WebTransport (`pull`, for Rust applications with their own profile);
//! and an object store in the origin private file system. On other targets
//! the crate is empty.
#![cfg(target_arch = "wasm32")]

mod js;
pub mod live;
mod opfs;
pub mod pull;
mod session;
mod signal;

pub use opfs::OpfsStore;

use js_sys::{Function, Object, Reflect, Uint8Array};
use kide::auth::Secret;
use kide::hash::to_hex;
use kide::iface::{Live, LiveEvent};
use kide::transport::live::Session;
use std::sync::Arc;
use wasm_bindgen::prelude::*;

/// A live session, as JavaScript sees it. Calls never wait; they fail once
/// the session has ended.
#[wasm_bindgen]
pub struct LiveSession(Arc<Session>);

fn thrown(e: kide::Error) -> JsError {
    JsError::new(&e.to_string())
}

#[wasm_bindgen]
impl LiveSession {
    /// Opens the stream `name` at the application's `version`.
    pub fn open(&self, name: &str, version: &[u8]) -> Result<(), JsError> {
        self.0.open(name, version).map_err(thrown)
    }

    pub fn update(&self, name: &str, bytes: &[u8]) -> Result<(), JsError> {
        self.0.update(name, bytes).map_err(thrown)
    }

    pub fn close(&self, name: &str) -> Result<(), JsError> {
        self.0.close(name).map_err(thrown)
    }

    /// Ends the session once what was queued is sent.
    pub fn bye(&self) -> Result<(), JsError> {
        self.0.bye().map_err(thrown)
    }
}

/// A device key: a secp256k1 secret key that answers a server's challenge
/// (§4.2). The application keeps its bytes (in IndexedDB, say) and enrolls
/// its public key with the server, whose admission rule then lets it in.
#[wasm_bindgen]
pub struct Key(Secret);

#[wasm_bindgen]
impl Key {
    /// A new key from the browser's random source.
    pub fn generate() -> Result<Key, JsError> {
        loop {
            let mut b = [0; 32];
            js::random(&mut b).map_err(|e| JsError::new(&js::describe(&e)))?;
            // Fails only for 0 and values past the group order, which a
            // random draw almost never gives: draw again.
            if let Ok(s) = Secret::from_bytes(&b) {
                return Ok(Key(s));
            }
        }
    }

    /// The key whose secret is `bytes` (32 of them).
    #[wasm_bindgen(js_name = fromBytes)]
    pub fn from_bytes(bytes: &[u8]) -> Result<Key, JsError> {
        Ok(Key(secret(bytes).map_err(thrown)?))
    }

    /// The secret, to keep.
    pub fn bytes(&self) -> Vec<u8> {
        self.0.to_bytes().to_vec()
    }

    /// The x-only public key in hex, as a server's admission rule lists it.
    #[wasm_bindgen(js_name = publicKey)]
    pub fn public_key(&self) -> String {
        to_hex(&self.0.public())
    }
}

/// The secret key whose bytes are `bytes` (32 of them), as `Key::bytes`
/// gives them.
pub fn secret(bytes: &[u8]) -> kide::Result<Secret> {
    match <&[u8; 32]>::try_from(bytes) {
        Ok(b) => Secret::from_bytes(b),
        Err(_) => Err(kide::Error::Invalid("a secret key has 32 bytes".into())),
    }
}

/// Opens a live session to `url` (`https://host:port`). `certHash`, the
/// SHA-256 of a self-signed server certificate, is needed only for a server
/// without a certificate the browser trusts. `key`, the bytes of a `Key`,
/// answers a challenging server. `onEvent` is called with
/// `{type: "open", name, version}`, `{type: "update", name, bytes}`,
/// `{type: "close", name}` and last `{type: "ended", error}`; the bytes are
/// `Uint8Array`s and the error is text.
#[wasm_bindgen]
pub async fn live(
    url: String,
    #[wasm_bindgen(js_name = certHash)] cert_hash: Option<Vec<u8>>,
    #[wasm_bindgen(js_name = onEvent)] on_event: Function,
    key: Option<Vec<u8>>,
) -> Result<LiveSession, JsError> {
    let key = key.map(|k| secret(&k)).transpose().map_err(thrown)?;
    let deliver = move |e: LiveEvent| {
        // An exception in the application's handler is its own; the
        // session goes on.
        let _ = on_event.call1(&JsValue::NULL, &event(e));
    };
    let s = live::connect(&url, cert_hash.as_deref(), key.map(Arc::new), deliver).await;
    Ok(LiveSession(s.map_err(thrown)?))
}

/// An event as a plain object.
fn event(e: LiveEvent) -> JsValue {
    let o = Object::new();
    let set = |k: &str, v: JsValue| {
        let _ = Reflect::set(&o, &k.into(), &v);
    };
    let bytes = |b: &[u8]| JsValue::from(Uint8Array::from(b));
    match e {
        LiveEvent::Open { name, version } => {
            set("type", "open".into());
            set("name", name.into());
            set("version", bytes(&version));
        }
        LiveEvent::Update { name, bytes: b } => {
            set("type", "update".into());
            set("name", name.into());
            set("bytes", bytes(&b));
        }
        LiveEvent::Close { name } => {
            set("type", "close".into());
            set("name", name.into());
        }
        LiveEvent::Ended(e) => {
            set("type", "ended".into());
            set("error", e.to_string().into());
        }
    }
    o.into()
}
