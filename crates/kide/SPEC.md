# Kide — Specification, folder format 8, protocol 8

Kide stores folders as content-defined hash trees and synchronises them between one user's own devices. This document is normative for v2 of the implementation, which implements **folder format 8** and **protocol 8**. The two are numbered apart since protocol 8: the **folder format** covers the objects (§2, §3) and the folder profile's rules (§5), and each folder records it (§5.4); the **protocol** covers the messages (§4.2), and Hello negotiates it. Until version 7 one number named both: the v1 prototype implements version 2; version 3 was an earlier draft of this one with W = 256, version 4 had no challenge in Hello, version 5 had no Fetch, and version 6 had no live sessions; format 8 adds explicit operation metadata to version records. Section numbers (§) refer to this document.

**Changing them.** A change to the objects or to a rule devices must apply alike (deciding a pull, §5.6) is a new folder format: devices that do not know a folder's format refuse that folder, with `Incompatible`, and keep syncing the others. A change to the messages is a new protocol version, or an optional **feature**: an implementation may speak a range of protocol versions, so devices upgrade one at a time. Within one folder format and one protocol version there is no backward compatibility. One rule changed with protocol 8 although the folder format did not: the text merge (§5.6) applies to prose only, where version 7 merged every path that may hold text. Devices of protocols 7 and 8 never sync with one another, so they never decide one frontier differently.

**Contents.** 1 Assumptions · 2 Core format · 3 History · 4 Replication · 5 Folder profile · 6 Live documents · 7 Derived data · 8 Test vectors · 9 Parameter status.

## 1. Assumptions

The format follows from these assumptions. Every rule below is either forced by them, a convention with one obvious choice, or the single tunable k.

| # | Assumption |
|---|---|
| A1 | Objects are addressed by SHA-256 of their bytes |
| A2 | Canonical ids: an id depends only on the content it stands for, never on how it was produced |
| A3 | Locality: a small edit changes O(log n) objects of O(S) bytes each, for content that has a natural cut (§2.2) within every 8·S bytes |
| A4 | No coordination: identical actions on different devices produce identical objects. Version records contain no clock and no device id |
| A5 | Every object has a bounded size |
| A6 | Chunks and nodes target one size S = 2^k; k is the only tunable |
| A7 | No field is derivable from other fields or from the object's length; each fact has one encoding |
| A8 | An object's kind is known from whatever refers to it |
| A9 | Content can be read at any byte offset without reading what precedes it |
| A10 | A device's record at a path only moves forward along that path's history |
| A11 | Renames are not detected: a rename is a deletion and a creation |
| A12 | Peers are authenticated by the transport (FIPS) or by a Nostr-key challenge over a transport that authenticates the server (§4.2); the plain transport is unauthenticated |

**Principle.** Identity is canonical, storage layout is free, and verification happens at object boundaries. A device may keep content in any form, as long as it can produce an object's exact bytes on request; bytes are valid when their SHA-256 equals the id they were requested by.

## 2. Core format

### 2.1 Conventions and objects

- `H` is SHA-256. Ids are 32 bytes, written as lowercase hex.
- Integers in objects are `u64` little-endian, except the node level (`u8`) and modification times (`i64` little-endian, nanoseconds since the Unix epoch). Frames (§4.2) have their own fixed fields.
- There are three kinds of object. None carries a type byte; the referrer determines the kind (A8). The same bytes may therefore be valid as two kinds; the context in which an id is used decides.

| Kind | Bytes | Referenced from |
|---|---|---|
| Chunk | Raw content bytes, at most `max` (manifest chunks: §5.1) | A level-1 node |
| Node | §2.3 | A version record (as content root), a node one level up, a profile root |
| Version record | §3.1 | A profile entry (§5.1), another version record (as parent) |

- **Parameters.** S = 2^k with **k = 10**. Derived: `min` = S/4 = 256, `max` = 8·S = 8,192, `b` = k − 5 = 5, `W` = 2^b = 32 (the largest number of children of a node). Chunks then average about 1.25·S bytes; a node holds at most W children (1,281 bytes, about 1.25·S) and on average about 20.

### 2.2 Chunking

**Gear table.** `GEAR[i] = u64::from_le_bytes(H([i])[0..8])` for i in 0..256, where `[i]` is the single byte i.

**Window hash.** `g(i)` is the value of `h` after `h = (h << 1) + GEAR[data[j]]` (wrapping) for j from max(0, i − 63) to i, starting from h = 0. Because each step shifts by one bit, `g(i)` depends only on the 64 bytes ending at i, and equals the running value of h over the whole input.

**Rule.** With `start` the offset after the previous cut (0 at first), a cut follows byte i when `i + 1 − start ≥ min` and either the top k bits of `g(i)` are zero (a *natural cut*) or `i + 1 − start = max` (a *forced cut*). The bytes after the last cut form a final chunk, which may be shorter than `min`. Empty content has no chunks.

```rust
fn cut_points(data: &[u8]) -> Vec<usize> {                 // end offset of every chunk
    let (mut cuts, mut start, mut h) = (Vec::new(), 0usize, 0u64);
    for (i, &b) in data.iter().enumerate() {
        h = (h << 1).wrapping_add(GEAR[b as usize]);
        let size = i + 1 - start;
        if size >= MIN && ((h >> (64 - K)) == 0 || size == MAX) {
            cuts.push(i + 1);
            start = i + 1;
        }
    }
    if start < data.len() { cuts.push(data.len()); }
    cuts
}
```

Implementations may stream; the output must equal this function's for every way of splitting the input.

### 2.3 Nodes

**Encoding.**

```
node  = level: u8 | child × n          (n = (length − 1) / 40)
child = id: [u8; 32] | bytes: u64
```

The children of a level-1 node are chunks; the children of a level-L node, L ≥ 2, are level L − 1 nodes. `bytes` is the length of the content below the child.

**Height.** `height(id)` = the number of leading zero bits of the id, read as a big-endian 256-bit number, divided by b and rounded down. The height of a node, as a child, is the height of the last chunk below it.

**Build.** From the chunk entries `(H(chunk), height, length)` of some content:

```rust
fn build(chunks: Vec<Entry>) -> Entry {                    // returns the root
    if chunks.is_empty() { return make_node(1, vec![]); }  // empty content
    let (mut items, mut level) = (chunks, 1u8);
    loop {
        let (mut next, mut group) = (Vec::new(), Vec::new());
        for e in items {
            let closes = e.height >= level as u32 || group.len() + 1 == W;
            group.push(e);
            if closes { next.push(make_node(level, std::mem::take(&mut group))); }
        }
        if !group.is_empty() { next.push(make_node(level, group)); }
        items = next;
        if items.len() == 1 { return items.pop().unwrap(); }  // invariant: the loop exits with one item
        level += 1;
    }
}
// make_node encodes and stores the node and returns
// Entry { id: H(encoding), height: the last child's height (0 if none), bytes: sum of the children's bytes }
```

A group closes after an entry whose height is at least the level, or after its W-th entry. The root of any content, even a single chunk, is a node. A level in which every entry closes its own group yields nodes with one child; they are valid.

**Validation.** A node has at least one byte, and `length − 1` is a multiple of 40; 1 ≤ `level`; 1 ≤ n ≤ W, except that a level-1 node may have n = 0 (empty content); every child's `bytes` is at least 1, and their sum fits a `u64`; when a child is available, the content below it is `bytes` long and, for a node, its level is `level − 1`. Validation checks encodings, not canonicity: a well-formed tree that the build would not produce is valid, and devices, which trust one another (§4.2), build canonically.

**Lookup.** The children's `bytes` lead from the root to the chunk holding any byte offset, one node per level (A9).

## 3. History

### 3.1 Version records

```
version record = parent_count: u64 | parent × parent_count | root: [u8; 32] | change_length: u64 | change | state
```

- `parent` ids are version-record ids, sorted ascending by byte order, without duplicates, at most W. A device's records at one path descend from one another (A10), so a frontier (§3.2) holds at most one element per device; a folder has at most W devices.
- `root` is the content root (a node). Thirty-two zero bytes mean *no content*: the record is a **tombstone**, its `state` is empty, and it has at least one parent.
- `state` is the rest of the object, at most S bytes, defined by the profile (§5.2) and opaque to the core, which only hashes and compares it.
- `change` is `change_length` bytes, at most MAX + 8 (8,200). It records an explicit operation, is opaque to the core, contains no object links and is not part of the value. Empty means an ordinary edit. A merge or conflict copy has empty change metadata; its parents retain the operations.
- **Change metadata (optional convention).** Records carry no time and no author (A4). A profile whose `state` is a UTF-8 JSON object may say who made a change, when and what it was with the keys `at` (milliseconds since the Unix epoch), `by` (the author: a key in hex, or a name the application gives) and `what` (a short description). Such a profile gives up A4 for its records: the same change made by two authors, or at two times, gives two records. The core reads none of the keys; tools such as an inspector show them. Other keys belong to the application.
- The **value** of a record is the pair (root, state). Records with equal value, parents and change metadata are the same record.
- **Validation:** the object is at least 48 + 32 · `parent_count` + `change_length` bytes, and the rules above hold. When its parents are available (§4.3 brings them), no parent of a merge record is a merge record.

| Event | Record |
|---|---|
| A path seen for the first time | (value, no parents) |
| An edit or a change of profile state | (new value, [current version]) |
| A deletion | (tombstone, [current version]) |
| Re-creation at a deleted path | (value, [tombstone]) |
| Merge of a frontier F (§3.2) | (merge value, F sorted) |
| Conflict copy of a frontier element x | (x's value, [x]) |

### 3.2 Leaves, frontier and merge

"A is an ancestor of B" means A is reachable from B by following parents.

- A **merge record** has two or more parents; its parents are never merge records.
- The **leaves** of a record are its parents if it is a merge record, and the record itself otherwise.
- The **frontier** of two records L and R of one path is the set of elements of (leaves of L) ∪ (leaves of R) that are not an ancestor of another element of that set.
- If the frontier has one element, it is L or R, and no merge is made. Otherwise the merge is the record (v, frontier sorted), where the profile derives the value v from the frontier alone, or from data that all devices converge on (§6.1).

The frontier is symmetric in L and R, and the merge depends only on the frontier, so every device that has seen the same concurrent edits makes the same merge record, whatever the topology.

**Ancestor search.** Breadth-first from the younger record over parents, with a visited set, entirely in the local store (§4.3 brings all history).

**Pruning.** History is never pruned: a version record is removed only when no root reaches it, directly or as history of a record that a root reaches (§4.1). Tombstones stay in manifests, so their histories stay too.

## 4. Replication

### 4.1 Store rules

- Objects are never modified. A received object is stored only after its id has been checked and it has been validated by kind (§2.3, §3.1, §5.1, §5.2). Validation by kind includes what the referrer states about the object: a child node's level (`level` − 1) and content length (`bytes`), a chunk's length, and, for the parents of a merge record, that none is a merge record.
- The **content closure** of a node is the node and everything below it. The content closure of a version record is the record and the content closure of its root. The **history** of a version record is all its ancestors, as records only.
- **Invariants.** A node is stored only if its content closure is stored. A version record is stored only if all its ancestors are. A version record is **complete** if it is stored, it is a tombstone or its root is stored, and, if it is a merge record, all its leaves are complete; wanted with its whole history's content, it is complete if it and every record it descends from are stored, each a tombstone or with its root stored.
- A profile names its **roots**. A profile writes a root only after every object it needs is stored and durable.
- **Links.** A profile says how its roots are wanted and, for each of its own objects, which objects it links to: each link has an id, how the object is wanted, and optionally a **label**. An object is wanted as one of the profile's own objects, as content whose chunks are the profile's own objects, as a version's content (its chunks link to nothing), or as a version record: as history only, with its content, with its **whole history's content** (every record it descends from with its content too), or with its content as the profile's own objects (content whose chunks are the profile's own objects: a checkpoint of the profile's data). Each chunk is read on its own, so content whose chunks are the profile's own objects is one chunk or cut where objects end, as a manifest is (§5.1). Every walk follows the same links: fetching (§4.3) and garbage collection. A profile validates its own objects; the core validates nodes, chunks and records. The **path** of an object on a walk is the labels of the links followed from the root, joined by `/`; an unlabelled link keeps its parent's path. The root's path is empty.
- **Partial trees.** A device may hold a profile's tree only in part, where its application chooses so (an area of interest, a time range); the folder profile never does. In a partial tree a stored object's children may be absent, and the invariants above do not hold. A walk over a partial tree descends into stored objects too, follows a labelled link only if a filter of the application accepts its path, leaves an object the server answers Missing for absent, and may stop at any time, storing what it fetched children first. Garbage collection over partial trees skips absent objects, so an object reachable only through an absent one is removed. An absent object is never a deletion (§4.4).
- **Garbage collection** keeps, for every root, what the profile names as needed (§5.7) and removes every other object. The history of every version record a root reaches is kept with it; a record that no root reaches in any way is removed like any other object.
- **Corruption.** A store that reads an object whose bytes do not hash to its id answers `Corrupt` and, if it may write, **drops** the object: from then on the store lacks it, so it can be stored again, and a fetch brings it again. Objects that link to it are then stored without their whole closure, against the invariants above, which a whole walk trusts. So the store also marks itself **damaged**, and a **repair** walks the closure of the roots it keeps as a partial walk with every path accepted, fetching whatever is absent from a peer, and then clears the mark. What no peer has stays absent, and whatever needs it fails with `NotFound`.

### 4.2 Transport, trust and framing

- One TCP connection per session: a pull, or a live session (below). The connecting device (client) pulls from the listening one (server). Bidirectional sync means each device pulls from the other.
- **FIPS transport** (experimental until tested between two machines; review C7). Kide uses FIPS only as an authenticated IPv6 network, through ordinary sockets on its interface; the FIPS native datagram API is not used. A server listens on its own FIPS address and accepts a connection if and only if the source address is the FIPS address of a configured device (§5.8). The FIPS address of a Nostr public key P is `0xfd ‖ H(P as 32-byte x-only key)[0..15]`. FIPS authenticates every packet with a Noise XK session and rebuilds the source address from that session, so a connection from such an address over FIPS proves that the peer holds the key. A host **must** drop packets addressed to its own FIPS address that arrive on any interface other than the FIPS one, loopback included. FIPS addresses fill all of `fd00::/8`, which other networks also use (Tailscale's `fd7a:115c:a1e0::/48`, for example), so the rule names the one address, not the range; it matches what FIPS's own DNS responder does. The FIPS baseline firewall polices only the FIPS interface and does not do this. With nftables, for an interface `fips0`:

  ```
  table inet kide {
      chain input {
          type filter hook input priority filter; policy accept;
          ip6 daddr <own FIPS address> iifname != "fips0" drop
      }
  }
  ```

  Without the rule, a host that accepts packets for a local address on any interface could receive a forged source over another network, although the TCP handshake could not complete, because the reply travels over FIPS to the real device. Kide adds no handshake and no encryption of its own.
- **Challenge.** Where the transport does not identify the client, the server identifies it by a Nostr key: its Hello carries a random, non-zero 32-byte `challenge`, and the client answers with Auth before any request: its x-only public key and the BIP-340 Schnorr signature by that key of the 32-byte message `H("kide auth" ‖ 0x00 ‖ name ‖ 0x00 ‖ challenge)`, where `name` is what the client knows the server by (its host name, or its address and port; UTF-8 without NUL) and the auxiliary randomness may be zero. The server accepts the answer if it verifies for one of its own names and it admits the key; otherwise it fails the session with `Invalid`. Signing the name keeps a server from relaying another server's challenge, so the challenge is sound only over a transport that authenticates the server (TLS). A server that challenges no one sends a zero challenge, and a client always sends one; a client without a key fails with `Invalid` and sends Bye when challenged. A request before the answer, or an Auth that was not asked for, is `Protocol`.
- **QUIC transport,** as WebTransport over HTTP/3, so that browsers can use it too. A peer is an `https://host:port` URL, with a path where the server serves several services on one port: each path has its own store, root, admission, read rights and live sessions, and a session to a path the server does not serve is refused before Hello, which the client sees as `Unavailable`. A session, a pull or a live session, is one WebTransport session with one bidirectional stream, which carries the frames as a TCP connection would. TLS authenticates the server by its certificate; the client signs the URL's host as the server's name, whatever the path: the services on a port share its certificate, and the challenge is fresh for every session. A browser client answers the challenge the same way, with a key its application keeps. A server that sets an admission rule challenges every session; otherwise it serves anyone who reaches it.
- **HTTPS fallback (TCP 443).** When UDP is unavailable, an application may use a TLS-authenticated WebSocket at the same `https://host[:port]/path` (443 by default), with HTTP/1.1 Upgrade and the `kide-sync-v1` WebSocket subprotocol. The server certificate is verified against application trust roots or an explicitly exchanged SHA-256 DER certificate pin; the challenge signs the URL host just as with QUIC. There is no plaintext retry. Routing, admission, read rights, protocol frames and live control are unchanged. The application chooses when to retry QUIC and reports the selected transport; an authentication, format, protocol or corruption failure is never treated as permission to weaken trust. TLS may terminate at a trusted reverse proxy that forwards the WebSocket to a TLS backend; its admission names the external host. The proxy must preserve Upgrade, path, subprotocol and binary messages, and allow long-lived connections.
  Each binary WebSocket message is `tag: u8 | stream: u64 little-endian | body`. Stream 0 is the primary pull/live session and exists from Upgrade. Only the server opens additional streams: tag 0, a strictly increasing nonzero identifier and a UTF-8 folder path of at most 4,096 bytes (no length field; the WebSocket message bounds it). Tag 1 carries 1 to 65,536 bytes of the ordinary protocol byte stream; protocol frames may span messages. Tag 2 ends a stream with an empty body. Tag 3 refuses/aborts a stream with a UTF-8 reason of at most 4,096 bytes, seen as `Unavailable`. Data for a never-opened stream, a reused/out-of-order open identifier, malformed envelope or text message ends the connection; late data for a closed stream is discarded. Standard WebSocket Ping/Pong/Close are allowed. These envelopes belong to the transport, so folder format and protocol versions stay unchanged. A connection permits at most 64 concurrent byte streams (one primary plus 63 reverse pulls); inbound queues and the shared outgoing queue each hold at most 16 messages, with backpressure and the same 30-second read/write limits. Ending the primary session ends the connection and all reverse pulls. The server registers a device for reverse pulls only after its live session has proved an admitted key; replacing that device's connection cannot let the older session remove its replacement.
- **Pulls back.** A device that cannot be reached (behind NAT) may let the server pull from it through its own connection: it opens a live session, in which it proves its key by the challenge, and keeps it open; the server may then open further bidirectional streams on that connection. Each such stream starts with the path of a folder the device serves (`length: u16` little-endian, at most 4,096, then the path in UTF-8), after which the stream carries a pull as in a session of its own, with the device as the server: it sends Hello without a challenge (TLS already authenticated the server it connected to) and serves that folder, or ends the stream if it serves no folder there. The device decides what it serves by path; the server knows the device by the key of the live session. Peers that do not ask for pulls back never see a server-opened stream, so the protocol number stays. A gateway relays one stream per session and does not carry pulls back.
- **Gateway.** The services on one QUIC port may run as separate processes behind a **gateway**, which owns the port and the certificate and relays each session by its URL path to the service's own server over TCP, copying the stream's bytes both ways unread; a path it has no route for, or whose server it cannot reach, is refused before Hello as above. Hello, the challenge and everything after run end to end, so such a server identifies every connection by the challenge alone: it requires an admission rule, accepts connections from any address, and listens where only the gateway reaches it (on loopback). Its admission names the host of the gateway's URL, which its clients sign. TLS, which keeps a challenge from being relayed (above), ends at the gateway, which the service trusts as it trusts its own host.
- **Read rights.** A server may serve each peer only what that peer may read. The application supplies a rule that answers whether a peer may read the object at a path (§4.1). The peer is the key proved by the challenge, or else the configured device whose address the connection comes from (FIPS and plain transports), or else nobody. A session is granted the root it was sent in Hello. An object granted under a path grants each object it links to, under that link's path, if the link is unlabelled or the rule accepts the path; rights therefore only narrow along a path. An object reached through several paths is readable if any of them is. Grants follow the profile's links from the root however the client reached an object, so a client that holds the top of a tree from an earlier session may still ask for what lies below it. The server answers Missing for an id the session has not been granted, as for one it lacks; read rights need no message of their own. The ids in a readable object are visible to the peer, including those of children it may not read, and so are the labels of its links: whether names stay private depends on how an application lays out its tree.
- **Limits.** A server serves at most 1,024 sessions at once and refuses a connection beyond that before Hello, which the client sees as `Unavailable`; a session that has not opened its stream within the read limit is dropped, and so is a gateway's relay once one direction has ended and the other stays silent for the read limit. A client fails an answer to Fetch that pushes more than the limit of §4.3 allows with `Protocol`. A live session holds at most 1,024 streams at the server in any state but closed; a LiveOpen beyond that is `Protocol`. Under read rights, widening the grants for a requested id never reads a content chunk, since it links to nothing.
- **Plain transport,** for local testing only. The server accepts a connection only if the source address is a configured device's address (§5.8). It is **unauthenticated**: whoever can use such an address can read the folder and serve it changes.
- **Frame:** `length: u32` (little-endian) `| type: u8 | payload`, where `length` counts the type byte and the payload; at most 2^(2k+2) bytes (4 MiB at k = 10). A read that waits more than 30 seconds fails with `Unavailable`, also a server's read of the next request; a connection that ends inside a frame has broken and is `Unavailable` too, while a frame over the limit or with a malformed payload is `Protocol`. The server answers requests in order; a client may send further requests before it has read the answers to earlier ones.

| Type | Message | Direction | Payload |
|---|---|---|---|
| `0x01` | Hello | Both, once at the start | `high: u8` (= 8), `low: u8` (= 8), `features: u64`, `format: u16`, `root: [u8; 32]`, `challenge: [u8; 32]` |
| `0x02` | Get | Client → server | `id × n` (1 ≤ n ≤ 4,096) |
| `0x03` | Object | Server → client | `id`, then the object's bytes |
| `0x04` | Missing | Server → client | `id` |
| `0x05` | End | Server → client | Ends the answer to one Get or GetChildren |
| `0x06` | Bye | Client → server | Closes the session |
| `0x07` | GetChildren | Client → server | `node id × n` (1 ≤ n ≤ 4,096), level-1 nodes |
| `0x08` | Children | Server → client | `node id`, then the node's chunks concatenated in node order |
| `0x09` | Auth | Client → server, once, after a non-zero challenge | `key: [u8; 32]`, `signature: [u8; 64]` |
| `0x0A` | Fetch | Client → server | `want: [u8; 32]`, `count: u32` (≤ 4,096), `have: id × count`, then `scope`: the rest of the payload |
| `0x10` | LiveOpen | Both, in a live session | `name_len: u16`, `name` (UTF-8), then `version`: the rest of the payload |
| `0x11` | LiveUpdate | Both, in a live session | `name_len: u16`, `name` (UTF-8), then `bytes`: the rest of the payload |
| `0x12` | LiveClose | Both, in a live session | `name` (UTF-8): the whole payload |
| `0x13` | Ping | Both, in a live session | empty |

The server answers a Get with one Object or Missing per id, in request order, then End; a GetChildren with one Children or Missing per node, in request order, then End. It sends Children only if it holds the object, the object is a valid level-1 node, and it holds all its chunks; otherwise, including for an id that is not a level-1 node, it sends Missing. It answers a Fetch with any number of Objects, then End (§4.3); a server that knows no profile for its root sends End alone. The client splits a Children payload by the `bytes` of the node, which it holds, and checks every chunk; a wrong total or one mismatch makes the whole answer for that node `Corrupt`. Without read rights, a server serves any object in its store.

**Versions.** Hello's layout is fixed from protocol 8 on. `low` and `high` are the lowest and highest protocol versions the sender speaks; a session uses the highest version both speak. `features` are optional capabilities within a version, little-endian: a session uses those both sides set, and a side ignores bits it does not know; none is defined yet. `format` is the folder format of the sender's root (§5.4; 8 for every root this document describes). The client fails with `Incompatible`, and sends Bye, if the two sides share no protocol version or their formats differ; the message says which side is older. A Hello of 65 bytes is that of protocol 7 and earlier (`protocol: u8`, `root`, `challenge`): it is read as `low` = `high` = `protocol`, no features and format 0, so a client of protocol 8 refuses such a server with a message instead of a malformed frame. The server answers Hello whatever the client's versions are; the client decides. A daemon or application that gets `Incompatible` does not retry as for `Unavailable`: nothing changes until one side is updated (§5.8).

**Live sessions.** A session is a pull or a live session; its first message after Hello (and Auth) decides: a request makes it a pull, LiveOpen or Ping a live session, and a message of the other kind later is `Protocol`. A live session carries named streams of an application's bytes in both directions at once; Kide interprets neither a stream's `version` nor its updates, and its name is a path (§4.1). The client opens a stream with LiveOpen and its version; the server accepts it with LiveOpen and its own version, or refuses it with LiveClose. With read rights, the server refuses a name the peer may not read as a path (read rights, above) before the application sees it; what an update means, and whether to accept it, is the application's. Once a stream is open, either side sends LiveUpdates on it, delivered in order, and either side closes it with LiveClose, which the other answers with LiveClose unless it has sent one itself; a side that has sent LiveClose drops the updates that arrive before the answer. Opening a stream that is not closed, updating or closing one that is not open, and closing one before the server answered its open, are `Protocol`. Ping carries nothing and asks for nothing: a side that has sent nothing for 10 seconds sends it, so the read limit ends only a session whose peer is gone. Each side, on opening, sends the updates the other lacks by its version, and afterwards each forwards its changes as they happen. Sending never waits: each side queues at most 4 MiB of frames it has not yet written, and a send that would queue more ends the session with `Unavailable`. The client then opens a new session and reopens its streams at its versions, and the catch-up replaces what was lost; nothing is dropped silently. A side that cannot keep up with what it receives stops reading, so its peer's queue fills. The client ends a live session with Bye once what it queued is sent; either side may end it by closing the connection.

### 4.3 Fetching a closure

The client fetches the closure of the server's root: what the profile's links reach (§4.1), plus the history of every version record in it.

```
fetch(root):
    todo = [(root, wanted as the profile says)]
    seen = {}                                           // (id, how it is wanted) already walked
    while todo is not empty:
        todo = [x in todo if (x, how x is wanted) is not in seen]; add them to seen
        need = [x in todo if x is not stored,
                or x is a version record wanted with content that is not complete]
        request = [x in need if x is neither stored nor staged]
        Get(request) in batches of 1 to 4,096: check H(bytes) = id, validate by kind, stage
        whole = [x in need if x is a level-1 node with at least one chunk, none of its chunks
                 is stored, staged or a chunk of another node in whole, and its chunk ids are distinct]
        GetChildren(whole) in batches of 1 to 4,096: split, check every chunk, stage
        todo = the children of every node in need, typed by context,
               plus, for every version record in need:
                   its parents, wanted as history,
                       or with their whole history's content if it is wanted so,
                   and, if it is wanted with content: its root, as a version's content
                       or as the profile's own objects, as the record is wanted,
                   and, if it is also a merge record: its leaves, wanted alike,
               plus the links the profile finds in every one of its own objects in need,
                   wanted as each link says
    move every staged object into the store after everything it references   // e.g. post-order
```

A version record is wanted *with content* when the profile names it or it is a leaf of a merge record wanted with content, and with its whole history's content when the profile names it so or it is a parent of a record wanted so; otherwise it is wanted *as history*, and is complete once stored. A merge's leaves are its parents, so they come with their content, wanted as the merge's is (§4.1, §5.7). The link from a record to its root is unlabelled, so the labels of a checkpoint's objects continue the record's path: filters, scopes and read rights treat an older checkpoint's objects as they treat the newest.

An object references its children, its root, its parents and, for the profile's own objects, what they link to. Moving each staged object after everything it references keeps the invariants of §4.1 at every moment; a post-order of the walked graph does this. The Gets of one round do not depend on one another, so a client may send them all before it reads their answers; that round's GetChildren follow once they are answered. A Missing answer to Get is `NotFound`. A Missing answer to GetChildren is not an error: that node's chunks are among the next round's `todo` and come by Get. A client never sends an empty request.

**Fetch.** Before the first round, the client may send one Fetch with `want` its root, `have` up to 4,096 roots of the same profile whose closures it holds (its own current root, for example) and `scope` empty or an application's bytes. The server answers with what `want` reaches and the `have`s do not, as Objects, then End. It walks `want` and the `have`s side by side, one layer at a time along the profile's links (§4.1): before each layer of `want`, it marks the next layer of the `have`s, never descends on either side into an id that both have reached, and pushes each object of `want`'s layer that the `have`s have not reached, once. It follows only links whose path is within the scope, as the application's scope rule decides (an empty scope is everything), and, with read rights, only what the session is granted (§4.2). It skips objects it lacks or that do not validate, and stops before the object that would take the pushed bytes past 16 MiB, unless it is the first. The client checks each pushed object's id against its bytes (a mismatch is `Corrupt`) and stages it; it validates the object when the walk reaches it, as if it had come by Get, and an object the walk never reaches is discarded with staging. Fetch only saves round trips: whatever the push left out, by its limit, its scope, or because a `have` sits at another depth, the walk fetches as usual, and an object pushed although the client held it costs bytes, never correctness. The folder profile sends `Fetch(remote root, [HEAD, then the 63 newest retained roots of .kide/roots not already named], empty)` (§5.4): the server can walk only the `have`s it holds, and in two-way sync the client's `HEAD` is usually newer than any root the server has seen, while a root it pulled earlier is not.

Over a partial tree (§4.1) the walk is the same, except that `need` is every object walked, stored or not, that only links whose path the filter accepts are followed, and that a Missing answer to Get leaves that object absent; an absent object is not asked for again in the same walk.

Staged objects live in a separate store that survives an interrupted pull; the next pull uses them without fetching them again, while still walking their children. A node with any chunk already staged or stored is not whole, so a partly received node falls back to Get. Every answer fits one frame: a Children answer is at most 33 + W · (`max` + 4,144) = 394,785 bytes (§5.1), well below the frame limit, which leaves room for live updates (§4.2).

### 4.4 Errors

Every operation fails with exactly one of these classes, and they are never converted into one another:

| Class | Meaning | Response |
|---|---|---|
| `NotFound` | The source does not have the object | Look elsewhere |
| `Unavailable` | The source cannot be reached now (refused, reset, timed out) | Retry later; change nothing |
| `Corrupt` | Bytes do not match their id | Discard them; distrust the source |
| `Invalid` | Bytes or input break a rule of this spec; a protocol mismatch | Reject |
| `Protocol` | A malformed frame or an unexpected message | Close the session |
| `Io` | A local file-system failure | Report |
| `Incompatible` | The peer shares no protocol version, or its root has another folder format (§4.2); a folder of another format (§5.4) | Do not retry until one side is updated |
| `Refused` | A guard of the user's data holds (§5.5) | Change nothing; wait for the user |

Any of these ends a pull before reconciliation: staged objects remain, and nothing else changes. A missing or unreachable object is never taken for a deletion; deletions travel only as tombstones.

## 5. Folder profile

The folder profile keeps an ordinary folder equal on the user's devices.

### 5.1 Manifest

A folder's state is a **manifest**: its entries sorted by path in byte order, concatenated.

```
entry = path: UTF-8 bytes | 0x00 | version: [u8; 32] | size: u64 | mtime: i64
```

- `path` is the full path relative to the folder: components (§5.3) joined by `/`, plus one trailing `/` for an empty directory; at most 4,096 bytes, the trailing `/` included.
- A **file** has the id of its current version record, the size of its content, and its synchronised modification time (§5.5).
- A **symbolic link** has the id of its current version record, the length of its target, and `mtime` 0: a link's own modification time is not synchronised.
- A **deleted file** keeps its entry: `version` is its tombstone record, `size` and `mtime` are 0.
- An **empty directory** has a path ending in `/`, a version record whose root is the root of empty content and whose state is the directory kind (§5.2), and `size` and `mtime` 0. A directory that holds anything, even only names that scanning skips (§5.5), has no entry of its own; such a directory is neither listed nor synchronised. When an empty directory gains content or is removed, its entry becomes a tombstone.
- Paths are unique. Byte order makes every directory's entries contiguous.

**Manifest content.** The manifest is stored as content (§2.2, §2.3) with one change to cutting: when the rule of §2.2 decides a cut after byte i, the cut is placed at the end of the entry that holds byte i, `start` is set there, and the search for the next cut continues from there. The window hash covers every byte, so it needs no adjustment.

```rust
fn manifest_cut_points(data: &[u8], entry_ends: &[usize]) -> Vec<usize> {
    let (mut cuts, mut start, mut h, mut e) = (Vec::new(), 0usize, 0u64, 0usize);
    for (i, &b) in data.iter().enumerate() {
        h = (h << 1).wrapping_add(GEAR[b as usize]);
        if i < start { continue; }                          // rest of an entry already cut after
        let size = i + 1 - start;
        if size >= MIN && ((h >> (64 - K)) == 0 || size == MAX) {
            while entry_ends[e] <= i { e += 1; }            // the entry holding byte i
            cuts.push(entry_ends[e]);
            start = entry_ends[e];
        }
    }
    if start < data.len() { cuts.push(data.len()); }
    cuts
}
```

Every manifest chunk therefore begins and ends at an entry boundary and decodes on its own. An entry is at most 4,145 bytes, so a manifest chunk is at most `max` + 4,144 bytes. Below a manifest root, every chunk is a manifest chunk (A8).

**Validation.** Of each manifest chunk: its entries decode completely, and their paths are valid (§5.3) and strictly ascending. Of a whole manifest, before its root is written: paths are strictly ascending across chunks; tombstone and directory entries have `size` and `mtime` 0, and symbolic-link entries have `mtime` 0; a live entry's path ends in `/` exactly when its record's state is the directory kind; a live entry's `size` equals the length of its record's content; and no live entry's path begins with `p/`, where `p` is the path of a live file (`a` with `a/x` or `a/`), or with the path of a live empty directory other than its own (`d/` with `d/x`). This needs every record named by an entry, and the root node of every live record, in the store; a missing one fails with `NotFound`.

**Root.** The folder's root is the content root of its manifest. An empty folder has the root of empty content. There is no history of manifests; a device may keep earlier roots as additional garbage-collection roots (§5.7).

### 5.2 Version state

The `state` of a live record is one byte: `0` regular file, `1` executable regular file, `2` symbolic link, `3` directory. A symbolic link's content is its target, as bytes. A directory's content is empty. Nothing else about a file is synchronised: no owner, group, other permission bits, access or change times, extended attributes or access-control lists.

**Explicit operations (format 8).** A folder record with non-empty `change`
has exactly one parent. The folder profile's non-empty `change` is:
`1 | from_length: u16 | from UTF-8 | to_length: u16 | to UTF-8` for a move,
or `2 | earlier_manifest_root: [u8; 32]` for a forward restore. Integers are
little-endian; paths obey §5.3, differ, and either both end in `/` or neither
does. Directory operands are prefixes; descendants share the same operation.
A restore's manifest hash is provenance, not a link or a promise to retain that
manifest forever. Unknown tags, malformed lengths or trailing bytes are invalid.

**Move.** A caller first scans, then explicitly names a source and unused
destination in one folder. No content-hash or inode guessing identifies moves.
Refuse overlap, a destination with live entries or disk content, held/ignored
sources, invalid paths, and changes to the scanned source. A whole-directory
rename moves its disk tree together. Each live source entry becomes a tombstone
after its current record; each corresponding destination gets a record with the
same value, the source record as parent, and the move metadata. A destination
with deleted history retains both the source and deleted-destination record as
parents of an intermediate join, sorted and deduplicated. The final move is a
single-parent child of that join, so ordinary merge flattening never erases the
move event. If either input is itself a merge, a single-parent
continuation of its value (empty change metadata) bridges it first, so no merge
record is directly a parent of a merge record.
All source/destination records and descendants publish in one manifest HEAD.
A durable local move intent contains old/new HEAD, operands and the next index,
protected together by a SHA-256 checksum;
it precedes the disk rename. Opening writable finishes an interrupted intent,
refusing ambiguous disk state rather than guessing. Read-only opens keep seeing
the last published HEAD. Stores are durable before the intent and HEAD.

**Concurrency.** Path reconciliation remains §5.6. Concurrent rename/edit retains
the edit in the old path's history/conflict copy and the moved value at the new
path; edits are not silently redirected. Concurrent moves retain both named
destinations. Collisions use the existing deterministic conflict-copy rules.
History exposes explicit moves, their operands and each entry's original parents;
identical copies neither gain another path's later edits nor acquire move lineage.

**Forward restore.** Restoring an earlier manifest is an explicit replacement of
the current snapshot, guarded by the caller's expected current HEAD. It refuses
unscanned changes, held/ignored affected paths and missing/corrupt wanted objects
before writing. Each changed path gets a new single-parent record after its
current version (or the earlier version if absent); removed paths get tombstones.
Live restored records carry restore provenance. HEAD advances to this new tree;
it is never reset to the earlier root. Intervening edits stay in history and in
trash when replaced. The caller must explicitly accept replacing intervening
changes (CLI `restore-tree <root> --replace`); stale expectations fail. Disk races
while applying preserve the raced path and are reported as skipped changes,
never overwritten or treated as deletion. A caller must review skips before
claiming the entire earlier snapshot restored. Cross-folder and ownership moves
are outside this API.

### 5.3 Names

- A path component is valid UTF-8, 1 to 255 bytes, without `/` or the zero byte, and not `.` or `..`. Case is preserved; components are compared by bytes. Reserved, and therefore invalid: a top-level component equal to `.kide`, and any component ending in `.kide-tmp`, both in any ASCII case.
- Names are in Unicode Normalization Form C: scanning produces it (below). Decoders do not check it, so whether a manifest is valid never depends on an implementation's Unicode version.
- **Scanning** converts every name to NFC. Where the file system distinguishes normalisation forms, the file is renamed on disk to its NFC name; if that name is taken, or the folder is a send folder (§5.8), the file is skipped and reported. Names that are not valid UTF-8, or otherwise invalid, are skipped and reported.
- The **platform key** of a component c is NFC(simple case folding(NFC(c))), with trailing dots and spaces removed. Two live entries **collide** when, at the first component in which their paths differ by bytes, the two components have equal keys (the trailing `/` of a directory entry is ignored): `a` collides with `A`, with `A/x` and with `A/`; `a/x` and `a/y` do not. The format allows collisions. The platform key decides only what a device holds and never enters an object, so devices may use different Unicode versions for it.
- A device **holds** an entry it cannot represent on disk. On a file system that conflates keys, it first shows the live entries already on disk; two of those can collide only if the file system does not in fact conflate keys, and then both stay shown and the mismatch is reported. It then goes through the other live entries in byte order of their paths, holding every entry that collides with one it shows. Which entries a device holds is local and never enters an object. It also holds every entry whose name the platform forbids; such entries take no part in collisions. On Windows, the forbidden names are those with the characters `< > : " \ | ? *` and bytes 0x01 to 0x1F, the stems CON, PRN, AUX, NUL, COM1 to COM9 and LPT1 to LPT9 with any extension, names ending in a dot or space, and paths longer than the platform limit. A held entry stays in the manifest, is reported, and its absence on disk is never read as a deletion. When an entry stops being held, after a scan or a pull, it is written to disk (§5.6, applying step 3) before it is next checked against the disk.
- **Reporting.** Skipped names and held entries are reported the same way: on standard error once per path while the program runs, whether or not logging is enabled, and in any status output. A device never renames a file to resolve a collision.

### 5.4 Local state

```
<dir>/.kide/
  config       configuration (§5.8)
  format       the folder format, in decimal: 8
  HEAD         hex of the current root
  index        stat cache: path → version, size, mtime, inode, synchronised mtime, checked time
  move-intent  pending explicit move (§5.2), removed after publishing
  roots        retained earlier roots, one per line: unix time and root hex (§5.7)
  lock         exclusive lock held by every command that changes state
  confirm      hex of the root at which the deletions a guarded scan refused are confirmed (§5.5)
  objects/     the object store; objects/damaged marks a store that dropped a corrupt object (§4.1)
  staging/     objects of an unfinished pull
  trash/<unix-time>/…   files removed or replaced by sync
```

A folder whose `format` names another format than the program's fails to open with `Incompatible`. A missing format marker also fails with `Incompatible`; there is no migration or fallback.

The synchronised folder is `<dir>` itself, or the directory named by `folder` in the configuration (§5.8), which then holds no Kide state.

An index entry is **racy** if it was checked less than 2 seconds after the observed modification time; a racy entry is never trusted as a cache key. This holds for links and directories too, since a deleted link's inode number may be reused at once. The index and `HEAD` are rewritten whole, via a temporary file and a rename. Every time `HEAD` is replaced, the replaced root is appended to `roots` with the current time; a torn last line is ignored and cut off before the next append, and garbage collection rewrites the file whole without the expired lines.

**Platforms.** The folder runs on unix and Windows. On Windows a file has no inode (a change is told by size and mtime), no executable bit (files are written and scanned as regular), and making a symbolic link needs the right to (Developer Mode or an administrator); a link that cannot be made fails to apply like any write.

### 5.5 Scanning

1. Walk the folder. Skip the reserved names (§5.3), special files and other invalid names. Record empty directories: those that hold nothing at all.
2. For each file and symbolic link, note the time, then read its metadata. If the index holds an entry with the same size, mtime, inode and kind, and the entry is not racy, keep it.
3. Otherwise chunk the content as a stream, build its tree and store the new objects. Read the metadata again; if it changed meanwhile, keep the old entry and retry on the next scan.
4. If the new value equals the value of the entry's version, update the stat fields; if the observed mtime differs from the recorded one, set the synchronised mtime to it. A touch thus never creates a version.
5. Otherwise create a version record (§3.1) and set the synchronised mtime to the observed one.
6. For every live entry whose file is gone, and every empty-directory entry whose directory is gone or no longer empty, create a tombstone. Held entries (§5.3) are not checked against the disk.
7. Build the manifest from the index, store its content, sync the store and write `HEAD`.

**Guard.** A folder that is unmounted, emptied by another program or half lost is not a deletion the user made, and a scan must not record it as one: the tombstones would travel to every device. So a scan is **guarded**: it fails `Refused` and changes nothing (no index, no `HEAD`) when

- the synchronised directory does not exist or is not a directory; or
- step 6 would leave no live entry with a file on record, when it makes at least one tombstone: the folder is empty; or
- the net loss, the tombstones of step 6 minus the live entries created at paths that had no file on record, is at least 10 and more than half of the live entries that had a file on record. A rename is a tombstone and a creation, so moving files loses nothing net.

Held and ignored entries do not count. The user then chooses one of:

- **Confirm** (`kide confirm`): write the current `HEAD` into `.kide/confirm`. The next scan at that `HEAD` is not guarded; a scan that makes a tombstone removes the file. Confirming needs no lock, so it works while a daemon holds the folder.
- **Restore** (`kide restore`): a scan that keeps every live entry whose file is gone, with no file on record, so that the entry is written back from the store (§5.3, "when an entry stops being held"); it creates the directory if it is missing. Nothing is deleted and `HEAD` does not move for those paths.
- **Reset** or **join** (§5.6).

`guard = off` (§5.8) turns the guard off, for a folder only a program writes, such as a server's copy that takes deletions only by pulls and by its own writes.

### 5.6 Pull and decisions

```
pull(device):
    scan()
    exchange Hello(HEAD) and Hello(R); fail Incompatible if the versions or formats do not agree (§4.2)
    if R == HEAD: Bye; return
    fetch(R); sync the store; clear staging
    decide every differing path; apply the decisions; Bye
```

An application may **review** the decisions before they are applied, for a device that takes changes only where the peer may make them: a refused path keeps the local entry (and no clash move touches it), and a failed review fails the pull, which then changes nothing. The pulled objects stay in the store; the peer's refused changes stay the peer's.

**Comparing.** The paths to decide are those whose entries differ between the two manifests in version, size or mtime, found by a merge join of the two entry sequences. Subtrees with equal ids hold equal entries and may be skipped on both sides.

**Rule.** With L and R the local and remote version records of a path, or none:

| # | Local | Remote | Decision |
|---|---|---|---|
| 1 | L | R = L | Only the mtime rule below |
| 2 | L | none | Keep L |
| 3 | none | R | Adopt R |
| 4 | L | R ≠ L | Frontier rule |

**Frontier rule.** With F the frontier of L and R (§3.2): if F has one element, keep L or adopt R. Otherwise the result is M = (value of w, F sorted), where w is the live element of F with the largest id, or the largest id if all are tombstones, unless the text merge below gives M's value. If M equals L, keep L; if R, adopt R; otherwise adopt M.

**Text merge.** A path **may hold text** unless its last component has an extension (the part after its last dot, unless that dot is the first character) that is, in any ASCII case, one of: 7z a aac avi avif bin bmp bz2 class cr2 db dll dmg dng doc docx dylib epub exe flac gif gz heic heif ico iso jar jpeg jpg m4a m4v mkv mov mp3 mp4 nef o odp ods odt ogg opus otf pack pdf png ppt pptx psd rar so sqlite tar tgz tif tiff ttf wasm wav webm webp woff woff2 xls xlsx xz zip zst. Every version of such a path keeps its content (§5.7), so any device can read its history back. A content is **text** if it is valid UTF-8 and holds no zero byte. A path is **prose** if its last component has an extension that is, in any ASCII case, one of: adoc markdown md org rst text txt. Only prose merges as text: code and structured text (JSON, canvases, lock files) would merge into something wrong without anyone being told, so they keep conflict copies until a merge that shows its conflicts exists. When the path is prose, F holds two or more live elements, all regular files (state 0 or 1), not all of one value, and they have a **base**, and the base and every live element have text contents, M = (t, F sorted) with w's state, where t is their merged text:

- The base is the nearest common ancestor of F's live elements: of the records all of them descend from or are, those from which no other such record descends; the one with the smallest id if several. It must be a live regular file; if there is none, or it is not, the rule above stands.
- t folds the live elements in ascending id order: t₁ = the first one's content, t_{k+1} = merge(base, t_k, the next one's content).
- merge(o, a, b) is a's if a = o or a = b, b's if b = o. Otherwise each text missing a final newline gets one, and each is split into lines, each line with its newline. Each side's changes are **hunks** against o: the gaps between the matched pairs of a longest common subsequence of lines found by Myers' greedy algorithm, after the common first and last lines are matched (the reference is `text::common` in the implementation). Hunks of the two sides **overlap** if they start at the same line of o or their ranges of o's lines intersect. Hunks that overlap nothing of the other side apply as they are, adjacent ones too. Overlapping hunks form a group over the union of their ranges, grown while a hunk of either side starts within it; the group's lines become a's version of them followed by b's, or one of them if both are equal. The result ends without a newline if the side that changed o's final-newline state lacks one, a's if both did.
- If any content exceeds 4 MiB, or a diff needs more than 4,096 inserted or deleted lines, the rule above stands.

Every input of the text merge is a function of F and the history, so devices that decide the same F make the same M. A text merge makes no conflict copy: every element's changes are in t, and none of its text is lost, since overlapping changes are kept side by side.

**Repair.** A pull whose store is marked damaged first repairs (§4.1) the closures of `HEAD` and R over the same session. A scan, plan or apply that fails with `Corrupt` or `NotFound` on a store that has just been marked damaged repairs `HEAD` and R and runs again, once. `kide verify --repair` re-hashes every object, dropping what is corrupt, and repairs `HEAD` and the retained roots from a device.

**Joining and resetting.** Two pulls a user asks for use another rule than the table above, and depend on which side is local; a device never uses them as its everyday rule, and the result is ordinary history that any device pulls with the table.

- **Join** (`kide join`), for a device whose `.kide` is new but whose folder holds files, such as a reinstalled one: its records have no parents (§3.1), so the frontier rule would pick a winner by id, bring back files deleted elsewhere and let stale copies win about half the time. When the table's rule gives a new merge, the join adopts R instead; if L is live and no record in R's history (R and its ancestors) has L's value, L's value is kept as a conflict copy, the record (L's value, [L]) at L's copy path with L's mtime, decided there as usual. Every other decision is the table's.
- **Reset** (`kide reset`): every path whose L differs from R adopts R, and a live L with no R becomes the tombstone (tombstone, [L]). Its scan restores (§5.5) instead of deleting, so a folder the guard refused can be reset. The device then equals the peer, apart from tombstones at the paths only it had; replaced and deleted files go to the trash or stay in the store's history.

**Conflict copies** are made only when the result is a new merge M, neither L nor R, and not a text merge; when the result is L or R, the device that made that merge also made its copies, and they arrive as ordinary entries. With a new M, every live element x of F whose root differs from M's is preserved at `<stem>.kide-conflict-<first 12 hex digits of x><.ext>` with record (x's value, [x]), unless x is a leaf of L or of R where that record is a merge whose value is not x's: whoever made that merge made x's copy then, and a user may since have deleted it. The extension is the part after the last dot, unless that dot is the first character of the name: `notes.md` → `notes.kide-conflict-3fa2b1c90d4e.md`, `.bashrc` → `.bashrc.kide-conflict-3fa2b1c90d4e`. A copy takes the mtime of the entry, L's or R's, whose record's value is x's, or the larger of the two if both are. If the copy's name would exceed 255 bytes, the stem is cut to the longest prefix, at a character boundary, for which it fits. If even an empty stem does not fit, or the copy's path would exceed 4,096 bytes, no copy is made: the loser stays in history (§5.7 keeps it), and the would-be copy path and the loser's id are reported as for held entries (§5.3). A copy is decided at its path like a remote record, by this section's rule, against whatever that path holds; that decision may make further copies. Each level adds 27 bytes to the name, so the 255-byte limit bounds the depth.

**Name clashes.** When a live file at p and live entries below p (`p/…`) result from the decisions, the directory keeps the name. The file's record moves to its conflict-copy path as (its value, [its id]), decided there like any copy, and the entry at p becomes the tombstone (tombstone, [its id]); the file itself goes to the copy path, or to the trash if the copy path's decision does not adopt it. A tombstone at p stays and takes no part in a clash. A live empty-directory entry `d/` with live entries below it becomes the tombstone (tombstone, [its id]), as a scan would make it.

**Modification times.** With mL and mR the local and remote mtimes:

For regular files (links and directories carry `mtime` 0):

| Decision | Resulting mtime |
|---|---|
| Row 1 | max(mL, mR) |
| Keep L | mL |
| Adopt R | mR |
| Merge with the value of both sides, or of neither | max(mL, mR) |
| Merge with the value of one side only | That side's mtime |

Each result depends only on the two entries, so devices converge in any topology.

**Applying decisions.**

1. A path is changed only if its file still matches the index (same size, mtime and inode, or absent when the index says absent); for a racy entry the content and kind must also match. Otherwise it is skipped, and the next scan and pull reconcile it.
2. Clash moves come first, then deletions, then writes. A path whose write fails locally is skipped and retried later.
3. Writing: write the content into `.<name>.kide-tmp` beside the target, fsync it, set mode 0644 or 0755 (or create the link or directory); check again, just before replacing, that the target's metadata still matches the index as in step 1, and if not, remove the temporary file and skip the path, so that a save made while the content was written is not lost; keep the file or link it replaces in the trash (step 4's place, as a second link to it, or a copy); rename it over the target, set the decided mtime, re-read the metadata and record it in the index.
4. Deleting: move the file to `.kide/trash/<unix-time>/<path>` (or `<unix-time>-<n>` when taken), then remove parent directories that became empty and have no entry.
5. Paths are validated (§5.3) before any write, which rules out traversal and writes into `.kide`. Sync never follows a symbolic link, neither as a file nor as a parent component: a path with a symbolic link on disk as one of its parent components is skipped, like a failed write.
6. The index is written after the folder changes; `HEAD` last.

### 5.7 Garbage collection

The roots are `HEAD` and the roots in `.kide/roots`, each wanted as content whose chunks are manifest chunks, the profile's own objects. A manifest chunk links to the version record of each entry, wanted with content and labelled with the entry's path; one at a path that may hold text (§5.6) is wanted with its whole history's content. A root therefore needs its manifest content, every version record named in it with that record's content closure, every live leaf of a merge record, the content of every version at every path that may hold text, and all history. `kide gc` keeps those, removes every other object, clears `staging/`, deletes trash entries older than `trash_days` and retained roots older than `keep_days`, and syncs the store.

### 5.8 Configuration and daemon

```
folder        = <dir>       # the synchronised directory, if not the one holding .kide/; relative to that one
mode          = sync        # sync (default), or send
ignore        = <pattern>   # neither scanned nor written on this device; repeatable
sync          = <pattern>   # synced although an earlier ignore covers it; repeatable
transport     = quic        # default; fips is experimental; plain is for local tests (§4.2)
self          = npub1… 192.0.2.10:7840 # this device
device        = npub1… 192.0.2.11:7840 <certificate-sha256> # an own device: pulled from, and allowed to pull; repeatable; at most W − 1 = 31 (§3.1)
port          = 7839        # fips: the port on every device's FIPS address
scan_interval = 10          # seconds
pull_interval = 10          # seconds
trash_days    = 30
keep_days     = 30
guard         = on          # on (default), or off: scans record a lost or mostly deleted folder (§5.5)
```

A **send folder** (`mode = send`) never writes into its folder: it scans and serves, never pulls (`kide pull` is refused, `kide run` pulls from no device), and scanning skips names not in NFC instead of renaming them (§5.3). Its devices pull from it as from any device; what they change never comes back. It lets Kide read a folder that another program keeps, such as a vault another sync service writes.

**Ignored paths.** `ignore` and `sync` lines are rules, taken in the order they appear. A rule's pattern is a valid path (§5.3) once `*` is read as an ordinary character, a trailing `/` dropped; in it, `*` matches any run of characters within one component, the empty one included. A rule applies to the paths its pattern matches and to every path below them, and the last rule that applies to a path decides: the path is **ignored** if that rule is `ignore`; a path no rule applies to is synced. So `ignore = .obsidian` then `sync = .obsidian/plugins/*/data.json` syncs only each plugin's settings there. Scanning never looks at an ignored path, so local files there never become versions and their absence is never a deletion; entries other devices send there are held (§5.3) without being reported: kept in the manifest, never written, and never checked against the disk. Ignoring is local, like holding: it never enters an object, and devices may ignore different paths. For example, `ignore = .obsidian` keeps a vault's own Obsidian configuration out of the folder. The configuration a new folder starts with ignores `.git`: a Git repository's own files (index, refs, packs) break when two devices write them independently, so a repository is shared through Git remotes, not by syncing its `.git`.

In the default QUIC transport, `init` creates a private secp256k1 device key
(`.kide/device.key`, 32 bytes), a self-signed TLS certificate (`tls.cert`, DER)
and its private key (`tls.key`, PKCS#8 DER). Private files are created with mode
0600 on Unix. It prints the public npub and SHA-256 certificate hash; these must
be exchanged over a trusted channel. `self = <npub> <address:port>` must match the
local key; each `device = <npub> <address:port> <certificate-sha256>` supplies the
peer's pin. Each connection trusts only that peer's certificate and answers the
challenge with the local key; the server challenges and admits only configured
device keys, independent of source IP. All network commands use this transport.
The program serves `/` and signs the peer address's IP host. WebTransport pins
require certificates valid at most 14 days. With the daemon stopped, `renew-tls`
replaces only TLS credentials and prints a new hash to exchange and configure
on peers before restarting; it never changes the device key. A partial renewal
fails closed and can be rerun. These are local configuration rules, with no
change to the wire protocol or object format.

In the plain transport, `self` and every `device` line also carry an address and port, as in `device = npub1… [::1]:7840`: the server listens on its own, connects to the devices', and accepts connections from the devices' addresses.

`kide run` holds the lock, serves the folder (§4.2), scans every `scan_interval` and pulls from every device every `pull_interval`. It also scans as soon as it notices a change in the folder, where the platform tells it (inotify on Linux, FSEvents on macOS, ReadDirectoryChangesW on Windows; changes in `.kide` and ignored paths do not count; the periodic scan stays, for changes it was not told of), and it pulls from a device as soon as that device announces a root.

**Announcing roots.** A daemon's server accepts the live stream `head` (§4.2) from its devices: it opens the stream with its current root as the version (32 bytes) and sends every new root as an update, once, when a scan or pull changes it; every other stream name is refused. A daemon opens `head` on every device it pulls from, pulls from that device when the stream opens and on every update, and opens a new session after one ends, waiting 1 s, then twice as long each time up to 30 s. Announcements only bring pulls forward: a pull compares roots and fetches what it lacks as always, so a lost announcement costs time, never correctness. An `Unavailable` device is retried after a delay that starts at `pull_interval` and doubles up to 5 minutes; a success resets it. A device that sends a corrupt object is reported, nothing from that pull is applied, and it is retried with the same back-off as an `Unavailable` device. An `Incompatible` device is reported and retried only every 5 minutes. A scan refused by the guard (§5.5) is reported and retried at every interval, and the pulls fail with it, until the user confirms, restores, resets or joins. The daemon collects garbage (§5.7) once a day, between pulls; `kide gc` does the same on demand when no daemon runs. `kide run` has no stop command and may be killed at any moment: staging survives, `HEAD` is written last, and the store recovers to its last synced state.

## 6. Live documents

Live documents are optional and built on the folder profile: a document's history is stored as ordinary files. The document engine is a CRDT (the first implementation uses Loro) reached through an interface with: the text; the version; export of updates since a version; import; a diff-based local edit; a fork at a version; and a snapshot.

### 6.1 Documents

- A text file `<name>` is a document when a sibling directory `<name>.history/` exists. Each file in it is named `<id-hex>.loro`, where the id is H of its bytes, and holds an export. History files are never modified, so additions on two devices never conflict.
- The text file is derived: it equals the text of a document into which every history file has been imported, in any order.
- **Saving** (after 2 seconds of inactivity and on close) exports the updates since the last save as a new history file, then writes the text file.
- **Merge.** When the frontier of a document's text file has two or more elements, no conflict copy is made. After all other paths are applied, the text is regenerated from all history files and recorded as (regenerated value, F sorted). Devices with equal sets of history files make equal records; since history files never conflict, the sets become equal and documents converge.
- **Live channel.** A document is kept live on a live session (§4.2): the stream's name is the document's path, its version the document's version, and its updates the engine's exported updates.
- **Compaction** replaces the history files with one snapshot file; the removed files propagate as deletions, and a file arriving meanwhile is imported on top.
- **Front end.** One static HTML page with a plain `<textarea>`, served by `kide run` on localhost only. Local edits are sent as splices (UTF-16 position, deleted length, inserted text) by HTTP POST; remote edits arrive as splices over Server-Sent Events and are applied with `setRangeText(…, "preserve")`.

### 6.2 Edits made in other programs

When a scan reports a changed document: if its content root is one the daemon wrote, nothing happens. Otherwise take the written version the new text most resembles (most shared chunks, confirmed by diff size). If the document has not changed since that version, apply the new text as a local edit; otherwise fork at that version, apply the text on the fork, and import the fork's updates, so they count as concurrent with everything since. Then save.

## 7. Derived data

Annotations (such as embeddings) are kept outside the format, in `.kide/annotations/<annotator>/<chunk id>`, where the annotator id names a model and its version. A chunk is annotated once; every other occurrence reuses the result.

## 8. Test vectors

The values are produced by the reference implementation `crates/kide/reference/kide_ref.py`, written from this spec alone. An implementation must reproduce every value. They are never edited by hand: a change to any rule they depend on (§2, §3.1, §5.1 to §5.3) increments the folder format (§1, §5.4), updates the reference, and regenerates this section.

**Inputs.**

```
T: (0..3000).map(|i| format!("{i:05} {}\n", "lorem ipsum ".repeat(i % 7))).concat()   // 128,928 bytes
B: (0u32..8192).flat_map(|i| H(&i.to_le_bytes())).collect()                          // 262,144 bytes
L: (0..6000).map(|i| format!("{{\"id\":{i},\"tag\":\"{}\"}},", "ab".repeat(i % 5))).concat()  // 148,890 bytes
Z: 1,048,576 zero bytes                                                               // forced cuts and closes at W
```

**Records.** Change metadata is empty. The state is the single byte given. v1 = (no parents, root of `hello\n`, state 0); v2 = ([v1], root of `hello world\n`, state 0); v3 = ([v2], tombstone: zero root, empty state); vx = ([v1], root of `hello\n`, state 1); vb = (no parents, root of B, state 0); vd = (no parents, root of empty content, state 3).

vm moves v1 from `a.txt` to `renamed.txt`; vr restores its value forward after vm, with the manifest root below as provenance.

**Terms.** *Widest node:* the largest child count of any node in the tree. *Level:* the root node's level.

**Manifest.** `a.txt` → v1, size 6, mtime 1,761,134,400,000,000,000; `b/c.bin` → vb, size 262,144, mtime 1,761,138,000,123,456,789; `d/` → vd, 0, 0.

| Value | Expected |
|---|---|
| `GEAR[0]` | `0x987ab3ff9c0b346e` |
| `GEAR[1]` | `0xc55445342f12f54b` |
| `GEAR[255]` | `0xd04019aae60a10a8` |
| T: chunk count; first three cut offsets | 102; 2755, 4589, 5873 |
| B: chunk count; first three cut offsets | 201; 2202, 3808, 4232 |
| L: chunk count; first three cut offsets | 109; 693, 1973, 2312 |
| Z: chunk count; widest node | 128; 32 children |
| Root of T | `16f16f8358dd7630563a5c7cf38e4740f8de61917a0d653e38514ca375dd274a`, level 2 |
| Root of B | `055193732071cb286bd072506c08812c4850d77dbb16ca68f884e48e47d9e162`, level 2 |
| Root of L | `3711b36c37e4c22f8a167e27af9bab1a78d9491b602816b35a82ba574a645718`, level 2 |
| Root of Z | `72883f314f3e0c082c160ac301d952b0c505d9de73c65c45c58ef5ad09957a2f`, level 2 |
| Root of empty content | `4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce23c7785459a`, level 1 |
| Root of `hello\n` | `d47869fad8f086635a8b76ec59d377a364ba9cd89bda87d06bbf4140ec3476fe`, level 1 |
| Root of `hello world\n` | `f47bddb1e3919b5e4cf3ade301377b03cc906286fc5a13155ac5b0f669382f90`, level 1 |
| v1 | `c83ec0bf3a3e8f86808088457a387b3ff116c3aae5cc0142eff4d6825d46497d` |
| v2 | `61ff42bd4be644562498bb2b2340286cfe166a8aa805a98b60f0ef69b2fcd40f` |
| v3 | `9590e69d2172c5a0013f9b68d3ea2c73f1b6584a983d71a2f439f4d4659eee24` |
| vx | `a496b42c77d8de6ca91c5d418431ef2a5e138e8bb2e5e9d4ca8f5a631d7de2ad` |
| vb | `1be223cf2cd3b81a0723b4b23a5de9e11f9e751f6fbbdf697f31802ceed7eef5` |
| vd | `681d074a81a5dc6c0f330f83257c43928ac39bfbe847783caf3e667be8a2f1c8` |
| Manifest root | `03dc07e212974fda5660c8be5b9971359fd06293fa8ba70a18e1df4ad7970623`, level 1 |
| vm (move) | `183f361b2ce00b81b580a01b363edb798a26f2b7477a1cfcf2cdc400ad6f4d44` |
| vr (forward restore) | `c0f2fc48c7eb41f20723f3c9eab07cdf95a0c062b54d27e4bfaa88a10cea5bfc` |

## 9. Parameter status and FIPS

**Parameters, decided by measurement** at k = 10 on a real note corpus and on the inputs of §8:

- **k = 10.** The largest k for which one inserted byte in 8 MiB of text costs less than 10 KiB on the wire, averaged over positions; k = 11 and k = 12 cost 16.1 and 23.4 KiB.
- **min = S/4.** Minimums of 64 and S/8 add 11 to 17 % more objects with no gain in deduplication, and cost more on T.
- **W = 2^b.** One inserted byte in 8 MiB costs 7.9 KiB on average (12.6 KiB at most) in corpus text, against 8.7 (15.5) KiB with W = 8·2^b, and 5.8 (9.5) KiB against 9.0 (11.1) KiB in T repeated 65 times. The price is 41 % more content nodes, but only 1.8 % more objects and 0.08 % more first-sync bytes; trees of the corpus are four levels deep either way.
- **Other figures.** Renaming a directory of 1,000 files costs 267 KB of manifest. Forced cuts make 1.05 % of the corpus bytes.

**Storage, measured and not adopted.** A store indexed per level-1 node instead of per object was measured and not adopted. Its full chunk map takes as much memory as a per-object index, and edits cost about eight times more in the store; a lean form would make chunks reachable only through their node. If a per-object in-memory index (about 50 MiB per GiB of content) becomes a limit, an index on disk is the smaller change.

**Round trips.** Fetch (§4.3) brings a pull down to about one round trip after Hello when the push covers the change; round trips over FIPS are still to be measured between two machines.

**Transports beyond FIPS.** QUIC is the program's default transport; FIPS is optional and experimental. Over QUIC (§4.2), TLS authenticates the server and the challenge the client; read rights (§4.2) let a server with several readers serve each only what it may read.

**FIPS use.** FIPS provides authentication, encryption, routing and discovery; Kide relies on nothing else from it.

- **Presence.** FIPS sends no reachability events. Where its control socket is available, a daemon may poll it (`show_sessions`, `show_peers`) and pull from a device as soon as a session or link to it appears, in addition to every `pull_interval`.
- **Ordering devices.** The same queries report round-trip time, loss and goodput per device; a pull may use them to choose or order the devices it fetches from.
- **One-way links.** FIPS has none: every FIPS transport is two-way. A bundle for one-way media would be a file format, outside this version.
- **Signing.** FIPS offers no signing service, so a device list published to Nostr would be signed with a key Kide holds.
- **Later:** sharing a folder with another person's npub, which FIPS already reaches but which changes the trust model of §4.2.
