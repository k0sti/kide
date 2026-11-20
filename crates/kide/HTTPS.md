# HTTPS fallback

Enable the library's `https` feature to sync over TLS and WebSockets on TCP 443
when a network blocks QUIC's UDP. This is an application API; the `kide` program
continues to use QUIC by default. The protocol, device keys, admission and read
rights are the same on both transports (§4.2 of [SPEC.md](SPEC.md)).

- `transport::Https::new(tls, Some(device_key))` takes an
  `Arc<transport::https_tls::ClientConfig>` with the application's trusted roots
  and normal certificate/host verification. Both ends select HTTP/1.1 ALPN.
- `Https::pinned(certificate_hash, Some(device_key))` instead trusts exactly the
  SHA-256 hash of the server's DER certificate. Exchange it through an
  authenticated channel. This explicit pin replaces WebPKI name/expiry checks;
  the application controls its lifetime and rotation. A pin mismatch fails.
- `HttpsServer::start(listen, tls, route, Some(backs))` takes an
  `Arc<transport::https_tls::ServerConfig>` and dynamic `Fn(&str) -> Option<Served>`.
  `HttpsServer::from_der` builds it from a DER certificate and PKCS#8 key.
  Bind TCP 443 or place it behind a trusted reverse proxy. `addr()` reports the
  bound address; `stop()` and drop break connections and stop listening.
- `Transport::connect` opens a pull and `Transport::live` opens live control.
  Peers are `https://host[:port]/path`; the default port is 443. Route matching
  excludes the query. Unknown routes fail before Hello.
- `Https::live_serving(peer, events, serve)` keeps live control and reverse pulls
  on one outgoing connection. The callback selects each device folder by path.
  As with QUIC, return a `Served` without admission for a reverse pull: the device
  has already authenticated the server through TLS. Return `None` to refuse a
  path. Reverse sessions are pulls only, without live events.
- `HttpsBacks::default()` is the server's reverse-pull `Transport`. Give its clone
  to the server, then address a connected device as `<key hex>/<folder path>`.
  `keys()` lists devices whose live session proved a key. No connection is
  registered before admission. Concurrent reverse pulls share the connection;
  ending live control tears them down. Reconnecting replaces only that key's
  connection, and an older disconnect cannot remove the replacement.

Set `Served::admission` for every protected route, including the external host
among its `names`. Without admission a route is public, just as with QUIC.
The key challenge is inside verified TLS; the transport never retries without
certificate verification or falls back to plaintext.

An application can try QUIC first, then this transport after a connectivity
failure, and periodically retry QUIC. Keep the authenticated server identity,
route and admission policy the same. Surface admission, certificate, protocol,
format and corruption failures; a transport retry cannot fix or relax them.
Kide supplies the two mechanisms; the application owns retry timing, selected
transport status and deployment integration.

For an existing HTTPS site, route WebSocket upgrades on the sync paths to the
TLS backend. The trusted proxy must preserve the request path,
`Sec-WebSocket-Protocol: kide-sync-v1`, binary messages and Upgrade headers, use
a verified TLS backend connection, and allow long-lived sessions. Admission
uses the public host, not the backend host. This server handles WebSocket sync
only; it does not serve a website. Explicit HTTP CONNECT proxies are not
configured by this API.

The transport bounds messages to 64 KiB plus a nine-byte header, inbound and
outgoing queues to 16 messages, and concurrent streams to 64 including the
primary session. Slow reads apply backpressure; a read or write stalled for
30 seconds fails as `Unavailable`. TCP shares congestion and ordering among
streams. Live control sends the normal protocol keepalive every 10 seconds.
