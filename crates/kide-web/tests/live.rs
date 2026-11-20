//! Live sessions from the browser against `examples/browser_peer` (in `v2`),
//! whose URL and certificate hash `test.sh` passes in `KIDE_PEER` at build
//! time; without it the tests do nothing. The checks follow
//! `kide::conformance::live`.
#![cfg(target_arch = "wasm32")]

use js_sys::{Function, Promise, Reflect};
use kide::error::Error;
use kide::hash::from_hex;
use kide::iface::{Live, LiveEvent};
use kide::transport::live::Session;
use kide_web::live::connect;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

type Events = Rc<RefCell<VecDeque<LiveEvent>>>;

async fn sleep(ms: i32) {
    let p = Promise::new(&mut |resolve, _| {
        let set: Function = Reflect::get(&js_sys::global(), &"setTimeout".into())
            .unwrap()
            .into();
        set.call2(&JsValue::NULL, &resolve, &ms.into()).unwrap();
    });
    JsFuture::from(p).await.unwrap();
}

/// A session to the echo server, or `None` if there is none to test against.
async fn session() -> Option<(Arc<Session>, Events)> {
    let peer: Vec<&str> = option_env!("KIDE_PEER").map_or(vec![], |p| p.split(' ').collect());
    let [url, hash, ..] = peer[..] else {
        console_log!("KIDE_PEER is unset: skipped");
        return None;
    };
    let events = Events::default();
    let into = events.clone();
    let hash = from_hex(hash).unwrap();
    let s = connect(url, Some(&hash), None, move |e| {
        into.borrow_mut().push_back(e)
    });
    Some((s.await.unwrap(), events))
}

async fn next(events: &Events) -> LiveEvent {
    for _ in 0..1000 {
        if let Some(e) = events.borrow_mut().pop_front() {
            return e;
        }
        sleep(10).await;
    }
    panic!("no live event within 10 s");
}

async fn expect(events: &Events, want: LiveEvent) {
    let got = next(events).await;
    assert_eq!(format!("{got:?}"), format!("{want:?}"));
}

fn open(name: &str, version: &[u8]) -> LiveEvent {
    LiveEvent::Open {
        name: name.into(),
        version: version.to_vec(),
    }
}

fn update(name: &str, bytes: &[u8]) -> LiveEvent {
    LiveEvent::Update {
        name: name.into(),
        bytes: bytes.to_vec(),
    }
}

fn close(name: &str) -> LiveEvent {
    LiveEvent::Close { name: name.into() }
}

#[wasm_bindgen_test]
async fn a_path_the_server_does_not_serve_is_unavailable() {
    let Some(peer) = option_env!("KIDE_PEER") else {
        return;
    };
    let [url, hash, ..] = peer.split(' ').collect::<Vec<_>>()[..] else {
        panic!("KIDE_PEER: {peer:?}");
    };
    let url = url.replace("/open", "/none");
    let hash = from_hex(hash).unwrap();
    let r = connect(&url, Some(&hash), None, |_| {}).await.err();
    assert!(matches!(r, Some(Error::Unavailable(_))), "{r:?}");
}

#[wasm_bindgen_test]
async fn a_stream_opens_echoes_and_closes() {
    let Some((live, rx)) = session().await else {
        return;
    };
    live.open("doc", b"7").unwrap();
    expect(&rx, open("doc", b"server")).await;
    expect(&rx, update("doc", b"hello 7")).await;
    live.update("doc", b"edit").unwrap();
    expect(&rx, update("doc", b"edit")).await;
    live.close("doc").unwrap();
    expect(&rx, close("doc")).await;
    let r = live.update("doc", b"late");
    assert!(matches!(r, Err(Error::Protocol(_))), "closed: {r:?}");
    live.open("doc", b"8").unwrap();
    expect(&rx, open("doc", b"server")).await;
    live.bye().unwrap();
}

#[wasm_bindgen_test]
async fn the_server_refuses_and_closes_streams() {
    let Some((live, rx)) = session().await else {
        return;
    };
    live.open("refused/x", b"").unwrap();
    expect(&rx, close("refused/x")).await;
    live.open("doc", b"").unwrap();
    expect(&rx, open("doc", b"server")).await;
    expect(&rx, update("doc", b"hello ")).await;
    live.update("doc", b"close").unwrap();
    expect(&rx, close("doc")).await;
    live.bye().unwrap();
}

#[wasm_bindgen_test]
async fn updates_arrive_in_order_both_ways() {
    let Some((live, rx)) = session().await else {
        return;
    };
    for name in ["a", "b"] {
        live.open(name, b"").unwrap();
        expect(&rx, open(name, b"server")).await;
        expect(&rx, update(name, b"hello ")).await;
    }
    let n = 500u32;
    for i in 0..n {
        let name = ["a", "b"][i as usize % 2];
        live.update(name, &i.to_le_bytes()).unwrap();
    }
    for i in 0..n {
        let name = ["a", "b"][i as usize % 2];
        expect(&rx, update(name, &i.to_le_bytes())).await;
    }
    live.bye().unwrap();
}

#[wasm_bindgen_test]
async fn bye_ends_the_session() {
    let Some((live, rx)) = session().await else {
        return;
    };
    live.open("doc", b"").unwrap();
    expect(&rx, open("doc", b"server")).await;
    live.bye().unwrap();
    let mut e = next(&rx).await;
    if matches!(e, LiveEvent::Update { .. }) {
        e = next(&rx).await; // the catch-up, if it came before Bye
    }
    assert!(
        matches!(e, LiveEvent::Ended(Error::Unavailable(_))),
        "{e:?}"
    );
    assert!(live.update("doc", b"x").is_err(), "after bye");
}
