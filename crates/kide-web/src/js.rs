//! The browser APIs this crate uses, declared by hand: WebTransport and the
//! streams of its bidirectional stream, the origin private file system, and
//! timers. `web-sys` marks WebTransport unstable, and these few declarations
//! are all it would give.

use js_sys::{Function, Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[derive(Clone)]
    pub type WebTransport;
    #[wasm_bindgen(constructor, catch)]
    pub fn new(url: &str, options: &JsValue) -> Result<WebTransport, JsValue>;
    #[wasm_bindgen(method, getter)]
    pub fn ready(this: &WebTransport) -> Promise;
    #[wasm_bindgen(method, js_name = createBidirectionalStream)]
    pub fn create_bidirectional_stream(this: &WebTransport) -> Promise;
    #[wasm_bindgen(method, catch)]
    pub fn close(this: &WebTransport) -> Result<(), JsValue>;

    pub type BidirectionalStream;
    #[wasm_bindgen(method, getter)]
    pub fn readable(this: &BidirectionalStream) -> ReadableStream;
    #[wasm_bindgen(method, getter)]
    pub fn writable(this: &BidirectionalStream) -> WritableStream;

    pub type ReadableStream;
    #[wasm_bindgen(method, js_name = getReader)]
    pub fn get_reader(this: &ReadableStream) -> Reader;

    /// A `ReadableStreamDefaultReader`.
    pub type Reader;
    #[wasm_bindgen(method)]
    pub fn read(this: &Reader) -> Promise;

    pub type WritableStream;
    #[wasm_bindgen(method, js_name = getWriter)]
    pub fn get_writer(this: &WritableStream) -> Writer;

    /// A `WritableStreamDefaultWriter`.
    pub type Writer;
    #[wasm_bindgen(method)]
    pub fn write(this: &Writer, chunk: &Uint8Array) -> Promise;

    /// A `FileSystemDirectoryHandle` of the origin private file system.
    pub type DirectoryHandle;
    #[wasm_bindgen(method, js_name = getDirectoryHandle)]
    pub fn get_directory_handle(this: &DirectoryHandle, name: &str, options: &JsValue) -> Promise;
    #[wasm_bindgen(method, js_name = getFileHandle)]
    pub fn get_file_handle(this: &DirectoryHandle, name: &str, options: &JsValue) -> Promise;

    /// A `FileSystemFileHandle`.
    pub type FileHandle;
    #[wasm_bindgen(method, js_name = createSyncAccessHandle)]
    pub fn create_sync_access_handle(this: &FileHandle) -> Promise;

    /// A `FileSystemSyncAccessHandle`: synchronous file access, in dedicated
    /// workers only.
    pub type SyncHandle;
    #[wasm_bindgen(method, catch)]
    pub fn read(this: &SyncHandle, buf: &mut [u8], options: &JsValue) -> Result<f64, JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn write(this: &SyncHandle, buf: &[u8], options: &JsValue) -> Result<f64, JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn truncate(this: &SyncHandle, size: f64) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = getSize)]
    pub fn get_size(this: &SyncHandle) -> Result<f64, JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn flush(this: &SyncHandle) -> Result<(), JsValue>;
    #[wasm_bindgen(method)]
    pub fn close(this: &SyncHandle);

    /// `crypto.getRandomValues`, which fills `buf`.
    #[wasm_bindgen(js_namespace = crypto, js_name = getRandomValues, catch)]
    pub fn random(buf: &mut [u8]) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_name = setTimeout)]
    fn set_timeout(f: &Function, ms: f64) -> JsValue;
    #[wasm_bindgen(js_name = clearTimeout)]
    fn clear_timeout(id: &JsValue);
}

/// Runs `f` once after `ms` milliseconds, unless dropped before.
pub struct Timer {
    id: JsValue,
    _f: Closure<dyn FnMut()>,
}

impl Timer {
    pub fn once(ms: f64, f: impl FnOnce() + 'static) -> Timer {
        let mut f = Some(f);
        let f = Closure::<dyn FnMut()>::new(move || {
            if let Some(f) = f.take() {
                f();
            }
        });
        let id = set_timeout(f.as_ref().unchecked_ref(), ms);
        Timer { id, _f: f }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        clear_timeout(&self.id);
    }
}

/// A JavaScript exception as text.
pub fn describe(e: &JsValue) -> String {
    match e.dyn_ref::<js_sys::Error>() {
        Some(e) => String::from(e.message()),
        None => format!("{e:?}"),
    }
}

/// The root of the origin private file system: `navigator.storage.getDirectory()`.
pub async fn storage_root() -> Result<DirectoryHandle, JsValue> {
    let navigator = Reflect::get(&js_sys::global(), &"navigator".into())?;
    let storage = Reflect::get(&navigator, &"storage".into())?;
    let get: Function = Reflect::get(&storage, &"getDirectory".into())?.dyn_into()?;
    let root = JsFuture::from(Promise::from(get.call0(&storage)?)).await?;
    Ok(root.unchecked_into())
}

/// `{ key: value }`.
pub fn object(key: &str, value: JsValue) -> JsValue {
    let o = Object::new();
    let _ = Reflect::set(&o, &key.into(), &value);
    o.into()
}
