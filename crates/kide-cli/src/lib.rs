//! The `kide` program's parts beside its commands: the daemon that serves,
//! scans and pulls one folder (§5.8), announcing heads, the addresses taken
//! from a folder's configuration, and the FIPS host-rule warning. The
//! library (`kide`) has the folder profile itself; this is one application
//! of it.

pub mod daemon;
#[cfg(feature = "fips")]
pub mod fips;
pub mod heads;
pub mod network;
#[cfg(feature = "watch")]
pub mod watch;

pub mod quic;
