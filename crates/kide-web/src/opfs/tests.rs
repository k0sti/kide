use super::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_dedicated_worker);

/// A directory name no other test uses, in this run or an earlier one.
fn unique(what: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("test-{what}-{}-{n}", js_sys::Date::now() as u64)
}

async fn open(name: &str) -> OpfsStore {
    OpfsStore::open(name).await.unwrap()
}

#[wasm_bindgen_test]
async fn conformance() {
    let mut stores = Vec::new();
    for _ in 0..12 {
        stores.push(open(&unique("conformance")).await);
    }
    let stores = RefCell::new(stores);
    kide::conformance::object_store(&|| {
        let s = stores.borrow_mut().pop().expect("a store per check");
        Box::new(s)
    });
}

#[wasm_bindgen_test]
async fn objects_survive_reopening_and_compaction() {
    let name = unique("reopen");
    let s = open(&name).await;
    let ids: Vec<Hash> = (0..20u8).map(|i| s.put(&[i; 100]).unwrap()).collect();
    s.remove(&ids[0]).unwrap();
    s.sync().unwrap();
    drop(s);

    let s = open(&name).await;
    assert!(!s.has(&ids[0]).unwrap(), "removed");
    assert_eq!(s.get(&ids[5]).unwrap(), [5; 100]);
    assert_eq!(s.ids().unwrap().len(), 19);
    let before = s.state().end;
    s.compact().unwrap();
    assert!(
        s.state().end < before,
        "compaction drops the removed object"
    );
    assert_eq!(s.get(&ids[7]).unwrap(), [7; 100]);
    drop(s);

    let s = open(&name).await;
    assert_eq!(s.ids().unwrap().len(), 19);
    assert_eq!(s.get(&ids[19]).unwrap(), [19; 100]);
    assert_eq!(s.state().garbage, 0);
}

#[wasm_bindgen_test]
async fn a_torn_tail_is_cut_on_opening() {
    let name = unique("torn");
    let s = open(&name).await;
    let a = s.put(b"kept").unwrap();
    {
        let st = s.state();
        let torn = record(&h(b"lost"), Some(b"lost"));
        write(st.file(), st.end, &torn[..20]).unwrap();
    }
    drop(s);

    let s = open(&name).await;
    assert_eq!(s.ids().unwrap(), [a]);
    let b = s.put(b"after").unwrap();
    drop(s);
    let s = open(&name).await;
    assert_eq!(
        s.get(&b).unwrap(),
        b"after",
        "written where the tail was cut"
    );
}

#[wasm_bindgen_test]
async fn a_compaction_without_its_header_is_ignored() {
    let name = unique("interrupted");
    let s = open(&name).await;
    let a = s.put(b"current").unwrap();
    {
        let st = s.state();
        let other = &st.files[1 - st.cur];
        write(other, 0, &0u64.to_le_bytes()).unwrap();
        write(other, HEAD, &record(&h(b"half"), Some(b"half"))).unwrap();
    }
    drop(s);

    let s = open(&name).await;
    assert_eq!(s.ids().unwrap(), [a]);
    let st = s.state();
    assert_eq!(
        size(&st.files[1 - st.cur]).unwrap(),
        0,
        "the leftover is cleared"
    );
}
