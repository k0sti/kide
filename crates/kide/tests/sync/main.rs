//! Pulls between devices (§5.6): device fixtures over the in-process
//! transport with plain folders in temporary directories.
//! Unix only: the fixtures use symbolic links and permission bits.
#![cfg(unix)]

mod device;
mod failures;
mod gc;
mod moves;
mod names;
mod random;
mod recovery;
mod three_devices;
mod two_devices;
