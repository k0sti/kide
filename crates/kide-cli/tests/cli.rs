//! The commands of the `kide` binary: init, scan, ls, log, verify, checkout,
//! gc, serve, pull and run.
//! Unix only: the fixtures use symbolic links and permission bits.
#![cfg(unix)]

use kide::conformance::TempDir;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn kide(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kide"))
        .args(args)
        .output()
        .unwrap()
}

fn ok(args: &[&str]) -> String {
    let out = kide(args);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "kide {args:?}: {err}");
    String::from_utf8(out.stdout).unwrap()
}

fn field(out: &str, key: &str) -> String {
    let value = out.lines().find_map(|l| l.strip_prefix(&format!("{key} ")));
    value
        .unwrap_or_else(|| panic!("{key} missing from:\n{out}"))
        .to_string()
}

/// Writes a file with an old mtime, outside the racy window.
fn write(dir: &Path, path: &str, data: &[u8]) {
    let p = dir.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, data).unwrap();
    let f = fs::File::options().write(true).open(&p).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000))
        .unwrap();
}

#[test]
fn init_scan_ls_verify_checkout() {
    let tmp = TempDir::new("cli");
    let d = tmp.path().to_str().unwrap();
    ok(&["init", d]);
    assert_eq!(kide(&["init", d]).status.code(), Some(1), "init twice");
    write(tmp.path(), "a.txt", b"one\n");
    write(tmp.path(), "sub/b.bin", b"\0binary");
    fs::create_dir(tmp.path().join("empty")).unwrap();
    let first = ok(&["-C", d, "scan"]);
    assert_eq!(field(&first, "versions"), "3");
    std::os::unix::fs::symlink("a.txt", tmp.path().join("link")).unwrap();
    fs::remove_file(tmp.path().join("sub/b.bin")).unwrap();
    let second = ok(&["-C", d, "scan"]);
    assert_eq!(
        (field(&second, "versions"), field(&second, "tombstones")),
        ("2".into(), "1".into())
    );
    let ls = ok(&["-C", d, "ls"]);
    let rows: Vec<(String, String)> = ls
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            (f[0].to_string(), f[4..].join(" "))
        })
        .collect();
    let want = [
        ("file", "a.txt"),
        ("dir", "empty/"),
        ("link", "link"),
        ("dir", "sub/"),
        ("deleted", "sub/b.bin"),
    ];
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(k, p)| (k.to_string(), p.to_string()))
        .collect();
    assert_eq!(rows, want, "{ls}");
    assert_eq!(ok(&["-C", d, "ls", "--live"]).lines().count(), 4);
    assert!(ok(&["-C", d, "verify"]).starts_with("objects "));

    let out = TempDir::new("cli-out");
    let target = out.path().join("copy");
    let t = target.to_str().unwrap();
    assert_eq!(
        field(&ok(&["-C", d, "checkout", "HEAD", t]), "written"),
        "4"
    );
    assert_eq!(fs::read(target.join("a.txt")).unwrap(), b"one\n");
    assert_eq!(
        fs::read_link(target.join("link")).unwrap(),
        Path::new("a.txt")
    );
    assert!(target.join("sub").is_dir() && target.join("empty").is_dir());
    let head = field(&second, "head");
    assert_eq!(
        kide(&["-C", d, "checkout", &head, t]).status.code(),
        Some(1),
        "not empty"
    );
}

#[test]
fn log_shows_a_path_newest_first() {
    let tmp = TempDir::new("cli");
    let d = tmp.path().to_str().unwrap();
    ok(&["init", d]);
    write(tmp.path(), "a.txt", b"one\n");
    ok(&["-C", d, "scan"]);
    write(tmp.path(), "a.txt", b"one two\n");
    ok(&["-C", d, "scan"]);
    fs::remove_file(tmp.path().join("a.txt")).unwrap();
    let refused = kide(&["-C", d, "scan"]);
    assert_eq!(
        refused.status.code(),
        Some(1),
        "the folder is empty: guarded"
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("refused"));
    ok(&["-C", d, "restore"]);
    assert_eq!(fs::read(tmp.path().join("a.txt")).unwrap(), b"one two\n");
    fs::remove_file(tmp.path().join("a.txt")).unwrap();
    ok(&["-C", d, "confirm"]);
    ok(&["-C", d, "scan"]);
    let log = ok(&["-C", d, "log", "a.txt"]);
    let rows: Vec<Vec<&str>> = log.lines().map(|l| l.split(' ').collect()).collect();
    let shape: Vec<[&str; 3]> = rows.iter().map(|r| [r[0], r[3], r[4]]).collect();
    assert_eq!(
        shape,
        [["*", "delete", "-"], ["*", "edit", "8"], ["*", "new", "4"]],
        "{log}"
    );
    let version = ok(&["-C", d, "ls"]).split(' ').nth(3).unwrap().to_string();
    assert!(version.starts_with(rows[0][2]), "{log}");
    assert_eq!(kide(&["-C", d, "log", "b.txt"]).status.code(), Some(1));
    assert_eq!(kide(&["-C", d, "log"]).status.code(), Some(2));
}

#[test]
fn usage_errors() {
    assert_eq!(kide(&[]).status.code(), Some(2));
    assert_eq!(kide(&["frobnicate"]).status.code(), Some(2));
    assert_eq!(kide(&["scan", "extra"]).status.code(), Some(2));
    assert_eq!(kide(&["ls", "--colour"]).status.code(), Some(2));
    let tmp = TempDir::new("cli");
    let d = tmp.path().to_str().unwrap();
    assert_eq!(
        kide(&["-C", d, "scan"]).status.code(),
        Some(1),
        "not a Kide folder"
    );
    ok(&["init", d]);
    assert_eq!(
        kide(&["-C", d, "checkout", "nothex", "x"]).status.code(),
        Some(2)
    );
}

#[test]
fn serve_answers_a_fetch() {
    use kide::fetch::fetch;
    use kide::folder::net::Manifests;
    use kide::iface::Transport;
    use kide::store::MemStore;
    use kide::transport::Tcp;

    let tmp = TempDir::new("cli-serve");
    let d = tmp.path().to_str().unwrap();
    ok(&["init", d]);
    write(tmp.path(), "a.txt", b"served\n");
    let head = field(&ok(&["-C", d, "scan"]), "head");
    let [this, other] = [1u8, 2].map(key);
    let config =
        format!("transport = plain\nself = {this} 127.0.0.1:0\ndevice = {other} 127.0.0.1:1\n");
    fs::write(tmp.path().join(".kide/config"), config).unwrap();
    let (child, line) = serve(d);
    let addr = line.trim().rsplit(' ').next().unwrap().to_string();
    assert_eq!(line, format!("serving {head} on {addr}\n"));

    let mut s = Tcp::default().connect(&addr).unwrap();
    let root = s.hello(&kide::Hash::ZERO).unwrap();
    assert_eq!(root.to_hex(), head);
    let store = MemStore::new();
    let staging = kide::store::PackStore::open(tmp.path().join("client-staging")).unwrap();
    fetch(&mut *s, &store, &staging, &Manifests, &root, &[]).unwrap();
    s.bye().unwrap();
    drop(child);
    let r = kide::folder::verify::verify(&store, &root).unwrap();
    assert!(r.ok(), "{r:?}");
}

fn key(i: u8) -> kide::folder::npub::Key {
    kide::folder::npub::Key(kide::hash::h(&[i]).0)
}

/// Kills the server even when the test fails.
struct Killed(std::process::Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `kide serve` in the folder `dir`; returns it and its first line.
fn serve(dir: &str) -> (Killed, String) {
    start(dir, "serve")
}

/// Starts a long-running command in the folder `dir`; returns it and its
/// first line.
fn start(dir: &str, cmd: &str) -> (Killed, String) {
    use std::io::{BufRead, BufReader};
    let mut child = Killed(
        Command::new(env!("CARGO_BIN_EXE_kide"))
            .args(["-C", dir, cmd])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(child.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    (child, line)
}

#[test]
fn pull_from_served_devices() {
    use std::os::unix::ffi::OsStrExt;
    let tmp = TempDir::new("cli-pull");
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    let (da, db) = (a.to_str().unwrap(), b.to_str().unwrap());
    ok(&["init", da]);
    ok(&["init", db]);
    write(&b, "from-b/notes.md", b"written on b\n");
    ok(&["-C", db, "scan"]);
    let [ka, kb, kc, stranger] = [11u8, 12, 13, 14].map(key);
    let config = format!("transport = plain\nself = {kb} 127.0.0.1:0\ndevice = {ka} 127.0.0.1:1\n");
    fs::write(b.join(".kide/config"), config).unwrap();
    let (_server, line) = serve(db);
    let addr = line.trim().rsplit(' ').next().unwrap().to_string();
    // c is configured but not running.
    let config = format!(
        "transport = plain\nself = {ka} 127.0.0.1:1\ndevice = {kb} {addr}\ndevice = {kc} 127.0.0.1:9\n"
    );
    fs::write(a.join(".kide/config"), config).unwrap();
    // A name scanning skips, reported once although each pull scans.
    // Some file systems (APFS) refuse such a name; then nothing is skipped.
    let bad = fs::write(a.join(std::ffi::OsStr::from_bytes(b"bad-\xff")), b"x").is_ok();
    let out = Command::new(env!("CARGO_BIN_EXE_kide"))
        .args(["-C", da, "pull"])
        .env_remove("KIDE_LOG")
        .output()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(out.status.code(), Some(1), "c is unreachable: {stderr}");
    assert!(
        stdout.contains(&format!("pulled {kb}: decided 1 written 1 ")),
        "{stdout}"
    );
    assert!(
        stderr.contains(&format!("pull from {kc}: unavailable")),
        "{stderr}"
    );
    if bad {
        assert_eq!(
            stderr.matches("skipped \"bad-\u{fffd}\"").count(),
            1,
            "{stderr}"
        );
        assert!(stdout.contains("skipped bad-\u{fffd}\n"), "{stdout}");
    }
    assert_eq!(
        fs::read(a.join("from-b/notes.md")).unwrap(),
        b"written on b\n"
    );
    // One device by key; an unknown one; something that is no key.
    let again = ok(&["-C", da, "pull", &kb.to_string()]);
    assert!(again.contains("decided 0"), "{again}");
    let unknown = kide(&["-C", da, "pull", &stranger.to_string()]);
    assert_eq!(unknown.status.code(), Some(1));
    assert_eq!(kide(&["-C", da, "pull", "nokey"]).status.code(), Some(2));
}

/// Every entry below `dir`: path, size, mtime and inode.
fn snapshot(dir: &Path) -> Vec<(String, u64, std::time::SystemTime, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let m = fs::symlink_metadata(e.path()).unwrap();
        let name = e.file_name().into_string().unwrap();
        out.push((name.clone(), m.len(), m.modified().unwrap(), m.ino()));
        if m.is_dir() {
            let inner = snapshot(&e.path()).into_iter();
            out.extend(inner.map(|(p, l, t, i)| (format!("{name}/{p}"), l, t, i)));
        }
    }
    out.sort();
    out
}

#[test]
fn a_send_folder_never_writes_its_files_and_a_mirror_pulls_them() {
    let tmp = TempDir::new("cli-send");
    let (vault, s, m) = (
        tmp.path().join("vault"),
        tmp.path().join("send"),
        tmp.path().join("mirror"),
    );
    let (ds, dm) = (s.to_str().unwrap(), m.to_str().unwrap());
    write(&vault, "notes/a.md", b"from the vault\n");
    write(&vault, "e\u{301}.md", b"not NFC\n");
    write(&vault, ".obsidian/app.json", b"{\"setting\": 1}\n");
    let before = snapshot(&vault);
    ok(&["init", ds]);
    ok(&["init", dm]);
    let (ks, km) = (key(21), key(22));
    let config = format!(
        "folder = {}\nmode = send\ntransport = plain\nself = {ks} 127.0.0.1:0\n\
         device = {km} 127.0.0.1:9\n",
        vault.display()
    );
    fs::write(s.join(".kide/config"), config).unwrap();
    let out = ok(&["-C", ds, "scan"]);
    assert!(out.contains("versions 2\n"), "{out}");
    assert!(out.contains("skipped e\u{301}.md\n"), "{out}");
    let refused = kide(&["-C", ds, "pull"]);
    assert_eq!(refused.status.code(), Some(1));
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(err.contains("send"), "{err}");
    let (_daemon, line) = start(ds, "run");
    let addr = line.trim().rsplit(' ').next().unwrap().to_string();
    // The mirror ignores Obsidian's own configuration: never scanned, and
    // the sender's entries there are held, not written.
    let config = format!(
        "transport = plain\nself = {km} 127.0.0.1:9\ndevice = {ks} {addr}\nignore = .obsidian\n"
    );
    fs::write(m.join(".kide/config"), config).unwrap();
    write(&m, ".obsidian/workspace.json", b"{}\n");
    let out = ok(&["-C", dm, "pull"]);
    assert!(
        out.contains(&format!("pulled {ks}: decided 2 written 1 ")),
        "{out}"
    );
    assert_eq!(fs::read(m.join("notes/a.md")).unwrap(), b"from the vault\n");
    assert!(!m.join(".obsidian/app.json").exists());
    write(&m, ".obsidian/workspace.json", b"{\"changed\": 1}\n");
    let out = ok(&["-C", dm, "scan"]);
    assert!(out.contains("versions 0\n"), "{out}");
    assert!(
        !out.contains("held"),
        "ignored paths are not reported: {out}"
    );
    let out = ok(&["-C", dm, "pull"]);
    assert!(out.contains("decided 0"), "{out}");
    assert_eq!(
        fs::read(m.join(".obsidian/workspace.json")).unwrap(),
        b"{\"changed\": 1}\n"
    );
    assert_eq!(
        snapshot(&vault),
        before,
        "the send folder's files are untouched"
    );
}

#[test]
fn gc_runs_and_is_refused_while_the_daemon_runs() {
    let tmp = TempDir::new("cli-gc");
    let d = tmp.path().to_str().unwrap();
    ok(&["init", d]);
    write(tmp.path(), "a.txt", b"one\n");
    ok(&["-C", d, "scan"]);
    write(tmp.path(), "a.txt", b"two\n");
    let head = field(&ok(&["-C", d, "scan"]), "head");
    let out = ok(&["-C", d, "gc"]);
    assert_eq!(field(&out, "removed"), "0", "retained roots keep it all");
    assert_eq!(field(&out, "roots dropped"), "0");
    let config = format!(
        "transport = plain\nself = {} 127.0.0.1:0\ndevice = {} 127.0.0.1:9\n",
        key(1),
        key(2)
    );
    fs::write(tmp.path().join(".kide/config"), config).unwrap();
    let (daemon, line) = start(d, "run");
    assert!(
        line.starts_with(&format!("running {head} on 127.0.0.1:")),
        "{line}"
    );
    let refused = kide(&["-C", d, "gc"]);
    assert_eq!(refused.status.code(), Some(1));
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(err.contains("locked"), "{err}");
    drop(daemon);
    ok(&["-C", d, "gc"]);
}

#[cfg(feature = "fips")]
#[test]
fn run_in_fips_states_the_address_rule() {
    let tmp = TempDir::new("cli-fips");
    let d = tmp.path().to_str().unwrap();
    ok(&["init", d]);
    let config = format!("transport = fips\nself = {}\ndevice = {}\n", key(1), key(2));
    fs::write(tmp.path().join(".kide/config"), config).unwrap();
    // This host holds no FIPS address, so listening fails after the warning.
    let out = kide(&["-C", d, "run"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    let rule = "must drop packets addressed to its own FIPS address";
    assert!(err.contains(rule), "{err}");
    let own = kide::transport::fips::address(&key(1).0);
    assert!(
        err.contains(&format!("ip6 daddr {own} iifname != ")),
        "{err}"
    );
    let help = String::from_utf8_lossy(&kide(&[]).stderr).into_owned();
    let wrapped = "own FIPS address that arrive on any interface other than the FIPS one";
    assert!(help.contains(wrapped), "{help}");
}

#[test]
fn quic_sync_requires_the_configured_device_and_certificate() {
    let (a, b) = (TempDir::new("quic-cli-a"), TempDir::new("quic-cli-b"));
    let (ad, bd) = (a.path().to_str().unwrap(), b.path().to_str().unwrap());
    let (ai, bi) = (ok(&["init", ad]), ok(&["init", bd]));
    let (ak, bk) = (field(&ai, "key"), field(&bi, "key"));
    let ap = field(&ai, "certificate");
    write(a.path(), "hello.txt", b"authenticated\n");
    ok(&["-C", ad, "scan"]);
    fs::write(
        a.path().join(".kide/config"),
        format!(
            "self = {ak} 127.0.0.1:0\ndevice = {bk} 127.0.0.1:1 {}\n",
            field(&bi, "certificate")
        ),
    )
    .unwrap();
    let mut server = Command::new(env!("CARGO_BIN_EXE_kide"))
        .args(["-C", ad, "serve"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::BufRead;
    let mut line = String::new();
    std::io::BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let addr = line.split_whitespace().last().unwrap();
    let config =
        |pin: &str, own: &str| format!("self = {own} 127.0.0.1:1\ndevice = {ak} {addr} {pin}\n");
    fs::write(b.path().join(".kide/config"), config(&"00".repeat(32), &bk)).unwrap();
    assert!(!kide(&["-C", bd, "pull"]).status.success(), "wrong TLS pin");
    fs::write(b.path().join(".kide/config"), config(&ap, &ak)).unwrap();
    assert!(
        !kide(&["-C", bd, "pull"]).status.success(),
        "self mismatches secret"
    );
    let stranger = TempDir::new("quic-cli-stranger");
    let sd = stranger.path().to_str().unwrap();
    let sk = field(&ok(&["init", sd]), "key");
    fs::write(stranger.path().join(".kide/config"), config(&ap, &sk)).unwrap();
    assert!(
        !kide(&["-C", sd, "pull"]).status.success(),
        "unadmitted key on the same source IP"
    );
    fs::write(b.path().join(".kide/config"), config(&ap, &bk)).unwrap();
    let out = kide(&["-C", bd, "pull"]);
    server.kill().unwrap();
    server.wait().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(b.path().join("hello.txt")).unwrap(),
        b"authenticated\n"
    );
}

#[test]
fn renewing_tls_preserves_the_device_key() {
    let tmp = TempDir::new("cli-renew");
    let dir = tmp.path().to_str().unwrap();
    let before = ok(&["init", dir]);
    let key = fs::read(tmp.path().join(".kide/device.key")).unwrap();
    let after = ok(&["-C", dir, "renew-tls"]);
    assert_ne!(field(&before, "certificate"), field(&after, "certificate"));
    assert_eq!(fs::read(tmp.path().join(".kide/device.key")).unwrap(), key);
    use std::os::unix::fs::PermissionsExt;
    for name in ["device.key", "tls.key"] {
        assert_eq!(
            fs::metadata(tmp.path().join(".kide").join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn explicit_move_and_forward_tree_restore() {
    let tmp = TempDir::new("cli-move-restore");
    let dir = tmp.path().to_str().unwrap();
    ok(&["init", dir]);
    write(tmp.path(), "old/sub/a.txt", b"earlier\n");
    let earlier = field(&ok(&["-C", dir, "scan"]), "head");
    ok(&["-C", dir, "move", "old/", "new/"]);
    let log = ok(&["-C", dir, "log", "new/sub/a.txt"]);
    assert!(
        log.contains("move") && log.contains("old/") && log.contains("new/"),
        "{log}"
    );
    write(tmp.path(), "new/sub/a.txt", b"later\n");
    ok(&["-C", dir, "scan"]);
    assert_eq!(
        kide(&["-C", dir, "restore-tree", &earlier]).status.code(),
        Some(2),
        "replacement requires an explicit choice"
    );
    let restored = ok(&["-C", dir, "restore-tree", &earlier, "--replace"]);
    assert_ne!(field(&restored, "head"), earlier);
    assert_eq!(
        fs::read(tmp.path().join("old/sub/a.txt")).unwrap(),
        b"earlier\n"
    );
    assert!(!tmp.path().join("new/sub/a.txt").exists());
}
