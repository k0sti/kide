//! The QUIC server for the browser tests in `kide-web`: two services on one
//! port, whose live sessions run `conformance::echo` and which serve a
//! history of two versions of one content under the profile
//! `walk::Versions`. The one at `/open` serves anyone; the one at `/keyed`
//! challenges every client for a key (§4.2) and admits only the key of the
//! secret `[7; 32]`. The one at `/parts` serves a tree of
//! `conformance::Plain` objects, for partial pulls: the root links to `0`
//! and `1`, each to a content; its scope rule reads the scope as a path
//! prefix. Other paths are refused. Prints, on one line: the first's URL, the
//! SHA-256 of the self-signed certificate in hex, the root and the SHA-256 of
//! the newest content in hex, the second's URL, then the third's URL and the
//! ids of its objects `0` and `1`. Then serves until killed.

use kide::auth::Secret;
use kide::conformance::{Plain, Rng, echo};
use kide::format::content::write_content;
use kide::format::record::Record;
use kide::hash::{Hash, h, to_hex};
use kide::iface::ObjectStore;
use kide::iface::Want;
use kide::protocol::Admission;
use kide::store::MemStore;
use kide::transport::{QuicServer, Served};
use kide::walk::Versions;
use std::sync::Arc;
use wtransport::Identity;

fn main() -> kide::Result<()> {
    let store = Arc::new(MemStore::new());
    let content = |data: &[u8]| -> kide::Result<Hash> {
        let mut sink = |_: &Hash, b: &[u8]| store.put(b).map(|_| ());
        Ok(write_content(data, &mut sink)?.id)
    };
    let mut rng = Rng::new(1);
    let first = rng.bytes(300_000);
    let mut second = first.clone();
    second[1000..1100].copy_from_slice(&rng.bytes(100));
    second.extend(rng.bytes(50_000));
    let record = |parents, data: &[u8]| -> kide::Result<Hash> {
        let r = Record {
            change: Vec::new(),
            parents,
            root: content(data)?,
            state: vec![0],
        };
        store.put(&r.encode()?)
    };
    let older = record(vec![], &first)?;
    let root = record(vec![older], &second)?;

    let identity = Identity::self_signed(["localhost", "127.0.0.1"])
        .map_err(|e| kide::Error::Invalid(e.to_string()))?;
    let cert = identity.certificate_chain().as_slice()[0].hash();
    let served = Served {
        live: Some(echo()),
        profile: Some(Arc::new(Versions)),
        ..Served::new(store.clone(), move || root)
    };
    let admitted = Secret::from_bytes(&[7; 32])?.public();
    let keyed = Served {
        live: Some(echo()),
        admission: Some(Arc::new(Admission {
            names: vec!["127.0.0.1".into()],
            admits: Box::new(move |k| *k == admitted),
        })),
        ..served.clone()
    };
    let object = |data: &[u8]| -> kide::Result<Hash> {
        store.put(&Plain::object(&[(Want::Content, content(data)?)]))
    };
    let (zero, one) = (object(&first)?, object(&rng.bytes(200_000))?);
    let tree = store.put(&Plain::object(&[(Want::Object, zero), (Want::Object, one)]))?;
    let parts = Served {
        live: None,
        profile: Some(Arc::new(Plain)),
        scope: Some(Arc::new(|scope: &[u8], path: &str| {
            path.as_bytes().starts_with(scope) || scope.starts_with(path.as_bytes())
        })),
        ..Served::new(store.clone(), move || tree)
    };
    let listen = "127.0.0.1:0"
        .parse()
        .map_err(|_| kide::Error::Invalid("address".into()))?;
    let paths: Vec<(String, Served)> = vec![
        ("/open".into(), served),
        ("/keyed".into(), keyed),
        ("/parts".into(), parts),
    ];
    let route = move |p: &str| paths.iter().find(|(q, _)| q == p).map(|(_, s)| s.clone());
    let server = QuicServer::start(listen, identity, route, None)?;
    let url = format!("https://127.0.0.1:{}", server.addr().port());
    println!(
        "{url}/open {} {} {} {url}/keyed {url}/parts {} {}",
        to_hex(cert.as_ref()),
        root.to_hex(),
        to_hex(&h(&second).0),
        zero.to_hex(),
        one.to_hex(),
    );
    loop {
        std::thread::park();
    }
}
