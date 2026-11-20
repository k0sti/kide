//! Pulling from the browser into an `OpfsStore`, in a dedicated worker,
//! against `examples/browser_peer` (in `v2`), whose line `test.sh` passes in
//! `KIDE_PEER` at build time; without it the test does nothing.
#![cfg(target_arch = "wasm32")]

use kide::hash::{Hash, from_hex, h, to_hex};
use kide::iface::ObjectStore;
use kide::load::{read_content, record};
use kide::walk::Versions;
use kide_web::OpfsStore;
use kide_web::pull::{pull, pull_at};
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_dedicated_worker);

#[wasm_bindgen_test]
async fn a_pull_brings_the_history_into_an_opfs_store() {
    let Some(peer) = option_env!("KIDE_PEER") else {
        console_log!("KIDE_PEER is unset: skipped");
        return;
    };
    let [url, cert, root, content, ..] = peer.split(' ').collect::<Vec<_>>()[..] else {
        panic!("KIDE_PEER: {peer:?}");
    };
    let cert = from_hex(cert).unwrap();
    let name = format!("pull-{}", js_sys::Date::now() as u64);

    let store = OpfsStore::open(&name).await.unwrap();
    let (got, report) = pull(url, Some(&cert), None, &store, &Versions, &[])
        .await
        .unwrap();
    assert_eq!(got.to_hex(), root);
    assert!(report.committed > 10, "{report:?}");
    store.sync().unwrap();
    drop(store);

    let store = OpfsStore::open(&name).await.unwrap();
    let newest = record(&store, &got).unwrap();
    let data = read_content(&store, &newest.root).unwrap();
    assert_eq!(to_hex(&h(&data).0), content, "the newest content");
    assert!(store.has(&newest.parents[0]).unwrap(), "the older record");

    let older = record(&store, &newest.parents[0]).unwrap();
    assert!(
        read_content(&store, &older.root).is_err(),
        "the older content is not pulled"
    );
    pull_at(
        url,
        Some(&cert),
        None,
        &store,
        &Versions,
        &newest.parents[0],
    )
    .await
    .unwrap();
    assert!(
        read_content(&store, &older.root).is_ok(),
        "pull_at brings it"
    );

    let have: [Hash; 1] = [got];
    let (_, again) = pull(url, Some(&cert), None, &store, &Versions, &have)
        .await
        .unwrap();
    assert_eq!((again.committed, again.pushed), (0, 0), "{again:?}");
}
