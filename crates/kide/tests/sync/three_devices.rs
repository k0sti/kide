//! Three devices: merges made on different devices agree, and a ring of
//! pulls converges (§3.2, §5.6).

use crate::device::{Device, Net, assert_same};
use kide::load;

fn ring() -> (Net, [Device; 3]) {
    let net = Net::default();
    let devs = ["a", "b", "c"].map(Device::new);
    for d in &devs {
        net.join(d);
    }
    (net, devs)
}

/// One round of the ring: a pulls c, b pulls a, c pulls b.
fn round(net: &Net, [a, b, c]: &mut [Device; 3]) {
    net.pull(a, c);
    net.pull(b, a);
    net.pull(c, b);
}

#[test]
fn three_concurrent_edits_converge_in_a_ring() {
    let (net, mut devs) = ring();
    devs[0].write("f.bin", b"base\n");
    round(&net, &mut devs);
    round(&net, &mut devs);
    for (i, d) in devs.iter().enumerate() {
        d.write("f.bin", format!("edit {i}\n").as_bytes());
        d.write(&format!("own-{i}"), b"x");
    }
    for _ in 0..3 {
        round(&net, &mut devs);
    }
    let [a, b, c] = &devs;
    assert_same(&[a, b, c]);
    let v = a.folder.index.0["f.bin"].version;
    let r = load::record(&*a.folder.store, &v).unwrap();
    assert_eq!(r.parents.len(), 3, "one flat merge of the three edits");
    assert_eq!(
        a.copies().len(),
        2,
        "the two losing edits: {:?}",
        a.copies()
    );
    let mut contents: Vec<Vec<u8>> = a.copies().iter().map(|p| a.read(p).unwrap()).collect();
    contents.push(a.read("f.bin").unwrap());
    contents.sort();
    let want: Vec<Vec<u8>> = (0..3).map(|i| format!("edit {i}\n").into_bytes()).collect();
    assert_eq!(contents, want, "no edit is lost");
}

#[test]
fn three_concurrent_edits_of_a_note_merge_into_one_text() {
    let (net, mut devs) = ring();
    devs[0].write("f.md", b"top\nmiddle\nbottom\n");
    round(&net, &mut devs);
    round(&net, &mut devs);
    let edits = [
        "TOP\nmiddle\nbottom\n",
        "top\nMIDDLE\nbottom\n",
        "top\nmiddle\nBOTTOM\n",
    ];
    for (d, e) in devs.iter().zip(edits) {
        d.write("f.md", e.as_bytes());
    }
    for _ in 0..3 {
        round(&net, &mut devs);
    }
    let [a, b, c] = &devs;
    assert_same(&[a, b, c]);
    assert_eq!(a.read("f.md").unwrap(), b"TOP\nMIDDLE\nBOTTOM\n");
    assert!(a.copies().is_empty(), "{:?}", a.copies());
}

#[test]
fn a_merge_travels_without_new_copies() {
    let (net, [mut a, mut b, mut c]) = ring();
    a.write("f.bin", b"base\n");
    net.sync(&mut a, &mut b);
    net.sync(&mut b, &mut c);
    a.write("f.bin", b"on a\n");
    b.write("f.bin", b"on b\n");
    a.scan();
    net.pull(&mut a, &mut b); // a merges
    net.pull(&mut c, &mut a); // c adopts the merge and its copy
    net.pull(&mut b, &mut c); // b adopts them from c
    assert_same(&[&a, &b, &c]);
    assert_eq!(c.copies().len(), 1);
}

#[test]
fn a_third_device_fast_forwards_to_a_clash_tombstone() {
    let (net, [mut a, mut b, mut c]) = ring();
    a.write("x", b"a file\n");
    net.sync(&mut a, &mut c);
    let file = c.folder.index.0["x"].version;
    b.write("x/inside", b"a directory\n");
    net.pull(&mut a, &mut b); // the directory keeps the name on a
    let t = a.folder.index.0["x"];
    assert!(!t.is_live(), "x is a tombstone on a");
    let r = load::record(&*a.folder.store, &t.version).unwrap();
    assert_eq!(r.parents, [file], "(tombstone, [the file's id])");
    let r = net.pull(&mut c, &mut a);
    assert_eq!(r.apply.trashed, 1, "c deletes its file: {r:?}");
    assert_eq!(c.head(), a.head(), "a fast-forward, no new merge");
    assert_eq!(c.folder.index.0["x"].version, t.version);
    assert_eq!(c.copies().len(), 1, "{:?}", c.copies());
    assert_eq!(c.read(&c.copies()[0]).unwrap(), b"a file\n");
    assert_eq!(c.read("x/inside").unwrap(), b"a directory\n");
    net.pull(&mut b, &mut c);
    assert_same(&[&a, &b, &c]);
}
