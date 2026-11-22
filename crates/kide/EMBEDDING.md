# Moves, history and forward undo in an application

This is an integration guide for format 8. [SPEC §5.2](SPEC.md#52-version-state)
defines the behavior. The native folder engines require a filesystem; a browser
application needs its own operation engine over the format and workspace traits.

## Move or rename inside one folder

Use paths relative to the same Kide folder, with `/` separators. File and symlink
operands have no trailing slash; directory operands both end in `/`. Directories
are prefixes, including their live empty-directory entries. Renaming a file is
the same operation as moving it.

```rust
use kide::change::Rescan;
use kide::folder::{Folder, moves::move_path, scan::scan};
use kide::names::Platform;

let mut folder = Folder::open(state_directory)?;
let ws = folder.workspace();
let scanned = scan(&mut folder, &ws, &mut Rescan, &Platform::current())?;
// Check scanned.raced and held paths in the application. If the request
// carries a reviewed expected HEAD, compare it with folder.head here.
let before_move = folder.head;
let after_move = move_path(&mut folder, &ws, "drafts/", "published/")?;
// Retain before_move as the undo target and after_move as its expectation.
```

`Folder::open` holds the folder's exclusive writer lock until drop; a competing
writer gets `Unavailable`. An application sharing one open `Folder` must also
serialize its own scan, pull and mutation requests. Apply its authorization,
nested-space boundary checks and any expected-HEAD comparison while serialized.
`move_path` itself has no expected-HEAD argument. A scan can advance HEAD, so
check the expectation after scanning, before making the move. Use a complete
scan (`Rescan`) when the application's incremental dirty set may be incomplete.
Publish the resulting HEAD through the application's normal announcements and
replication path. A successful local move or restore is not remote acknowledgement
that another device or server durably received it.

Call `move_path` **before** renaming anything on disk. It verifies the scanned
workspace, preserves content/state/mtime and record ancestry, renames the disk
tree, and publishes source tombstones and destination records in one HEAD.
Live destination history or disk content is refused. Deleted destination history
is joined into the new lineage. An empty parent directory can receive moved
content; its former empty-directory record becomes a tombstone retaining that
directory's history. Overlap, ignored/held sources, ignored
descendants, invalid/unrepresentable destinations and unscanned changes are
refused. Policy refusals here are `Invalid`; a filesystem collision can be `Io`.
Send-mode and read-only folders cannot perform the operation.

The manifest publication is atomic; the filesystem and HEAD are not a single
filesystem transaction. A durable `move-intent` lets writable reopen finish a
crash between rename, index and HEAD publication. On an error after mutation
started, stop using the folder for writes and reopen it under the lock; inspect
the recovered HEAD before reporting failure or retrying. An ambiguous intent
fails closed. Never remove the intent or replay a rename blindly.

Concurrent rename/edit follows ordinary path reconciliation: the destination
keeps the moved value, and the edit survives in the old path's history/conflict
copy. It is not redirected to the destination. Two concurrent moves retain both
destinations. Applications must show conflicts rather than promise a globally
serialized rename across disconnected devices.

## Undo a move by restoring a tree

Retain the pre-move manifest root and the post-move expected HEAD in the
application's undo record, scoped to the folder/volume. For immediate undo:

```rust
use kide::folder::restore::restore_tree;

scan(&mut folder, &ws, &mut Rescan, &Platform::current())?;
let report = restore_tree(&mut folder, &ws, &before_move, &after_move)?;
if !report.skipped.is_empty() {
    // Partial restore: preserve and display the skipped paths for review.
}
let resulting_head = report.head;
```

This restores the **whole tree**, including other paths changed since the target
snapshot. A stale expected HEAD returns `Invalid`. Do not replace the expectation
with the latest HEAD automatically: present intervening changes and obtain the
application's explicit acceptance before restoring through a newer expectation.
Scanning first makes local edits participate in that check. Guard refusals and
scan races also need resolution before claiming success.

`restore_tree` creates forward records with `Change::Restore(earlier_root)`;
changed paths descend from their current versions, and removed paths become
tombstones. HEAD does not rewind; unchanged values and mtimes need no new record.
Intervening history remains, with replaced disk files retained by the workspace's
trash policy. Missing/corrupt wanted content fails preflight before disk writes.
Races and local path failures during application may produce `Ok` with `skipped`
paths; later application errors may also follow partial progress. Inspect the
report or recovered state before reporting completion. This is a forward restore,
not the deletion guard's `Deletions::Restore` scan mode or `checkout`.

Earlier manifests are retained under the folder's `keep_days` policy. Restore
metadata is provenance, not a retention link. If an undo UI promises a longer
lifetime, the application must retain the target root and required content as GC
roots for that lifetime; merely storing its hash in an undo record is insufficient.

## Read history including moves

```rust
use kide::format::change::Change;
use kide::history::{Event, history};

let entry = folder.index.0.get("published/note.md");
if let Some(entry) = entry {
    let rows = history(&*folder.store, &entry.version)?;
    for row in rows {
        match (row.event, Change::decode(&row.record.change)?) {
            (Event::Move, Some(Change::Move { from, to })) => {
                // Display operands. For directory moves these are prefixes.
            }
            (Event::Restore, Some(Change::Restore(root))) => {
                // Display forward-restore provenance.
            }
            _ => { /* new, edit, copy, merge, delete */ }
        }
    }
}
```

Start from a **version record**, not a manifest HEAD. For an identified manifest
snapshot use `load::validate_manifest(store, &root)` to find its path's version;
a read-only folder's index and HEAD can change while a writer runs. A nonempty
directory has no record of its own: aggregate descendant histories for a folder
view. Both the source tombstone and destination record expose `Event::Move`.
Deduplicate at the application level if presenting one user action instead of
one row per affected path. The metadata has operands, not a global action ID.

`history` returns all reachable records in child-before-parent order, with lanes
for branches and joins; it is not timestamp ordering or a paginated query.
`row.record.parents` retains ancestry across paths. Use
`load::read_content(store, &row.record.root)` for a live version's bytes; a
tombstone has no content. Historical binary content may have been collected even
while the record remains. Content equality never establishes move identity.

## Disk layout changes

A rename made outside the explicit API cannot subsequently be adopted as a move.
Before a scan, `move_path` refuses the unscanned state; scanning records deletions
and new paths with independent lineage. File watchers and inode equality are not
a substitute for an explicit operation. Applications that control an in-space
rename must route it through `move_path` at the time of the action.

Relocating a whole space's working directory, or changing its display name or
parent outside that space, changes no relative path inside its Kide manifest.
Keep its volume identity and Kide state, pause writers/watchers, relocate the
working tree safely, update the `folder` configuration (which may be separate
from `.kide/`) and reopen using the new workspace location. Scan before resuming.
There is no Kide move event because the internal tree did not move. Nested spaces
remain separate roots; cross-folder moves have no atomic Kide operation.

## Error handling (SPEC §4.4)

| Error | Application action |
| --- | --- |
| `NotFound(id)` | This source lacks an object; seek another source or report unavailable historical content. Never infer deletion. |
| `Unavailable(message)` | Retry later, including writer-lock contention. Never turn it into `NotFound` or deletion. |
| `Corrupt(id)` | Reject the bytes and distrust the source; repair/refetch through the normal verified path. Preserve the error class. |
| `Invalid(message)` | Reject invalid paths, collisions, stale expectations or unscanned/unsupported state; review and correct the request. |
| `Refused(message)` | A scan deletion guard holds; require its confirm/restore/reset/join workflow. |
| `Incompatible(message)` | Stop normal sync/open until versions agree; no format-marker override. |
| `Protocol(message)` | Close the malformed/unexpected session. |
| `Io(error)` | Report the local filesystem failure; inspect partial operation state as described above. |

Do not map every failed content read to an absent version. UI wrappers must
distinguish collected content from unreachable or corrupt content. Application
authorization remains mandatory for moves, restores and historical reads.

## Format upgrades outside Kide

Kide supplies no legacy reader, migration command or fallback, including as a
separate crate in this repository. An application needing history-preserving
conversion owns a separate offline tool and its release/cutover policy. Its old
reader must come from the application's exact frozen format-7 pin, not from
heuristic decoding or a new library with its format check disabled. This public
repository starts at format 8; old development commits require their archived
source. Do not add a legacy dependency to the current library.

The converter's contract is:

1. Inventory profile roots before traversal. Folder HEADs and retained roots are
   manifest content; a `Doc` HEAD is a version record. Include removed stores,
   application-pinned roots/versions and every record in their parent DAGs. Read
   a consistent fenced snapshot; write a separate staging generation. Never
   classify arbitrary pack bytes by whether a decoder happens to accept them.
2. Read format-7 records with the old reader, then write format-8 `Record`s with
   identical content root and opaque state, mapped parents sorted by their new
   IDs, and empty `change`. Preserve tombstones, branches, merges and conflict
   copies; invent no moves. Detect cycles, invalid merge parents, limits and
   incomplete required closures. `Record::encode` validates local constraints;
   it does not validate all referenced parents or profile invariants.
   The old layout is `parent_count: u64 LE | parents: count * 32 bytes |
   content_root: 32 bytes | state: remaining bytes`; state starts at
   `40 + 32 * count`. Format 8 inserts `change_length: u64 LE | change` before
   state, so the converted record has a zero change length, not eight state
   bytes stripped or reinterpreted. Verify the source SHA-256 ID before decoding
   and enforce the frozen reader's count, ordering, state and tombstone rules.
3. Validate and copy unchanged content nodes/chunks byte-for-byte, retaining their
   IDs. Track typed traversal contexts and stronger content requirements when an
   object is reached again. A record retained as history need not have all old
   binary content, but every historical version that was readable must remain
   readable. Preserve actual partial availability and report it explicitly;
   missing currently required content is an error.
4. For each retained folder manifest use mapped entry versions and unchanged
   paths, sizes and mtimes; rebuild with `format::manifest::build_manifest`, then
   check with `load::validate_manifest`. Rebuild the index consistently, treating
   disk metadata as a cache to revalidate. Remap retained roots with timestamps.
   For `Doc`, map HEAD directly and preserve exact state and content bytes across
   its full history. `Doc::open` has no format marker guard: the application must
   check its generation/format stamp **before** opening it.
5. Persist an auditable, deterministic mapping scoped by source store, source
   format/generation and reference kind (record versus manifest/content). Include
   unchanged referenced IDs. Never globally replace hash strings in file bytes,
   opaque state or historical JSON. Resolve external pins/history links/restore
   guards through this mapping with current authorization and stale-head checks;
   unknown or wrong-store references fail closed. Inventory current application
   metadata separately and test any explicit rewrite.
6. Compare every current path/kind/content/mtime and every reachable history value
   through the mapping. Hash changes alter deterministic ordering: test unresolved
   frontiers, merges and conflict copies under the new resolver, including future
   reconciliation. An unexplained winner, tree or ancestry change blocks release.
7. Test synthetic fixtures for merges, tombstones/recreations, empty directories,
   symlinks and executable state, text/large binary content, retained roots,
   removed stores and accounts-like `Doc`s. Cover absent optional history content,
   missing/corrupt required objects, invalid graphs, budgets, interruption/resume,
   deterministic reruns and application reference guards. Reopen the converted
   stores with format 8 and verify their complete required closures. No real data
   enters fixtures or this repository.
8. Fence all writers for final verification and activate one complete journalled
   generation, including application references and format stamps. Keep source,
   mappings and a verified rollback backup. A format-7 client cannot upload through
   the ordinary format-8 protocol: drain it before cutover or preserve unuploaded
   work for recovery, then verify a fresh download. Never initialize fresh history
   over unsynced work or remove the old copy on a version mismatch.

The existing public building blocks are `ObjectStore`, `PackStore`,
`format::{record, node, manifest, content}`, `load`, `Folder` and `Doc`. The internal
`walk` traversal is private; a converter must implement typed legacy traversal
with the frozen reader, rather than depend on that private module. No additional
Kide API or dependency is required for the conversion contract above.
