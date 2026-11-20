//! Object stores (§4.1): in memory, and packed segments on disk; a pull's
//! staging (§4.3) is either.

mod mem;
#[cfg(any(unix, windows))]
mod pack;

pub use mem::MemStore;
#[cfg(any(unix, windows))]
pub use pack::{DAMAGED, PackStore, SEGMENT_MAX};
