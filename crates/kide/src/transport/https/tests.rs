use super::*;
use crate::auth::PeerKey;
use crate::conformance::{TransportFixture, live, transport};
use crate::hash::Hash;
use crate::iface::LiveEvent;
use crate::protocol::PROTOCOL as VERSION;
use crate::protocol::{Admission, sample_served};
use std::sync::Mutex;
use std::time::Instant;

type Identity = (Vec<u8>, Vec<u8>);
fn identity() -> Identity {
    crate::transport::QuicServer::self_signed().unwrap()
}
fn client(id: &Identity, key: Option<Arc<Secret>>) -> Https {
    Https::pinned(crate::hash::h(&id.0), key).unwrap()
}
fn start(served: Served) -> (HttpsServer, Identity) {
    let id = identity();
    let server = HttpsServer::from_der(
        "127.0.0.1:0".parse().unwrap(),
        id.0.clone(),
        id.1.clone(),
        move |_| Some(served.clone()),
        None,
    )
    .unwrap();
    (server, id)
}
fn url(s: &HttpsServer) -> String {
    format!("https://127.0.0.1:{}", s.addr().port())
}

fn fixture(objects: &[Vec<u8>], root: Hash, protocol: u8) -> TransportFixture {
    let (server, id) = start(sample_served(objects, root, protocol));
    let peer = url(&server);
    let server = Mutex::new(server);
    TransportFixture {
        transport: Box::new(client(&id, None)),
        peer,
        stop: Box::new(move || server.lock().unwrap().stop()),
    }
}

#[test]
fn persisted_credentials_reject_a_broken_private_key() {
    let (cert, _) = crate::transport::QuicServer::self_signed().unwrap();
    let result = HttpsServer::from_der(
        "127.0.0.1:0".parse().unwrap(),
        cert,
        vec![0; 32],
        |_| None,
        None,
    );
    assert!(matches!(result, Err(Error::Invalid(_))));
}

#[test]
fn conformance() {
    transport(&fixture);
}

#[test]
fn live_conformance() {
    live(&fixture);
}

#[test]
fn a_routed_server_serves_each_path_its_own() {
    let key = Arc::new(Secret::from_bytes(&[1; 32]).unwrap());
    let admitted: PeerKey = key.public();
    let admission = Admission {
        names: vec!["127.0.0.1".into()],
        admits: Box::new(move |k| *k == admitted),
    };
    let open = sample_served(&[], Hash([5; 32]), VERSION);
    let keyed = Served {
        admission: Some(Arc::new(admission)),
        ..sample_served(&[], Hash([6; 32]), VERSION)
    };
    let id = identity();
    let listen = "127.0.0.1:0".parse().unwrap();
    let paths: Vec<(String, Served)> = vec![("/open".into(), open), ("/keyed".into(), keyed)];
    let route = move |p: &str| paths.iter().find(|(q, _)| q == p).map(|(_, s)| s.clone());
    let server = HttpsServer::from_der(listen, id.0.clone(), id.1.clone(), route, None).unwrap();
    let at = |path: &str| format!("{}{path}", url(&server));
    let hello = |c: &Https, path: &str| c.connect(&at(path))?.hello(&Hash::ZERO);

    let anyone = client(&id, None);
    assert_eq!(hello(&anyone, "/open").unwrap(), Hash([5; 32]));
    assert_eq!(hello(&anyone, "/open?x=1").unwrap(), Hash([5; 32]));
    let r = hello(&anyone, "/keyed");
    assert!(matches!(r, Err(Error::Invalid(_))), "no key: {r:?}");
    let keyed = client(&id, Some(key));
    assert_eq!(hello(&keyed, "/keyed").unwrap(), Hash([6; 32]));
    for path in ["", "/", "/other", "/open/x"] {
        let r = anyone.connect(&at(path)).err();
        assert!(matches!(r, Some(Error::Unavailable(_))), "{path:?}: {r:?}");
    }
}

#[test]
fn a_dynamic_server_decides_its_paths_while_serving() {
    let roots = Arc::new(Mutex::new(vec![("/v/a".to_string(), Hash([7; 32]))]));
    let known = roots.clone();
    let route = move |path: &str| {
        let roots = known.lock().unwrap();
        let (_, root) = roots.iter().find(|(p, _)| p == path)?;
        Some(sample_served(&[], *root, VERSION))
    };
    let id = identity();
    let listen = "127.0.0.1:0".parse().unwrap();
    let server = HttpsServer::from_der(listen, id.0.clone(), id.1.clone(), route, None).unwrap();
    let at = |path: &str| format!("{}{path}", url(&server));
    let c = client(&id, None);
    let hello = |path: &str| c.connect(&at(path))?.hello(&Hash::ZERO);
    assert_eq!(hello("/v/a?x").unwrap(), Hash([7; 32]));
    assert!(matches!(hello("/v/b"), Err(Error::Unavailable(_))));
    roots.lock().unwrap().push(("/v/b".into(), Hash([8; 32])));
    assert_eq!(hello("/v/b").unwrap(), Hash([8; 32]));
}

#[test]
fn a_server_pulls_back_through_a_device_connection() {
    let key = Arc::new(Secret::from_bytes(&[1; 32]).unwrap());
    let device: PeerKey = key.public();
    let (events, _server_events) = std::sync::mpsc::sync_channel(16);
    let served = Served {
        admission: Some(Arc::new(Admission {
            names: vec!["127.0.0.1".into()],
            admits: Box::new(move |k| *k == device),
        })),
        live: Some(events),
        ..sample_served(&[], Hash([5; 32]), VERSION)
    };
    let id = identity();
    let listen = "127.0.0.1:0".parse().unwrap();
    let backs = Backs::default();
    let route = move |_: &str| Some(served.clone());
    let server = HttpsServer::from_der(
        listen,
        id.0.clone(),
        id.1.clone(),
        route,
        Some(backs.clone()),
    )
    .unwrap();
    let peer = format!("{}/v/a", Hash(device).to_hex());
    assert!(matches!(
        backs.connect(&peer).err(),
        Some(Error::Unavailable(_))
    ));

    // The device's folder: one object at root 9.
    let x = b"on the device".to_vec();
    let mine = sample_served(std::slice::from_ref(&x), Hash([9; 32]), VERSION);
    let c = client(&id, Some(key));
    let (events, _device_events) = std::sync::mpsc::sync_channel(16);
    let serve = move |path: &str| (path == "/v/a").then(|| mine.clone());
    let _live = c.live_serving(&url(&server), events, serve).unwrap();
    let start = Instant::now();
    while backs.keys().is_empty() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "never registered"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut s = backs.connect(&peer).unwrap();
    assert_eq!(s.hello(&Hash::ZERO).unwrap(), Hash([9; 32]));
    let got = s.get(&[crate::hash::h(&x)]).unwrap();
    assert_eq!(got[0].as_ref().ok(), Some(&x));
    s.bye().unwrap();
    // A path the device does not serve.
    let other = format!("{}/v/b", Hash(device).to_hex());
    let r = backs.connect(&other).and_then(|mut s| s.hello(&Hash::ZERO));
    assert!(r.is_err(), "{r:?}");
}

#[test]
fn a_refused_live_session_receives_the_reason_promptly() {
    let key = Arc::new(Secret::from_bytes(&[2; 32]).unwrap());
    let (events, _server_events) = std::sync::mpsc::sync_channel(16);
    let served = Served {
        admission: Some(Arc::new(Admission {
            names: vec!["127.0.0.1".into()],
            admits: Box::new(|_| false),
        })),
        live: Some(events),
        ..sample_served(&[], Hash::ZERO, VERSION)
    };
    let (server, id) = start(served);
    let quic = client(&id, Some(key));
    let (tx, rx) = std::sync::mpsc::sync_channel(16);
    let live = quic.live_serving(&url(&server), tx, |_| None).unwrap();
    let _ = live.open("kota", b"");
    let (_, event) = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
    assert!(
        matches!(event, LiveEvent::Ended(ref e) if e.to_string().contains("a key this server does not admit")),
        "{event:?}"
    );
}

#[test]
fn a_challenging_server_admits_only_its_keys() {
    let key = |i: u8| Arc::new(Secret::from_bytes(&[i; 32]).unwrap());
    let admitted: PeerKey = key(1).public();
    let admission = Admission {
        names: vec!["127.0.0.1".into()],
        admits: Box::new(move |k| *k == admitted),
    };
    let x = b"x".to_vec();
    let served = Served {
        admission: Some(Arc::new(admission)),
        ..sample_served(std::slice::from_ref(&x), Hash([5; 32]), VERSION)
    };
    let (server, id) = start(served);
    let mut s = client(&id, Some(key(1))).connect(&url(&server)).unwrap();
    assert_eq!(s.hello(&Hash::ZERO).unwrap(), Hash([5; 32]));
    let got = s.get(&[crate::hash::h(&x)]).unwrap();
    assert_eq!(got[0].as_ref().ok(), Some(&x));

    let mut s = client(&id, None).connect(&url(&server)).unwrap();
    let r = s.hello(&Hash::ZERO);
    assert!(matches!(r, Err(Error::Invalid(_))), "no key: {r:?}");
    let mut s = client(&id, Some(key(2))).connect(&url(&server)).unwrap();
    s.hello(&Hash::ZERO).unwrap();
    let r = s.get(&[crate::hash::h(&x)]);
    assert!(
        matches!(&r, Err(Error::Unavailable(m)) if m.contains("a key this server does not admit")),
        "another key must receive the close reason: {r:?}"
    );
}

#[test]
fn tls_pin_mismatch_and_plain_http_are_refused() {
    let (server, _) = start(sample_served(&[], Hash::ZERO, VERSION));
    let wrong = Https::pinned(Hash::ZERO, None).unwrap();
    assert!(matches!(
        wrong.connect(&url(&server)),
        Err(Error::Unavailable(_))
    ));
    assert!(matches!(
        wrong.connect("http://127.0.0.1:443"),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn reverse_pulls_concurrently_keep_live_control_and_cleanup() {
    let key = Arc::new(Secret::from_bytes(&[3; 32]).unwrap());
    let device = key.public();
    let (tx, server_events) = std::sync::mpsc::sync_channel(16);
    let served = Served {
        admission: Some(Arc::new(Admission {
            names: vec!["127.0.0.1".into()],
            admits: Box::new(move |k| *k == device),
        })),
        live: Some(tx),
        ..sample_served(&[], Hash::ZERO, VERSION)
    };
    let id = identity();
    let backs = Backs::default();
    let mut server = HttpsServer::from_der(
        "127.0.0.1:0".parse().unwrap(),
        id.0.clone(),
        id.1.clone(),
        move |_| Some(served.clone()),
        Some(backs.clone()),
    )
    .unwrap();
    // Bigger than one stream's receive queue: transfer must wait, not fail.
    let bytes = vec![42; 2 << 20];
    let hash = crate::hash::h(&bytes);
    let mine = sample_served(std::slice::from_ref(&bytes), Hash([9; 32]), VERSION);
    let client = client(&id, Some(key));
    let (tx, device_events) = std::sync::mpsc::sync_channel(16);
    let live = client
        .live_serving(&url(&server), tx, move |p| {
            (p == "/v/a").then(|| mine.clone())
        })
        .unwrap();
    live.open("control", b"v").unwrap();
    let (remote, event) = server_events
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(matches!(event, LiveEvent::Open { .. }));
    remote.open("control", b"v").unwrap();
    assert!(matches!(
        device_events
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .1,
        LiveEvent::Open { .. }
    ));
    let peer = format!("{}/v/a", Hash(device));
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let (backs, peer, bytes) = (backs.clone(), peer.clone(), &bytes);
            scope.spawn(move || {
                let mut session = backs.connect(&peer).unwrap();
                assert_eq!(session.hello(&Hash::ZERO).unwrap(), Hash([9; 32]));
                assert_eq!(session.get(&[hash]).unwrap()[0].as_ref().unwrap(), bytes);
                session.bye().unwrap();
            });
        }
        live.update("control", b"while pulling").unwrap();
        assert!(
            matches!(server_events.recv_timeout(std::time::Duration::from_secs(1)).unwrap().1, LiveEvent::Update { bytes, .. } if bytes == b"while pulling")
        );
    });
    let (tx, _second_device_events) = std::sync::mpsc::sync_channel(16);
    let second = client
        .live_serving(&url(&server), tx, |p| {
            (p == "/v/a").then(|| sample_served(&[], Hash([8; 32]), VERSION))
        })
        .unwrap();
    second.open("replacement", b"").unwrap();
    assert!(
        matches!(server_events.recv_timeout(std::time::Duration::from_secs(1)).unwrap().1, LiveEvent::Open { name, .. } if name == "replacement")
    );
    live.bye().unwrap();
    assert!(matches!(
        server_events
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .1,
        LiveEvent::Ended(_)
    ));
    let mut after = backs.connect(&peer).unwrap();
    assert_eq!(after.hello(&Hash::ZERO).unwrap(), Hash([8; 32]));
    after.bye().unwrap();
    server.stop();
    assert!(backs.keys().is_empty());
    assert!(matches!(backs.connect(&peer), Err(Error::Unavailable(_))));
}

#[test]
fn stopping_also_cancels_pending_tls_handshakes() {
    use std::io::Read;
    let (mut server, _) = start(sample_served(&[], Hash::ZERO, VERSION));
    let mut raw = std::net::TcpStream::connect(server.addr()).unwrap();
    raw.set_read_timeout(Some(std::time::Duration::from_millis(200)))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    server.stop();
    match raw.read(&mut [0; 1]) {
        Ok(0) => (),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => (),
        other => panic!("pending TLS connection survived stop: {other:?}"),
    }
}

#[test]
fn application_roots_verify_the_certificate_and_host() {
    let (server, id) = start(sample_served(&[], Hash::ZERO, VERSION));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(id.0.into()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let client = Https::new(Arc::new(config), None).unwrap();
    let localhost = format!("https://localhost:{}", server.addr().port());
    assert_eq!(
        client
            .connect(&localhost)
            .unwrap()
            .hello(&Hash::ZERO)
            .unwrap(),
        Hash::ZERO
    );
    assert!(
        matches!(client.connect(&url(&server)), Err(Error::Unavailable(_))),
        "IP address is not in this certificate"
    );
}
