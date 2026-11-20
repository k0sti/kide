# Kide

Kide keeps an ordinary folder equal on your own devices. It stores the folder as content-defined hash trees, so an edit costs only the chunks it touches, and keeps a version history per file, so concurrent edits are merged or kept as conflict copies instead of lost. Text files keep their whole history's content, and concurrent edits of a prose file (Markdown, plain text, Org, reStructuredText, AsciiDoc) merge line by line, with no conflict copy; code and data files keep a conflict copy instead. `SPEC.md` (folder format 8, protocol 8) is the normative description; this file is a short user guide.

## Commands

Every command works in the folder given by `-C <dir>`, or the current directory.

| Command | What it does |
|---|---|
| `kide init [dir]` | Makes a Kide folder: creates `.kide/` with a configuration template |
| `kide scan` | Records local changes and prints the new `HEAD` |
| `kide ls [--live]` | Lists the files of `HEAD`, deleted ones too unless `--live` |
| `kide log <path>` | Shows a path's version history in `HEAD`, newest first: one line per version with its lanes (`*` the version, `\|` a lane passing by, `/` a lane joining it), the first 12 hex digits of its id, how it came about (`new`, `edit`, `copy`, `merge`, `delete`), its size, and a merge's parents |
| `kide verify [--repair [device]]` | Re-hashes every stored object and checks `HEAD`; with `--repair`, drops corrupt objects and fetches what is missing from a device. A pull repairs on its own once the store has dropped a corrupt object |
| `kide checkout <root\|HEAD> <dir>` | Writes a root's files into an empty directory |
| `kide gc` | Removes objects that no root needs, trash older than `trash_days` and retained roots older than `keep_days` |
| `kide serve` | Serves `HEAD` to the configured devices until interrupted |
| `kide pull [device]` | Pulls from one configured device, or from all of them in turn |
| `kide confirm` | Lets the next scan record the deletions it refused (see below); works while `kide run` holds the folder |
| `kide restore` | Writes the files a scan finds gone back from the store: nothing is deleted |
| `kide reset [device]` | Takes a device's state: every path gets its version, local changes go (files to the trash) |
| `kide join [device]` | For a folder whose `.kide` is new but which holds files (a reinstalled device): where they differ, the device's version wins and the local one is kept as a conflict copy, unless the device's history already has it |
| `kide run` | The daemon: serves the folder, scans as soon as a file changes (with the default feature `watch`) and every `scan_interval`, pulls from a device as soon as it announces a new root and every `pull_interval`, and collects garbage once a day |

Commands that change the folder take the lock in `.kide/lock`; while `kide run` holds it, `scan`, `pull`, `restore`, `reset`, `join` and `gc` are refused. `ls`, `verify` (without `--repair`) and `checkout` need no lock.

`kide run` runs until it is killed. A kill is safe at any moment: objects of an unfinished pull stay in `.kide/staging/` for the next one, and `HEAD` is written only after everything it needs is stored. An unreachable device is retried after `pull_interval`, then after twice that, up to 5 minutes, until it answers. A device that sends a corrupt object is reported on standard error, nothing from that pull is applied, and it is retried with the same back-off. Garbage is collected once a day between pulls, as `kide gc` does on demand when no daemon runs.

**The guard.** A scan refuses, and changes nothing, when the synchronised directory is missing, when it would leave no file, or when it would lose at least 10 files and more than half of them at once (moving files loses nothing). An unmounted disk or a folder emptied by mistake is then not sent to the other devices as deletions. Choose `kide confirm` if the deletions are wanted, `kide restore` to put the files back, or `kide reset` to take another device's state. `guard = off` turns the guard off for a folder only a program writes.

**Versions.** Each folder records its format in `.kide/format`, and devices agree on a protocol version when they connect. A device that meets an older or newer one stops with a message saying which side to update, and retries only every 5 minutes.

Files that a pull removes or replaces go to `.kide/trash/<unix-time>/`. Names a device cannot represent (for example `a` beside `A` on a case-insensitive file system) are held, not written, and reported on standard error. Set `KIDE_LOG=1` for more detail.

## Moves and snapshot undo

`kide -C ~/notes move old/ new/` records a whole-folder move, retaining every
file's history. Directory operands end in `/`; file renames use plain paths.
The command scans first, refuses occupied destinations, and publishes source
and destination entries together. `kide log new/file.txt` shows move operands.
Ordinary disk renames seen by scanning remain delete/create: identical content
alone never proves a move. Concurrent rename/edit preserves both contents using
the normal conflict rules; it does not silently redirect edits to a new path.

To undo a folder move or restore an earlier snapshot, first review the retained
root you want, then `kide -C ~/notes restore-tree <root> --replace`. This explicitly
replaces intervening edits with the snapshot's contents, keeps those edits in
history/trash, and publishes new forward versions for peers. HEAD never rewinds.
Changed paths during application are left and reported; a partial restore exits
unsuccessfully. Applications use `folder::moves::move_path` and
`folder::restore::restore_tree`, passing their reviewed current HEAD to restore.
Format 8 replaces format 7 without migrations; older folders are refused.

## Configuration

`.kide/config` holds one `key = value` per line; `#` starts a comment.

```
folder        = /path/dir   # the directory synchronised, if not this one
mode          = sync        # or send: scan and serve only, never write into the folder
ignore        = .obsidian   # neither scanned nor written here; repeatable; * matches within a name
sync          = .obsidian/*.json   # synced although an earlier ignore covers it; the last rule decides
transport     = quic        # default; or fips (experimental), plain (local tests)
self          = npub1… 192.0.2.10:7840 # this device
device        = npub1… 192.0.2.11:7840 <certificate-sha256> # another own device; repeatable
port          = 7839        # fips: the port on every device's FIPS address
scan_interval = 10          # seconds
pull_interval = 10          # seconds
trash_days    = 30
keep_days     = 30
guard         = on          # or off: scans record a lost or mostly deleted folder unasked
```

**QUIC transport** (the default). The program creates a device key and TLS
identity on `init`. `self = <npub> <address:port>` names this device;
`device = <npub> <address:port> <certificate-sha256>` names each peer. Exchange
keys and certificate hashes over a trusted channel. TLS verifies each peer's
pinned certificate, and the challenge admits only configured device keys.
`serve`, `pull`, `run`, `reset`, `join` and `verify --repair` all use the chosen
transport. Keep `.kide/device.key` and `.kide/tls.key` private. Pinned certificates
last 14 days: stop, run `kide renew-tls`, exchange the new certificate hash,
update peers' device lines and restart. See the root README for two machines.

**FIPS transport** (experimental; review C7 requires a two-machine run). Devices reach one another at the FIPS addresses of their keys, and a server accepts only connections from the configured devices' addresses; FIPS authenticates and encrypts every packet. This is sound only if **the host drops packets addressed to its own FIPS address that arrive on any interface other than the FIPS one, loopback included**: otherwise a packet with a forged FIPS source address could arrive over another network. The rule names the one address, not `fd00::/8`, which other networks such as Tailscale also use. With nftables, for an interface `fips0`:

```
table inet kide {
    chain input {
        type filter hook input priority filter; policy accept;
        ip6 daddr <own FIPS address> iifname != "fips0" drop
    }
}
```

The FIPS baseline firewall does not do this, and Kide cannot check it without privileges; `kide serve` and `kide run` print a warning at start-up with the rule for the device's own address and interface.

**Plain transport**, for local testing only. `self` and every `device` also carry an address and port, as in `device = npub1… [::1]:7840`. It is **unauthenticated**: anyone who can use a configured device's address can read the folder and send it changes.

## The library

`examples/two_folders.rs` is the shortest complete use: two folders, a scan, a pull. `cargo doc -p kide --open` documents every module; each names the § of `SPEC.md` it implements.

| Layer | Modules | What |
|---|---|---|
| Format | `format`, `hash`, `load`, `history`, `doc` | Chunking, nodes, version records, manifests (§2, §3, §5.1); reading them back; a path's history; a document with history |
| Interfaces | `iface` | The traits everything else implements: `ObjectStore`, `Workspace`, `Transport`, `Session`, `Live`, `Resolver`, `ChangeSource`, `Profile`. Each has a conformance suite in `conformance` (feature `conformance`) |
| Storage | `store`, `gc`, `file` | Object stores in memory and as packed segments on disk; garbage collection |
| Replication | `protocol`, `fetch`, `walk`, `auth`, `transport` | The protocol as pure state machines (§4.2); fetching a closure (§4.3); the key challenge; TCP, FIPS, QUIC/WebTransport (feature `quic`), HTTPS/WebSocket (feature `https`, [setup](HTTPS.md)), in-process and gateway transports; live sessions |
| Folders | `folder`, `workspace`, `change`, `names`, `resolve` | The folder profile (§5): configuration, scan, pull with its plan and review, apply, watcher; names on disk; the decision table and the text merge. The daemon and the commands are `../kide-cli/` (`kide-cli`) |

Rules the code keeps: the format is concrete and everything else sits behind a trait in `iface`; traits are blocking and use only the standard library (asynchronous code lives inside an implementation); errors keep `NotFound`, `Unavailable` and `Corrupt` apart and never read a missing object as a deletion (§4.4). Without the folder modules, the library builds for `wasm32-unknown-unknown`; `../kide-web/` (`kide-web`) adds the browser's WebTransport and OPFS.

Features: `watch` (default), `fips`, `quic`, `conformance`. `kide-cli` turns on `quic`, `fips` and `watch`.

## Development

```
nix develop
cargo test -p kide -p kide-cli
./crates/kide/ci.sh
```
