//! Challenge keys from the browser, against the second server of
//! `examples/browser_peer` (in `v2`), which admits only the key of the
//! secret `[7; 32]`; `test.sh` passes its line in `KIDE_PEER` at build time,
//! and without it the tests do nothing.
#![cfg(target_arch = "wasm32")]

use kide::auth::Secret;
use kide::error::Error;
use kide::hash::{from_hex, to_hex};
use kide::iface::{Live, LiveEvent};
use kide::store::MemStore;
use kide::walk::Versions;
use kide_web::Key;
use kide_web::live::connect;
use kide_web::pull::pull;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

/// The challenging server's URL, the certificate hash and the root.
fn peer() -> Option<(&'static str, Vec<u8>, &'static str)> {
    let peer: Vec<&str> = option_env!("KIDE_PEER").map_or(vec![], |p| p.split(' ').collect());
    let [_, cert, root, _, keyed] = peer[..] else {
        console_log!("KIDE_PEER is unset: skipped");
        return None;
    };
    Some((keyed, from_hex(cert).unwrap(), root))
}

fn key(i: u8) -> Option<Arc<Secret>> {
    Some(Arc::new(Secret::from_bytes(&[i; 32]).unwrap()))
}

async fn sleep(ms: i32) {
    let p = js_sys::Promise::new(&mut |resolve, _| {
        let set: js_sys::Function = js_sys::Reflect::get(&js_sys::global(), &"setTimeout".into())
            .unwrap()
            .into();
        set.call2(&JsValue::NULL, &resolve, &ms.into()).unwrap();
    });
    JsFuture::from(p).await.unwrap();
}

type Events = Rc<RefCell<VecDeque<LiveEvent>>>;

async fn next(events: &Events) -> LiveEvent {
    for _ in 0..1000 {
        if let Some(e) = events.borrow_mut().pop_front() {
            return e;
        }
        sleep(10).await;
    }
    panic!("no live event within 10 s");
}

#[wasm_bindgen_test]
async fn the_admitted_key_opens_streams_and_pulls() {
    let Some((url, cert, root)) = peer() else {
        return;
    };
    let events = Events::default();
    let into = events.clone();
    let live = connect(url, Some(&cert), key(7), move |e| {
        into.borrow_mut().push_back(e)
    })
    .await
    .unwrap();
    live.open("doc", b"7").unwrap();
    let e = next(&events).await;
    assert!(
        matches!(&e, LiveEvent::Open { name, .. } if name == "doc"),
        "{e:?}"
    );
    live.bye().unwrap();

    let store = MemStore::new();
    let (got, _) = pull(url, Some(&cert), key(7), &store, &Versions, &[])
        .await
        .unwrap();
    assert_eq!(got.to_hex(), root);
}

#[wasm_bindgen_test]
async fn another_key_is_turned_away() {
    let Some((url, cert, _)) = peer() else {
        return;
    };
    let up = pull(url, Some(&cert), key(7), &MemStore::new(), &Versions, &[]).await;
    assert!(
        up.is_ok(),
        "the server serves the admitted key: {:?}",
        up.err()
    );
    let events = Events::default();
    let into = events.clone();
    // The server checks the answer after its Hello: the session may open
    // and then end, or fail at once.
    match connect(url, Some(&cert), key(8), move |e| {
        into.borrow_mut().push_back(e)
    })
    .await
    {
        Ok(live) => {
            let _ = live.open("doc", b"");
            let e = next(&events).await;
            assert!(
                matches!(e, LiveEvent::Ended(Error::Unavailable(_))),
                "{e:?}"
            );
        }
        Err(e) => assert!(matches!(e, Error::Unavailable(_)), "{e:?}"),
    }
    let r = pull(url, Some(&cert), key(8), &MemStore::new(), &Versions, &[]).await;
    assert!(matches!(r, Err(Error::Unavailable(_))), "{:?}", r.err());
}

#[wasm_bindgen_test]
async fn no_key_is_invalid() {
    let Some((url, cert, _)) = peer() else {
        return;
    };
    let r = connect(url, Some(&cert), None, |_| {}).await;
    assert!(matches!(r, Err(Error::Invalid(_))), "{:?}", r.err());
    let r = pull(url, Some(&cert), None, &MemStore::new(), &Versions, &[]).await;
    assert!(matches!(r, Err(Error::Invalid(_))), "{:?}", r.err());
}

#[wasm_bindgen_test]
fn a_key_round_trips_through_its_bytes() {
    let k = Key::generate().unwrap();
    let back = Key::from_bytes(&k.bytes()).unwrap();
    assert_eq!(back.public_key(), k.public_key());
    let s = Secret::from_bytes(&k.bytes().try_into().unwrap()).unwrap();
    assert_eq!(k.public_key(), to_hex(&s.public()));
    assert_ne!(Key::generate().unwrap().public_key(), k.public_key());
    assert!(Key::from_bytes(&[0; 32]).is_err(), "zero is no secret key");
    assert!(Key::from_bytes(&[1; 31]).is_err(), "31 bytes");
}
