//! Kide: folders as content-defined hash trees, synchronised between one
//! user's own devices. `SPEC.md` (folder format 8, protocol 8) is normative;
//! section numbers (§) refer to it.
//!
//! The modules that use the file system (the pack store, workspaces, change
//! sources and folders) exist on unix and Windows; the rest also builds for
//! `wasm32-unknown-unknown`. `examples/two_folders.rs` shows the library in
//! one page.

pub mod auth;
#[cfg(any(unix, windows))]
pub mod change;
#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
#[cfg(any(unix, windows))]
pub mod doc;
pub mod error;
pub mod fetch;
pub mod file;
#[cfg(any(unix, windows))]
pub mod folder;
pub mod format;
pub mod gc;
pub mod hash;
pub mod history;
pub mod iface;
pub mod load;
pub mod log;
pub mod names;
#[cfg(any(unix, windows))]
pub(crate) mod os;
pub mod protocol;
pub mod report;
pub mod resolve;
pub mod store;
pub(crate) mod text;
pub mod time;
pub mod transport;
pub mod walk;
#[cfg(any(unix, windows))]
pub mod workspace;

pub use error::{Error, Result};
pub use hash::Hash;
