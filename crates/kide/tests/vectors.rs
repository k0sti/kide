//! Every §8 test vector, through the public API.

use kide::Hash;
use kide::conformance::{input_b, input_l, input_t, input_z};
use kide::format::chunk::cut_points;
use kide::format::content::write_content;
use kide::format::gear;
use kide::format::manifest::{Kind, PathEntry, build_manifest};
use kide::format::node::{Node, Root};
use kide::format::record::Record;
use std::collections::HashMap;

type Objects = HashMap<Hash, Vec<u8>>;

fn store(data: &[u8], objs: &mut Objects) -> Root {
    write_content(data, &mut |id, b| {
        objs.insert(*id, b.to_vec());
        Ok(())
    })
    .unwrap()
}

/// The largest child count of any node below `root` (§8, widest node).
fn widest(root: &Hash, level: u8, objs: &Objects) -> usize {
    let n = Node::decode(&objs[root]).unwrap();
    assert_eq!(n.level, level);
    let below = match level {
        1 => 0,
        _ => n
            .children
            .iter()
            .map(|c| widest(&c.id, level - 1, objs))
            .max()
            .unwrap_or(0),
    };
    below.max(n.children.len())
}

#[test]
fn gear_table() {
    let g = gear();
    assert_eq!(
        [g[0], g[1], g[255]],
        [0x987ab3ff9c0b346e, 0xc55445342f12f54b, 0xd04019aae60a10a8]
    );
}

#[test]
fn chunks_and_content_roots() {
    let want = [
        (
            "T",
            input_t(),
            128_928,
            102,
            [2755, 4589, 5873],
            "16f16f8358dd7630563a5c7cf38e4740f8de61917a0d653e38514ca375dd274a",
        ),
        (
            "B",
            input_b(),
            262_144,
            201,
            [2202, 3808, 4232],
            "055193732071cb286bd072506c08812c4850d77dbb16ca68f884e48e47d9e162",
        ),
        (
            "L",
            input_l(),
            148_890,
            109,
            [693, 1973, 2312],
            "3711b36c37e4c22f8a167e27af9bab1a78d9491b602816b35a82ba574a645718",
        ),
    ];
    for (name, data, len, n, first, root) in want {
        assert_eq!(data.len(), len, "{name} length");
        let cuts = cut_points(&data);
        assert_eq!((cuts.len(), &cuts[..3]), (n, &first[..]), "{name} chunks");
        let r = store(&data, &mut Objects::new());
        assert_eq!((r.id.to_hex().as_str(), r.level), (root, 2), "{name} root");
    }
}

#[test]
fn forced_cuts_and_the_w_bound() {
    let z = input_z();
    assert_eq!((z.len(), cut_points(&z).len()), (1_048_576, 128));
    let mut objs = Objects::new();
    let r = store(&z, &mut objs);
    let want = "72883f314f3e0c082c160ac301d952b0c505d9de73c65c45c58ef5ad09957a2f";
    assert_eq!((r.id.to_hex().as_str(), r.level), (want, 2));
    assert_eq!(widest(&r.id, r.level, &objs), 32);
}

#[test]
fn small_content_roots() {
    for (data, root) in [
        (
            &b""[..],
            "4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce23c7785459a",
        ),
        (
            b"hello\n",
            "d47869fad8f086635a8b76ec59d377a364ba9cd89bda87d06bbf4140ec3476fe",
        ),
        (
            b"hello world\n",
            "f47bddb1e3919b5e4cf3ade301377b03cc906286fc5a13155ac5b0f669382f90",
        ),
    ] {
        let r = store(data, &mut Objects::new());
        assert_eq!((r.id.to_hex().as_str(), r.level), (root, 1), "{data:?}");
    }
}

/// The §8 records v1, v2, v3, vx, vb and vd.
fn records() -> [Hash; 6] {
    let root = |data: &[u8]| store(data, &mut Objects::new()).id;
    let rec = |parents: Vec<Hash>, root: Hash, kind: Kind| {
        Record {
            change: Vec::new(),
            parents,
            root,
            state: kind.state(),
        }
        .id()
        .unwrap()
    };
    let v1 = rec(vec![], root(b"hello\n"), Kind::Regular);
    let v2 = rec(vec![v1], root(b"hello world\n"), Kind::Regular);
    let v3 = Record::tombstone(vec![v2]).id().unwrap();
    let vx = rec(vec![v1], root(b"hello\n"), Kind::Executable);
    let vb = rec(vec![], root(&input_b()), Kind::Regular);
    let vd = rec(vec![], root(b""), Kind::Directory);
    [v1, v2, v3, vx, vb, vd]
}

#[test]
fn version_records() {
    let want = [
        "c83ec0bf3a3e8f86808088457a387b3ff116c3aae5cc0142eff4d6825d46497d",
        "61ff42bd4be644562498bb2b2340286cfe166a8aa805a98b60f0ef69b2fcd40f",
        "9590e69d2172c5a0013f9b68d3ea2c73f1b6584a983d71a2f439f4d4659eee24",
        "a496b42c77d8de6ca91c5d418431ef2a5e138e8bb2e5e9d4ca8f5a631d7de2ad",
        "1be223cf2cd3b81a0723b4b23a5de9e11f9e751f6fbbdf697f31802ceed7eef5",
        "681d074a81a5dc6c0f330f83257c43928ac39bfbe847783caf3e667be8a2f1c8",
    ];
    assert_eq!(records().map(|h| h.to_hex()), want.map(String::from));
}

#[test]
fn manifest_root() {
    let [v1, _, _, _, vb, vd] = records();
    let entry = |path: &str, version, size, mtime| PathEntry {
        path: path.into(),
        version,
        size,
        mtime,
    };
    let entries = [
        entry("a.txt", v1, 6, 1_761_134_400_000_000_000),
        entry("b/c.bin", vb, 262_144, 1_761_138_000_123_456_789),
        entry("d/", vd, 0, 0),
    ];
    let r = build_manifest(&entries, &mut |_, _| Ok(())).unwrap();
    let want = "03dc07e212974fda5660c8be5b9971359fd06293fa8ba70a18e1df4ad7970623";
    assert_eq!((r.id.to_hex().as_str(), r.level), (want, 1));
}

#[test]
fn explicit_operation_records() {
    use kide::format::change::Change;
    let mut moved = Record {
        parents: vec![records()[0]],
        root: store(b"hello\n", &mut Objects::new()).id,
        state: Kind::Regular.state(),
        change: Change::Move {
            from: "a.txt".into(),
            to: "renamed.txt".into(),
        }
        .encode()
        .unwrap(),
    };
    assert_eq!(
        moved.id().unwrap().to_hex(),
        "183f361b2ce00b81b580a01b363edb798a26f2b7477a1cfcf2cdc400ad6f4d44"
    );
    moved.parents = vec![moved.id().unwrap()];
    moved.change = Change::Restore(
        Hash::from_hex("03dc07e212974fda5660c8be5b9971359fd06293fa8ba70a18e1df4ad7970623").unwrap(),
    )
    .encode()
    .unwrap();
    assert_eq!(
        moved.id().unwrap().to_hex(),
        "c0f2fc48c7eb41f20723f3c9eab07cdf95a0c062b54d27e4bfaa88a10cea5bfc"
    );
}
