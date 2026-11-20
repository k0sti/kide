//! One pull (§5.6): scan, Hello, fetch the remote root's closure (§4.3),
//! validate its manifest, decide every differing path, apply, Bye. Any
//! error before applying leaves the folder, the index and `HEAD` as the
//! scan left them; fetched objects stay staged (§4.4).

use super::Folder;
use super::apply::{ApplyReport, apply};
use super::config::Mode;
use super::index::Index;
use super::net::Manifests;
use super::plan::{Plan, plan};
use super::repair::repair;
use super::scan::{ScanReport, scan};
use crate::error::{Error, Result, invalid};
use crate::fetch::{FetchReport, fetch};
use crate::hash::Hash;
use crate::iface::{ChangeSource, Resolver, Session, Transport, Workspace};
use crate::load::validate_manifest;
use crate::log;
use crate::names::Platform;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PullReport {
    pub scan: ScanReport,
    /// The server's root.
    pub remote: Hash,
    /// `None` when the roots were equal.
    pub fetch: Option<FetchReport>,
    /// Paths whose entries differed.
    pub decided: usize,
    /// Paths merged as text (§5.6): both sides' changes, no conflict copy.
    pub merged: Vec<String>,
    pub apply: ApplyReport,
    /// `HEAD` after the pull.
    pub head: Hash,
}

/// What a pull works with besides the folder.
pub struct Pulling<'a> {
    pub ws: &'a dyn Workspace,
    pub source: &'a mut dyn ChangeSource,
    pub resolver: &'a dyn Resolver,
    pub transport: &'a dyn Transport,
    pub platform: &'a Platform,
    /// Reviews the plan before it is applied (for a server that takes
    /// changes only where the peer may make them): it may refuse paths
    /// (`Plan::refuse`), or fail the pull, which then changes nothing.
    pub review: Option<&'a mut Review<'a>>,
}

/// See `Pulling::review`: the plan and the folder's index before applying.
pub type Review<'a> = dyn FnMut(&mut Plan, &Index) -> Result<()> + 'a;

/// How many retained roots a pull names besides `HEAD`.
pub(crate) const HAVES: usize = 63;

/// What a pull says it has (§5.6): `HEAD`, then the newest retained roots.
/// The server skips what any of them reaches, but can walk only those it
/// holds; in two-way sync this device's `HEAD` is often newer than anything
/// the server has seen, while the root it pulled last is not.
fn haves(folder: &Folder) -> Result<Vec<Hash>> {
    let mut have = vec![folder.head];
    for (_, root) in folder.roots()?.into_iter().rev() {
        if have.len() > HAVES {
            break;
        }
        if !have.contains(&root) {
            have.push(root);
        }
    }
    Ok(have)
}

/// Pulls from `peer` into `folder`. A store that dropped a corrupt object
/// (§4.1) is repaired over the same session first; a scan, plan or apply
/// that meets such an object repairs and runs again, once.
pub fn pull(folder: &mut Folder, with: Pulling, peer: &str) -> Result<PullReport> {
    if folder.config.mode == Mode::Send {
        return invalid("a send folder never pulls (mode = send)");
    }
    let Pulling {
        ws,
        source,
        resolver,
        transport,
        platform,
        mut review,
    } = with;
    let scanned = scan(folder, ws, source, platform);
    if let Err(e) = &scanned
        && !damage(folder, e)
    {
        return Err(e.clone());
    }
    let mut session = transport.connect(peer)?;
    let remote = session.hello(&folder.head)?;
    let mut r = PullReport {
        scan: match scanned {
            Ok(s) => s,
            Err(_) => {
                repair(folder, &mut *session, &[folder.head])?;
                scan(folder, ws, source, platform)?
            }
        },
        remote,
        head: folder.head,
        ..PullReport::default()
    };
    if folder.damaged() {
        repair(folder, &mut *session, &[folder.head, remote])?;
    }
    if r.remote == folder.head {
        session.bye()?;
        return Ok(r);
    }
    let mut run = |folder: &mut Folder, session: &mut dyn Session, r: &mut PullReport| {
        merge(
            folder,
            session,
            ws,
            resolver,
            platform,
            review.as_deref_mut(),
            r,
        )
    };
    match run(folder, &mut *session, &mut r) {
        Err(e) if damage(folder, &e) => {
            repair(folder, &mut *session, &[folder.head, remote])?;
            run(folder, &mut *session, &mut r)?;
        }
        done => done?,
    }
    r.head = folder.head;
    if let Err(e) = session.bye() {
        log!("pull from {peer}: Bye after applying: {e}"); // the pull is done
    }
    Ok(r)
}

/// Whether `e` comes from an object the store dropped as corrupt.
fn damage(folder: &Folder, e: &Error) -> bool {
    matches!(e, Error::Corrupt(_) | Error::NotFound(_)) && folder.damaged()
}

/// Fetches the remote root, decides every differing path and applies.
fn merge(
    folder: &mut Folder,
    session: &mut dyn Session,
    ws: &dyn Workspace,
    resolver: &dyn Resolver,
    platform: &Platform,
    review: Option<&mut Review>,
    r: &mut PullReport,
) -> Result<()> {
    let store = folder.store.clone();
    let staging = folder.staging()?;
    let have = haves(folder)?;
    // Fetching syncs the store and clears staging.
    r.fetch = Some(fetch(
        session, &*store, &staging, &Manifests, &r.remote, &have,
    )?);
    validate_manifest(&*store, &r.remote)?;
    let local = folder.head;
    let mut p = plan(
        &*store,
        resolver,
        &folder.index,
        &local,
        &r.remote,
        platform,
    )?;
    // Ignored paths (§5.8) are held: recorded, never written.
    let live = p.target.iter().filter(|(_, t)| t.kind.is_some());
    let ignored: Vec<String> = live
        .filter(|(path, _)| folder.config.ignores(path))
        .map(|(path, _)| path.clone())
        .collect();
    p.held.extend(ignored);
    if let Some(review) = review {
        review(&mut p, &folder.index)?;
    }
    r.decided = p.decided;
    r.merged = p.merged.clone();
    r.apply = apply(folder, ws, &p)?;
    Ok(())
}
