# Kide

Kide (Finnish: *crystal*) keeps folders equal across devices. It stores a folder as content-defined hash trees, so identical content has identical ids everywhere and an edit costs only the few chunks and nodes it touches. It keeps a version history per file, so concurrent edits are merged as text or kept side by side as conflict copies, never lost.

Kide is a Rust library with a small command-line program. It is for people who build sync into their own applications. It knows about folders, versions, objects, peers and device keys. It knows nothing about users, accounts or permissions, beyond the hooks an application fills in.

> **Status: pre-release.** The folder format is at version 8 and the protocol at version 8. Both can still change, without migrations; a device that meets another version refuses with a clear message. There is no stable API yet and nothing is on crates.io. Do not keep the only copy of anything in a Kide folder.

## What it does

- **Content-defined chunking and hash trees** (SHA-256): one inserted byte in 8 MiB of text costs under 10 KiB on the wire.
- **Per-file history without clocks or device ids**: version records with parent links. Concurrent records merge by a frontier rule, so any number of devices converge in any topology.
- **Moves and undo**: explicit file or folder moves retain version history; restoring an earlier tree creates a new forward change, with an explicit choice to replace intervening edits.
- **Text merge**: concurrent edits of a prose file (Markdown, plain text) merge line by line, three-way from the nearest common ancestor; overlapping changes are kept side by side. Other files, code and data included, keep conflict copies.
- **Folders as they are on disk**: plain files beside a content store in `.kide/`. Unicode normalisation, case-insensitive file systems and names a platform cannot represent are handled, never read as deletions. A guard refuses to send a lost or emptied folder as deletions.
- **Replication** over QUIC/WebTransport, authenticated HTTPS (TCP 443), plain TCP or [FIPS](https://github.com/jmcorgan/fips) (a mesh network whose addresses come from Nostr keys). Peers prove a secp256k1 (Nostr-format) device key with a challenge. Partial trees and read rights per path; a server can pull back through a device's own connection, for devices behind NAT.
- **Live sessions**: a small message channel next to pulls, for live documents and application state.
- **Documents with history** (`kide::doc`): a value kept as a version chain, for application state such as account records.
- **Browser**: the core builds for `wasm32-unknown-unknown`; `kide-web` syncs into the browser's private file system (OPFS) over WebTransport.
- **Hooks for applications**: admission and read rules per route, a review step between a pull's plan and its application, routes decided while serving.

## Where it is used

Kide is used by **Kota**, an application for folders shared between people.
Kota adds accounts and sign-in, groups, access lists, quotas and device enrolment.
Its server uses Kide's QUIC server with routes decided per request; its client
keeps many folders in sync with Kide's folder library, and its account records
are a `kide::doc`.

Kota pins Kide by commit. Anything a person syncing their own folders would
want, with no accounts at all, belongs in Kide, as a generic mechanism with a
hook. Anything that needs to know who a person is belongs in the application.

For networks that block UDP, the library's optional `https` feature supplies
TLS/WebSocket sync on TCP 443, including live control and reverse pulls.
See [HTTPS fallback](crates/kide/HTTPS.md) for the API and proxy setup.

## Quick start

You need [Rust](https://rustup.rs) 1.89 or later, or [Nix](https://nixos.org): `nix develop` gives the pinned toolchain, with the WebAssembly target and Chromium for the browser tests.

```
git clone https://github.com/k0sti/kide
cd kide
cargo run -p kide --example two_folders     # the library in one page: two folders, a scan, a pull
cargo build --release -p kide-cli           # the command-line program, target/release/kide
```

Two folders on two machines, authenticated with device keys over QUIC (UDP 7840).
On **each machine**:

```
./target/release/kide init ~/notes
# Prints this folder's public device key and TLS certificate SHA-256.
```

Exchange those two public values through a channel you trust. On machine A,
edit `~/notes/.kide/config` (reverse A and B on machine B):

```
self   = <A's npub> 192.0.2.10:7840
device = <B's npub> 192.0.2.11:7840 <B's certificate SHA-256>
```

Use the machines' reachable addresses and allow UDP 7840 through their firewalls.
Then run on each machine:

```
./target/release/kide -C ~/notes run
```

Edit either folder; changes sync both ways. TLS verifies the pinned certificate;
each peer proves its configured device key. Keep `.kide/device.key` and
`.kide/tls.key` private. `self` must match the key printed by `init`.

Pinned WebTransport certificates expire after 14 days. Stop the daemon, run
`kide -C ~/notes renew-tls`, exchange the new certificate SHA-256 with peers,
update their `device` lines and restart. The device key and file history stay.

FIPS is **experimental**, pending a run between two machines (review C7).
Plain TCP (`transport = plain`) is unauthenticated and for local testing only.

[`crates/kide/README.md`](crates/kide/README.md) is the user guide (commands, configuration, transports, the library's layers). [`crates/kide/SPEC.md`](crates/kide/SPEC.md) is the normative specification of the format and the protocol, with test vectors generated by an independent Python implementation.

## Build from source and test

```
cargo build -p kide -p kide-cli -p kide-git # the libraries and the program
cargo test -p kide -p kide-cli -p kide-git  # the light suite, a few seconds
```

The full suite needs the Nix shell: release-mode and heavy tests, clippy for native and `wasm32`, formatting, the Python reference and the browser tests in headless Chromium.

```
nix develop
cd crates/kide && ./ci.sh
```

## Repository layout

| Path | What |
|---|---|
| `crates/kide/` | The `kide` library, `SPEC.md`, and `reference/kide_ref.py`, the Python reference that generates the spec's test vectors |
| `crates/kide-cli/` | `kide-cli`: the `kide` program and its daemon |
| `crates/kide-git/` | `kide-git`: a folder linked to a Git branch (needs the `git` program) |
| `crates/kide-web/` | `kide-web`: Kide in the browser (WebTransport, OPFS) through wasm-bindgen; browser test support under `tests/` |
| `crates/kide/examples/` | `two_folders` (library basics) |

## Contributing and security

See [the specification](crates/kide/SPEC.md) for the implemented format and protocol. See [CONTRIBUTING.md](CONTRIBUTING.md) for the rules the code follows and the sign-off on commits. To report a vulnerability, see [SECURITY.md](SECURITY.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
