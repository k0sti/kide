# Security

Kide is pre-release software: there are no supported releases yet, and fixes go to `main`.

## Reporting a vulnerability

Please report vulnerabilities privately, by email to **security@atlantislabs.space**, not in a public issue. Say what you found, how to reproduce it, and which commit you looked at. Please allow time for investigation before public disclosure; include how you would like to be credited.

## Scope

Of most interest: the protocol and its key challenge (`crates/kide/src/protocol`, `crates/kide/src/auth.rs`), admission and read rights, the QUIC server and gateway (`crates/kide/src/transport`), the decoders of objects and messages, and the folder code that writes, replaces or deletes files on disk (`crates/kide/src/folder`).

Known limits, documented and not vulnerabilities in themselves:

- The `plain` transport is unauthenticated, by design, for trusted networks and tests.
- The FIPS transport is sound only with the host firewall rule in `crates/kide/README.md`.
- Objects are not encrypted at rest, and a server stores what devices send it in the clear.
