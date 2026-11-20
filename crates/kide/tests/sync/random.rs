//! Three devices converge after random edits, deletions, renames, mode
//! changes, touches, links and directories, and no side of a conflict is
//! lost. 20 seeds by default; `KIDE_SEEDS=n` runs n, `KIDE_SEED=s` one.

use crate::device::{Device, Net, assert_same};
use kide::conformance::Rng;
use kide::folder::plan::is_ancestor;
use kide::format::manifest::Kind;
use kide::hash::Hash;
use kide::iface::{Decision, History, Outcome, ReadContent, Resolver, Side};
use kide::resolve::DecisionTable;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;

const DIRS: [&str; 4] = ["", "a/", "a/b/", "c/"];
const NAMES: [&str; 5] = ["f0", "f1", "f2.md", "f3", "a"];
const MAX_ROUNDS: usize = 4;

/// The decision table, keeping the leaves of every new merge.
#[derive(Default)]
struct Recording(Mutex<Vec<Hash>>);

impl Resolver for Recording {
    fn decide(
        &self,
        path: &str,
        local: Option<&Side>,
        remote: Option<&Side>,
        history: &mut History,
        content: &mut ReadContent,
    ) -> kide::Result<Decision> {
        let d = DecisionTable.decide(path, local, remote, history, content)?;
        if let Outcome::Merge(m) = &d.outcome {
            self.0.lock().unwrap().extend(&m.parents);
        }
        Ok(d)
    }
}

fn random_path(rng: &mut Rng) -> String {
    let dir = DIRS[rng.below(4) as usize];
    format!("{dir}{}", NAMES[rng.below(5) as usize])
}

fn content(rng: &mut Rng) -> Vec<u8> {
    match rng.below(4) {
        0 => {
            let n = rng.below(6000) as usize;
            rng.bytes(n)
        }
        _ => format!("line {}\n", rng.next_u64())
            .repeat(1 + rng.below(20) as usize)
            .into_bytes(),
    }
}

/// Whether a file may be created at `path`: no parent is a file or link,
/// and nothing is there but perhaps an empty directory, which is removed.
fn can_create(root: &Path, path: &str) -> bool {
    let mut prefix = root.to_path_buf();
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        prefix.push(part);
        if fs::symlink_metadata(&prefix).is_ok_and(|m| !m.is_dir()) {
            return false;
        }
    }
    let p = root.join(path);
    match fs::symlink_metadata(&p) {
        Err(_) => true,
        Ok(m) => m.is_dir() && fs::remove_dir(&p).is_ok(),
    }
}

/// One random operation on one device's files.
fn operate(rng: &mut Rng, dev: &Device) {
    let root = dev.root();
    let files: Vec<String> = dev
        .files()
        .into_iter()
        .filter(|(p, f)| f.0 != Kind::Directory && !p.ends_with('/'))
        .map(|(p, _)| p)
        .collect();
    let pick = |rng: &mut Rng| match files.len() {
        0 => None,
        n => Some(files[rng.below(n as u64) as usize].clone()),
    };
    let is_file = |p: &str| fs::symlink_metadata(root.join(p)).is_ok_and(|m| m.is_file());
    match rng.below(12) {
        0 | 1 => {
            let p = random_path(rng);
            if can_create(root, &p) {
                dev.write(&p, &content(rng));
            }
        }
        2 | 3 => {
            if let Some(p) = pick(rng).filter(|p| is_file(p)) {
                dev.write(&p, &content(rng));
            }
        }
        4 => {
            if let Some(p) = pick(rng).filter(|p| is_file(p)) {
                let mut data = fs::read(root.join(&p)).unwrap();
                data.extend(b"appended\n");
                dev.write(&p, &data);
            }
        }
        5 => {
            if let Some(p) = pick(rng) {
                dev.remove(&p);
            }
        }
        6 => {
            let to = random_path(rng);
            if let Some(p) = pick(rng)
                && p != to
                && can_create(root, &to)
            {
                fs::create_dir_all(root.join(&to).parent().unwrap()).unwrap();
                fs::rename(root.join(&p), root.join(&to)).unwrap();
            }
        }
        7 => {
            if let Some(p) = pick(rng).filter(|p| is_file(p)) {
                let mode = [0o755, 0o644][rng.below(2) as usize];
                fs::set_permissions(root.join(&p), fs::Permissions::from_mode(mode)).unwrap();
            }
        }
        8 => {
            if let Some(p) = pick(rng).filter(|p| is_file(p)) {
                let t = std::time::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_500_000_000 + rng.below(3600));
                let f = fs::File::options().write(true).open(root.join(&p)).unwrap();
                f.set_modified(t).unwrap();
            }
        }
        9 => {
            let p = random_path(rng);
            if can_create(root, &p) {
                dev.put(&p, Kind::Symlink, NAMES[rng.below(5) as usize].as_bytes());
            }
        }
        10 => {
            let p = random_path(rng);
            if can_create(root, &p) {
                dev.put(&format!("{p}/"), Kind::Directory, b"");
            }
        }
        _ => {
            // A file replaced by a directory of the same name.
            if let Some(p) = pick(rng).filter(|p| !p.contains('/')) {
                dev.remove(&p);
                dev.write(&format!("{p}/child"), &content(rng));
            }
        }
    }
}

fn no_tmp(dir: &Path) {
    for e in fs::read_dir(dir).unwrap().flatten() {
        let name = e.file_name();
        assert!(
            !name.to_string_lossy().ends_with(".kide-tmp"),
            "{:?}",
            e.path()
        );
        if e.file_type().unwrap().is_dir() {
            no_tmp(&e.path());
        }
    }
}

/// Whether `id`, or a record descending from it, is some path's version: a
/// conflict's sides survive unless a later change made with knowledge of
/// them replaced them.
fn survives(dev: &Device, id: &Hash) -> bool {
    let store = &*dev.folder.store;
    let index = dev.folder.index.0.values();
    index
        .map(|e| e.version)
        .any(|v| v == *id || is_ancestor(store, id, &v).unwrap())
}

fn two(devs: &mut [Device], i: usize, j: usize) -> (&mut Device, &mut Device) {
    assert_ne!(i, j);
    if i < j {
        let (l, r) = devs.split_at_mut(j);
        (&mut l[i], &mut r[0])
    } else {
        let (l, r) = devs.split_at_mut(i);
        (&mut r[0], &mut l[j])
    }
}

fn run(seed: u64) -> usize {
    let mut rng = Rng::new(seed);
    let net = Net::default();
    let mut devs: Vec<Device> = ["a", "b", "c"].map(Device::new).into();
    for d in &devs {
        net.join(d);
    }
    for _ in 0..20 + rng.below(41) {
        let i = rng.below(3) as usize;
        operate(&mut rng, &devs[i]);
        match rng.below(10) {
            0..=2 => drop(devs[i].scan()),
            3 | 4 => {
                let j = (i + 1 + rng.below(2) as usize) % 3;
                let (to, from) = two(&mut devs, i, j);
                net.pull(to, from);
            }
            _ => {}
        }
    }
    let recorder = Recording::default();
    let mut rounds = 0;
    while rounds == 0 || devs.iter().any(|d| d.head() != devs[0].head()) {
        assert!(rounds < MAX_ROUNDS, "no convergence after {rounds} rounds");
        rounds += 1;
        for i in 0..3 {
            let (to, from) = two(&mut devs, i, (i + 1) % 3);
            from.scan();
            to.try_pull_with(&net.0, &from.name, &recorder)
                .unwrap_or_else(|e| panic!("round {rounds}: {} pulls {}: {e}", to.name, from.name));
        }
    }
    for d in &mut devs {
        let v = kide::folder::verify::verify(&*d.folder.store, &d.head()).unwrap();
        assert!(v.ok(), "{} fails verify: {v:?}", d.name);
        no_tmp(d.root());
        let head = d.head();
        assert_eq!(
            d.scan().head,
            head,
            "{}: a scan after converging changes nothing",
            d.name
        );
    }
    let [a, b, c] = &devs[..] else { unreachable!() };
    assert_same(&[a, b, c]);
    for id in recorder.0.lock().unwrap().iter() {
        assert!(survives(a, id), "a conflict side was lost: {id}");
    }
    rounds
}

#[test]
fn random_scenarios_converge() {
    let one = std::env::var("KIDE_SEED").ok().and_then(|s| s.parse().ok());
    let seeds: Vec<u64> = match one {
        Some(s) => vec![s],
        None => {
            let n = std::env::var("KIDE_SEEDS")
                .ok()
                .and_then(|s| s.parse().ok());
            // Twenty seeds take seconds: the light suite runs two.
            let full = kide::conformance::full_tests();
            (0..n.unwrap_or(if full { 20 } else { 2 })).collect()
        }
    };
    let mut failed = Vec::new();
    let mut most = 0;
    for &seed in &seeds {
        match std::panic::catch_unwind(|| run(seed)) {
            Ok(rounds) => most = most.max(rounds),
            Err(_) => {
                eprintln!("seed {seed} failed; rerun with KIDE_SEED={seed}");
                failed.push(seed);
            }
        }
    }
    eprintln!("{} seeds, at most {most} rounds to converge", seeds.len());
    assert!(
        failed.is_empty(),
        "{} of {} seeds failed: {failed:?}",
        failed.len(),
        seeds.len()
    );
}
