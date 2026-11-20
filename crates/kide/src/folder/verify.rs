//! `kide verify`: re-hash every stored object, then check that `HEAD`
//! names a valid manifest (§5.1) whose live content is all stored.

use crate::error::{Error, Result};
use crate::format::manifest::Kind;
use crate::hash::Hash;
use crate::iface::ObjectStore;
use crate::load::{self, read_content, validate_manifest};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub objects: usize,
    /// Objects whose bytes do not hash to their id.
    pub corrupt: Vec<Hash>,
    /// Missing, corrupt or invalid objects below `HEAD`, one line each.
    pub problems: Vec<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.corrupt.is_empty() && self.problems.is_empty()
    }
}

pub fn verify(store: &dyn ObjectStore, head: &Hash) -> Result<VerifyReport> {
    let mut r = VerifyReport::default();
    for id in store.ids()? {
        r.objects += 1;
        match store.get(&id) {
            Ok(_) | Err(Error::NotFound(_)) => {} // removed meanwhile
            Err(Error::Corrupt(x)) => r.corrupt.push(x),
            Err(e) => return Err(e),
        }
    }
    let entries = match validate_manifest(store, head) {
        Ok(entries) => entries,
        Err(e) => {
            note(&mut r, format!("HEAD {head}"), e)?;
            return Ok(r);
        }
    };
    for e in entries {
        let content = load::record(store, &e.version).and_then(|rec| match Kind::of(&rec)? {
            Some(_) => read_content(store, &rec.root).map(drop),
            None => Ok(()),
        });
        if let Err(err) = content {
            note(&mut r, e.path, err)?;
        }
    }
    Ok(r)
}

/// Records a problem with the objects; a local failure ends verification.
fn note(r: &mut VerifyReport, what: String, e: Error) -> Result<()> {
    match e {
        Error::NotFound(_) | Error::Corrupt(_) | Error::Invalid(_) => {
            r.problems.push(format!("{what}: {e}"));
            Ok(())
        }
        e => Err(e),
    }
}
