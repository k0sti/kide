//! A partial pull from the browser against `examples/browser_peer` (in
//! `v2`), whose line `test.sh` passes in `KIDE_PEER` at build time; without
//! it the test does nothing.
#![cfg(target_arch = "wasm32")]

use kide::conformance::Plain;
use kide::hash::{Hash, from_hex};
use kide::iface::ObjectStore;
use kide::store::MemStore;
use kide_web::pull::{Part, pull_partial};
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
async fn a_partial_pull_brings_only_its_part_and_the_server_pushes_it() {
    let Some(peer) = option_env!("KIDE_PEER") else {
        console_log!("KIDE_PEER is unset: skipped");
        return;
    };
    let [_, cert, _, _, _, url, zero, one] = peer.split(' ').collect::<Vec<_>>()[..] else {
        panic!("KIDE_PEER: {peer:?}");
    };
    let cert = from_hex(cert).unwrap();
    let [zero, one] = [zero, one].map(|h| Hash::from_hex(h).unwrap());
    let store = MemStore::new();
    let part = Part {
        filter: &|p: &str| p.starts_with('0'),
        scope: b"0".to_vec(),
    };
    let (root, report) = pull_partial(url, Some(&cert), None, &store, &Plain, &[], part)
        .await
        .unwrap();
    assert!(store.has(&root).unwrap() && store.has(&zero).unwrap());
    assert!(!store.has(&one).unwrap(), "path 1 is outside the part");
    assert_eq!(report.requested, 0, "the push carried the part: {report:?}");
    assert_eq!(
        report.pushed, report.committed,
        "and only the part: {report:?}"
    );
}
