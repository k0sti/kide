# Design

Kide synchronises trees of data without a coordinator. The folder profile is
one use of that machinery; applications can provide other profiles through the
traits in `crates/kide/src/iface.rs`.

Content-defined chunk boundaries follow the content rather than fixed offsets.
An insertion therefore changes only nearby chunks. Canonical hash trees give
identical content identical SHA-256 object ids, so peers can skip subtrees they
already hold. Stores can deduplicate objects independently of filenames.

Each file has version records that name their parents and content. Parent links
establish causality without wall clocks, device identifiers or a central order.
Concurrent versions are combined by a deterministic frontier rule. Prose can
merge as text; other files retain conflict copies. Missing or unreachable
objects never imply deletion.

The protocol runs as a synchronous state machine behind transport and store
traits. Applications choose admission, read rights, partial views and review
policy. QUIC/WebTransport supplies native and browser connectivity; optional
HTTPS/WebSocket supplies TCP 443 sync, reverse pulls and live control when UDP
is blocked. TLS verifies the server and the same challenge admits devices. Plain TCP
is for local testing only. FIPS is experimental pending a two-machine run. Accounts, user interfaces and access
policy belong to applications such as Kota.

The API and formats are pre-release. Objects are not encrypted at rest, and
Kide is not a substitute for an independent backup. The normative rules,
protocol limits and test vectors are in [the specification](crates/kide/SPEC.md).
An independent [Python reference](crates/kide/reference/) checks the format.
