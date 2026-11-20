//! Errors (§4.4). Every operation fails with exactly one class, and the classes
//! are never converted into one another: an unreachable object is not a
//! missing one, and neither is a deletion.

use crate::hash::Hash;
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// The source does not have the object: look elsewhere.
    NotFound(Hash),
    /// The source cannot be reached now: retry later and change nothing.
    Unavailable(String),
    /// The bytes do not match their id: discard them and distrust the source.
    Corrupt(Hash),
    /// Bytes or input that break a rule of the spec, or a key the other side
    /// refuses or lacks (§4.2).
    Invalid(String),
    /// A malformed frame or an unexpected message.
    Protocol(String),
    /// The peer speaks no common protocol version, or serves another folder
    /// format: do not retry until one side is updated.
    Incompatible(String),
    /// A guard of the user's data holds (§5.5): change nothing until the
    /// user confirms, restores, resets or joins.
    Refused(String),
    /// A local file-system failure.
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound(id) => write!(f, "not found: {id}"),
            Error::Unavailable(m) => write!(f, "unavailable: {m}"),
            Error::Corrupt(id) => write!(f, "corrupt object: {id}"),
            Error::Invalid(m) => write!(f, "invalid: {m}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Incompatible(m) => write!(f, "incompatible: {m}"),
            Error::Refused(m) => write!(f, "refused: {m}"),
            Error::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl Clone for Error {
    /// The same error; an `Io` error keeps its kind and message only.
    fn clone(&self) -> Self {
        match self {
            Error::NotFound(id) => Error::NotFound(*id),
            Error::Unavailable(m) => Error::Unavailable(m.clone()),
            Error::Corrupt(id) => Error::Corrupt(*id),
            Error::Invalid(m) => Error::Invalid(m.clone()),
            Error::Protocol(m) => Error::Protocol(m.clone()),
            Error::Incompatible(m) => Error::Incompatible(m.clone()),
            Error::Refused(m) => Error::Refused(m.clone()),
            Error::Io(e) => Error::Io(std::io::Error::new(e.kind(), e.to_string())),
        }
    }
}

impl From<std::io::Error> for Error {
    /// An `Error` carried through `std::io` (by a reader) comes back unchanged.
    fn from(e: std::io::Error) -> Self {
        if !e.get_ref().is_some_and(|inner| inner.is::<Error>()) {
            return Error::Io(e);
        }
        match e.into_inner().map(|inner| inner.downcast::<Error>()) {
            Some(Ok(inner)) => *inner,
            // Unreachable: the inner error was just seen to be an `Error`.
            _ => Error::Invalid("an error was lost".into()),
        }
    }
}

impl From<Error> for std::io::Error {
    /// Carries an `Error` through `std::io`; `Io` passes as itself.
    fn from(e: Error) -> Self {
        match e {
            Error::Io(e) => e,
            e => std::io::Error::other(e),
        }
    }
}

/// Shorthand for an `Invalid` error.
pub fn invalid<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_survive_a_trip_through_io() {
        let id = Hash([3; 32]);
        let back = |e: Error| Error::from(std::io::Error::from(e));
        assert!(matches!(back(Error::NotFound(id)), Error::NotFound(x) if x == id));
        assert!(matches!(back(Error::Corrupt(id)), Error::Corrupt(x) if x == id));
        assert!(matches!(
            back(Error::Unavailable("u".into())),
            Error::Unavailable(_)
        ));
        let os = std::io::Error::from_raw_os_error(2);
        assert!(matches!(back(Error::Io(os)), Error::Io(e) if e.raw_os_error() == Some(2)));
        let other = std::io::Error::other("plain");
        assert!(matches!(Error::from(other), Error::Io(_)));
    }
}
