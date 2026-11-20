use super::*;
use crate::conformance::Rng;
use crate::format::node::Node;
use crate::hash::h;
use std::collections::HashMap;

fn entry(path: &str, version: Hash, size: u64, mtime: i64) -> PathEntry {
    PathEntry {
        path: path.into(),
        version,
        size,
        mtime,
    }
}

fn encode(entries: &[PathEntry]) -> (Vec<u8>, Vec<usize>) {
    let (mut data, mut ends) = (Vec::new(), Vec::new());
    for e in entries {
        e.encode_into(&mut data);
        ends.push(data.len());
    }
    (data, ends)
}

/// Sorted entries with random paths up to four components deep.
fn sample(n: usize, rng: &mut Rng) -> Vec<PathEntry> {
    let mut v: Vec<PathEntry> = (0..n)
        .map(|i| {
            let depth = rng.below(4);
            let dirs: String = (0..depth)
                .map(|_| format!("d{:x}/", rng.below(40)))
                .collect();
            let size = rng.below(100_000);
            entry(
                &format!("{dirs}f{i:05}.txt"),
                h(&i.to_le_bytes()),
                size,
                i as i64,
            )
        })
        .collect();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

#[test]
fn entry_round_trip() {
    let e = entry("a/b.txt", h(b"v"), 6, -5);
    let mut b = Vec::new();
    e.encode_into(&mut b);
    assert_eq!(b.len(), 7 + TAIL);
    assert_eq!(b[7], 0);
    assert_eq!(decode_chunk(&b).unwrap(), vec![e]);
    assert!(decode_chunk(&[]).unwrap().is_empty());
}

#[test]
fn entry_of_maximum_path_length() {
    let max = format!(
        "{}{}/",
        format!("{}/", "x".repeat(255)).repeat(15),
        "y".repeat(255)
    );
    let e = entry(&max, h(b"d"), 0, 0);
    let (b, _) = encode(std::slice::from_ref(&e));
    assert_eq!(b.len(), MAX_ENTRY);
    assert_eq!(decode_chunk(&b).unwrap(), vec![e]);
    let (long, _) = encode(&[entry(&format!("z{max}"), h(b"d"), 0, 0)]);
    assert!(decode_chunk(&long).is_err(), "path of 4,097 bytes");
}

#[test]
fn decode_chunk_rejects_malformed_chunks() {
    let (ok, _) = encode(&[entry("a", h(b"1"), 1, 1), entry("b", h(b"2"), 2, 2)]);
    assert!(decode_chunk(&ok).is_ok());
    let (unsorted, _) = encode(&[entry("b", h(b"1"), 1, 1), entry("a", h(b"2"), 2, 2)]);
    let (dup, _) = encode(&[entry("a", h(b"1"), 1, 1), entry("a", h(b"2"), 2, 2)]);
    let (bad_path, _) = encode(&[entry("a//b", h(b"1"), 1, 1)]);
    let (reserved, _) = encode(&[entry(".KIDE/config", h(b"1"), 1, 1)]);
    let mut not_utf8 = ok.clone();
    not_utf8[0] = 0xff;
    let huge = [b'a'; MAX_MANIFEST_CHUNK + 1];
    let bad: [(&str, &[u8]); 9] = [
        ("truncated", &ok[..ok.len() - 1]),
        ("no zero byte", b"abc"),
        ("empty path", &ok[1..TAIL + 1]),
        ("not UTF-8", &not_utf8),
        ("unsorted", &unsorted),
        ("duplicate", &dup),
        ("invalid path", &bad_path),
        ("reserved name", &reserved),
        ("too long", &huge),
    ];
    for (name, b) in bad {
        assert!(decode_chunk(b).is_err(), "{name}");
    }
}

#[test]
fn forced_cuts_move_to_entry_ends() {
    let z = vec![0; 1 << 16];
    let ends: Vec<usize> = (3000..z.len()).step_by(3000).chain([z.len()]).collect();
    let want: Vec<usize> = (1..=7).map(|j| 9000 * j).chain([1 << 16]).collect();
    assert_eq!(manifest_cut_points(&z, &ends), want);
}

/// The chunks below `root`, in order.
fn leaf_chunks(root: &Hash, objs: &HashMap<Hash, Vec<u8>>) -> Vec<Vec<u8>> {
    let n = Node::decode(&objs[root]).unwrap();
    n.children
        .iter()
        .flat_map(|c| match n.level {
            1 => vec![objs[&c.id].clone()],
            _ => leaf_chunks(&c.id, objs),
        })
        .collect()
}

#[test]
fn chunks_hold_whole_entries() {
    let entries = sample(1500, &mut Rng::new(5));
    let (data, ends) = encode(&entries);
    let cuts = manifest_cut_points(&data, &ends);
    assert!(cuts.len() > 10, "{} chunks", cuts.len());
    assert!(cuts.iter().all(|c| ends.binary_search(c).is_ok()));
    let mut objs = HashMap::new();
    let root = build_manifest(&entries, &mut |id, b| {
        objs.insert(*id, b.to_vec());
        Ok(())
    })
    .unwrap();
    assert_eq!(root.bytes, data.len() as u64);
    let chunks = leaf_chunks(&root.id, &objs);
    assert_eq!(chunks.len(), cuts.len());
    let mut decoded = Vec::new();
    for c in &chunks {
        assert!(c.len() <= MAX_MANIFEST_CHUNK);
        decoded.extend(decode_chunk(c).unwrap());
    }
    assert_eq!(decoded, entries);
}

#[test]
fn other_parameters() {
    let entries = sample(1500, &mut Rng::new(6));
    let (data, ends) = encode(&entries);
    let spec = manifest_cut_points(&data, &ends);
    assert_eq!(manifest_cut_points_with(&data, &ends, &Params::SPEC), spec);
    let wide = manifest_cut_points_with(&data, &ends, &Params::of_k(11));
    assert!(
        wide.len() < spec.len(),
        "{} against {}",
        wide.len(),
        spec.len()
    );
    assert!(wide.iter().all(|c| ends.binary_search(c).is_ok()));
    let none = &mut |_: &Hash, _: &[u8]| Ok(());
    let root = build_manifest(&entries, none).unwrap();
    assert_eq!(
        build_manifest_with(&entries, &Params::SPEC, none).unwrap(),
        root
    );
    assert_ne!(
        build_manifest_with(&entries, &Params::of_k(11), none).unwrap(),
        root
    );
}

#[test]
fn build_rejects_bad_entries() {
    let v = h(b"v");
    let bad = [
        vec![entry("b", v, 1, 1), entry("a", v, 1, 1)],
        vec![entry("a", v, 1, 1), entry("a", v, 1, 1)],
        vec![entry("d/", v, 1, 0)],
        vec![entry("d/", v, 0, 1)],
        vec![entry("x.kide-tmp", v, 1, 1)],
    ];
    for es in bad {
        assert!(build_manifest(&es, &mut |_, _| Ok(())).is_err(), "{es:?}");
    }
    let empty = build_manifest(&[], &mut |_, _| Ok(())).unwrap();
    assert_eq!((empty.level, empty.bytes), (1, 0));
}

/// Validates `entries` against records given as (id, live).
fn validate_with(entries: &[PathEntry], records: &[(Hash, LiveKind)]) -> Result<()> {
    let map: HashMap<Hash, LiveKind> = records.iter().copied().collect();
    validate(entries, &mut |id| {
        map.get(id).copied().ok_or(crate::Error::NotFound(*id))
    })
}

#[test]
fn whole_manifest_rules() {
    let [file, dir, dead, link] = [h(b"file"), h(b"dir"), h(b"dead"), h(b"link")];
    let records = [
        (file, Some((Kind::Regular, 6))),
        (link, Some((Kind::Symlink, 3))),
        (dir, Some((Kind::Directory, 0))),
        (dead, None),
    ];
    let base = vec![entry("a.txt", file, 6, 1), entry("d/", dir, 0, 0)];
    let with = |extra: PathEntry| {
        let mut v = base.clone();
        v.push(extra);
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    };
    assert!(validate_with(&base, &records).is_ok());
    let fine = [
        entry("g/", dead, 0, 0),
        entry("a.txt/x", dead, 0, 0),
        entry("e", file, 6, 9),
        entry("l", link, 3, 0),
    ];
    for e in fine {
        assert!(validate_with(&with(e.clone()), &records).is_ok(), "{e:?}");
    }
    let broken = [
        entry("e", file, 7, 1),       // size differs
        entry("e", dead, 0, 5),       // tombstone with mtime
        entry("e", dead, 1, 0),       // tombstone with size
        entry("e", dir, 0, 0),        // directory without /
        entry("e/", file, 6, 1),      // file with /
        entry("a.txt/x", file, 6, 1), // below a live file
        entry("d/x", file, 6, 1),     // below a live empty directory
        entry("a.txt/", dir, 0, 0),   // empty directory below a file
        entry("d/e/", dir, 0, 0),     // empty directory below one
        entry("l", link, 3, 1),       // a link with an mtime
    ];
    for e in broken {
        assert!(validate_with(&with(e.clone()), &records).is_err(), "{e:?}");
    }
    let missing = validate_with(&with(entry("m", h(b"m"), 1, 1)), &records);
    assert!(matches!(missing, Err(crate::Error::NotFound(_))));
}

#[test]
fn kinds() {
    let r = |state: &[u8]| Record {
        change: Vec::new(),
        parents: vec![],
        root: h(b"r"),
        state: state.to_vec(),
    };
    assert_eq!(Kind::of(&r(&[1])).unwrap(), Some(Kind::Executable));
    assert_eq!(
        Kind::of(&r(&Kind::Directory.state())).unwrap(),
        Some(Kind::Directory)
    );
    assert_eq!(Kind::of(&Record::tombstone(vec![h(b"p")])).unwrap(), None);
    for bad in [&[][..], &[4], &[0, 0]] {
        assert!(Kind::of(&r(bad)).is_err(), "{bad:?}");
    }
}
