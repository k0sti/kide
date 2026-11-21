//! `.kide/config` (§5.8): one `key = value` per line, `#` starts a comment.
//! `device`, `ignore` and `sync` may repeat; every other key appears at
//! most once.

use super::npub::Key;
use super::rules::Rules;
use crate::error::{Result, invalid};
use crate::format::W;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// TLS with pinned certificates and device-key challenges: the default.
    Quic,
    /// Experimental, authenticated by FIPS (§4.2).
    Fips,
    /// Unauthenticated, for local testing only.
    Plain,
}

/// What a device does with its folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Scans, serves and pulls: the default.
    Sync,
    /// Scans and serves, never pulls, and never writes into the folder.
    Send,
}

/// A device: its key, and for QUIC/plain its address and port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Device {
    pub key: Key,
    pub addr: Option<SocketAddr>,
    /// SHA-256 of the peer TLS certificate, required for QUIC devices.
    pub certificate: Option<crate::hash::Hash>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The synchronised directory, if not the one holding `.kide/`; a
    /// relative path is taken from that one.
    pub folder: Option<PathBuf>,
    pub mode: Mode,
    /// The `ignore` and `sync` rules: what this device neither scans nor
    /// writes; the entries others send there are held (§5.3).
    pub rules: Rules,
    pub transport: Transport,
    /// This device; required by the network commands only.
    pub this: Option<Device>,
    pub devices: Vec<Device>,
    pub port: u16,
    pub scan_interval: u64,
    pub pull_interval: u64,
    pub trash_days: u64,
    pub keep_days: u64,
    /// Whether a scan refuses to record a lost or mostly deleted folder
    /// (§5.5).
    pub guard: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            folder: None,
            mode: Mode::Sync,
            rules: Rules::default(),
            transport: Transport::Quic,
            this: None,
            devices: Vec::new(),
            port: 7839,
            scan_interval: 10,
            pull_interval: 10,
            trash_days: 30,
            keep_days: 30,
            guard: true,
        }
    }
}

/// What `kide init` writes: every key, commented out, at its default.
pub const TEMPLATE: &str = "# Kide folder configuration (SPEC.md section 5.8)
# folder        = <dir>     # the directory synchronised, if not this one
# mode          = sync      # or send: never pull, never write into the folder
# ignore        = <pattern> # neither scanned nor written here, repeatable: e.g. .obsidian
ignore        = .git        # a Git repository breaks when two devices write it
# sync          = <pattern> # synced although an earlier ignore covers it: e.g. .obsidian/*.json
# transport     = quic      # or fips (experimental), plain (local tests only)
# self          = npub1... [::1]:7840 # this device; address for quic/plain
# device        = npub1... [::1]:7841 <certificate-sha256> # QUIC peer, repeatable
# port          = 7839      # fips: the port on every device's FIPS address
# scan_interval = 10        # seconds
# pull_interval = 10        # seconds
# trash_days    = 30
# keep_days     = 30
# guard         = on        # or off: record a lost or mostly deleted folder unasked
";

impl Config {
    /// Whether `path` is ignored by the rules (§5.8).
    pub fn ignores(&self, path: &str) -> bool {
        self.rules.ignores(path)
    }

    pub fn parse(text: &str) -> Result<Config> {
        let mut c = Config::default();
        let mut seen: Vec<&str> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let at = |m: String| invalid(format!("config line {}: {m}", n + 1));
            let Some((key, value)) = line.split_once('=') else {
                return at("expected key = value".into());
            };
            let (key, value) = (key.trim(), value.trim());
            if key.is_empty() || value.is_empty() {
                return at("empty key or value".into());
            }
            if !["device", "ignore", "sync"].contains(&key) && seen.contains(&key) {
                return at(format!("{key} appears twice"));
            }
            seen.push(key);
            if let Err(e) = c.set(key, value) {
                return at(e);
            }
        }
        c.check()?;
        Ok(c)
    }

    fn set(&mut self, key: &str, value: &str) -> std::result::Result<(), String> {
        let number = |min: u64| match value.parse::<u64>() {
            Ok(v) if v >= min => Ok(v),
            _ => Err(format!("{key} is not a number of at least {min}")),
        };
        match key {
            "folder" => self.folder = Some(PathBuf::from(value)),
            "mode" => {
                self.mode = match value {
                    "sync" => Mode::Sync,
                    "send" => Mode::Send,
                    _ => return Err(format!("unknown mode {value:?}")),
                }
            }
            "ignore" => self.rules.add(false, value)?,
            "sync" => self.rules.add(true, value)?,
            "transport" => {
                self.transport = match value {
                    "quic" => Transport::Quic,
                    "fips" => Transport::Fips,
                    "plain" => Transport::Plain,
                    _ => return Err(format!("unknown transport {value:?}")),
                }
            }
            "self" => self.this = Some(device(value)?),
            "device" => self.devices.push(device(value)?),
            "port" => match value.parse::<u16>() {
                Ok(p) if p > 0 => self.port = p,
                _ => return Err(format!("bad port {value:?}")),
            },
            "scan_interval" => self.scan_interval = number(1)?,
            "pull_interval" => self.pull_interval = number(1)?,
            "trash_days" => self.trash_days = number(0)?,
            "keep_days" => self.keep_days = number(0)?,
            "guard" => {
                self.guard = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err(format!("guard is on or off, not {value:?}")),
                }
            }
            _ => return Err(format!("unknown key {key}")),
        }
        Ok(())
    }

    /// The rules across lines: addresses exactly in the plain transport, at
    /// most W − 1 devices (§3.1), each once and none of them this device.
    fn check(&self) -> Result<()> {
        let addressed = self.transport != Transport::Fips;
        for d in self.this.iter().chain(&self.devices) {
            if d.addr.is_some() != addressed {
                return invalid(match addressed {
                    true => format!(
                        "config: {} needs an address with transport quic/plain",
                        d.key
                    ),
                    false => format!("config: {} has an address, but transport is fips", d.key),
                });
            }
        }
        for d in &self.devices {
            if (self.transport == Transport::Quic) != d.certificate.is_some() {
                return invalid(
                    "config: QUIC devices require a certificate hash; other transports do not",
                );
            }
        }
        if self.devices.len() > W - 1 {
            return invalid(format!(
                "config: {} devices, at most {}",
                self.devices.len(),
                W - 1
            ));
        }
        for (i, d) in self.devices.iter().enumerate() {
            if self.devices[..i].iter().any(|e| e.key == d.key) {
                return invalid(format!("config: device {} appears twice", d.key));
            }
            if self.this.is_some_and(|t| t.key == d.key) {
                return invalid(format!("config: device {} is this device", d.key));
            }
        }
        Ok(())
    }
}

/// `npub1…`, optionally followed by an address and port.
fn device(value: &str) -> std::result::Result<Device, String> {
    let mut parts = value.split_whitespace();
    let key = parts.next().unwrap_or("");
    let key = Key::from_npub(key).map_err(|e| e.to_string())?;
    let addr = match parts.next() {
        None => None,
        Some(a) => Some(a.parse().map_err(|_| format!("bad address {a:?}"))?),
    };
    let certificate = parts
        .next()
        .map(crate::hash::Hash::from_hex)
        .transpose()
        .map_err(|_| "bad certificate hash".to_string())?;
    match parts.next() {
        Some(extra) => Err(format!("unexpected {extra:?}")),
        None => Ok(Device {
            key,
            addr,
            certificate,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: u8) -> Key {
        Key(crate::hash::h(&[i]).0)
    }

    #[test]
    fn quic_is_default_and_requires_peer_certificate_pins() {
        assert_eq!(Config::default().transport, Transport::Quic);
        let text = format!(
            "self = {} 127.0.0.1:7840\ndevice = {} 127.0.0.1:7841 {}\n",
            key(0),
            key(1),
            crate::hash::Hash([3; 32])
        );
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.devices[0].certificate, Some(crate::hash::Hash([3; 32])));
        assert!(Config::parse(&format!("device = {} 127.0.0.1:7841", key(1))).is_err());
    }

    #[test]
    fn template_ignores_git_repositories_and_is_otherwise_default() {
        let c = Config::parse(TEMPLATE).unwrap();
        assert!(c.ignores(".git") && c.ignores(".git/HEAD"));
        assert!(!c.ignores("notes.md") && !c.ignores(".gitignore"));
        let without = Config::parse(&TEMPLATE.replace("ignore        = .git", "")).unwrap();
        assert_eq!(without, Config::default());
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn fips_values() {
        let text = format!(
            "transport = fips\nself = {}  # laptop\ndevice = {}\ndevice={}\n\n  # comment\nport = 9000\n\
             scan_interval = 3\npull_interval = 4\ntrash_days = 0\nkeep_days = 7\n",
            key(0),
            key(1),
            key(2)
        );
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.transport, Transport::Fips);
        assert_eq!(
            c.this,
            Some(Device {
                key: key(0),
                addr: None,
                certificate: None
            })
        );
        assert_eq!(
            c.devices.iter().map(|d| d.key).collect::<Vec<_>>(),
            [key(1), key(2)]
        );
        assert_eq!((c.port, c.scan_interval, c.pull_interval), (9000, 3, 4));
        assert_eq!((c.trash_days, c.keep_days), (0, 7));
    }

    #[test]
    fn folder_and_mode() {
        let c = Config::parse("folder = /var/vault\nmode = send\n").unwrap();
        assert_eq!(c.folder, Some(std::path::PathBuf::from("/var/vault")));
        assert_eq!(c.mode, Mode::Send);
        let d = Config::parse("mode = sync").unwrap();
        assert_eq!((d.folder, d.mode), (None, Mode::Sync));
        assert_eq!(Config::default().mode, Mode::Sync);
        assert!(Config::parse("mode = receive").is_err());
    }

    #[test]
    fn ignored_paths() {
        let c = Config::parse("ignore = .obsidian\nignore = notes/tmp/\n").unwrap();
        for p in [
            ".obsidian",
            ".obsidian/app.json",
            ".obsidian/x/",
            "notes/tmp/a",
        ] {
            assert!(c.ignores(p), "{p}");
        }
        for p in [".obsidian2", "a/.obsidian", "notes/tmpx", "notes/a"] {
            assert!(!c.ignores(p), "{p}");
        }
        assert!(Config::parse("ignore = ../x").is_err());
    }

    #[test]
    fn plain_addresses() {
        let text = format!(
            "device = {} 127.0.0.1:7841\nself = {} [::1]:7840\ntransport = plain\n",
            key(1),
            key(0)
        );
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.transport, Transport::Plain);
        assert_eq!(c.this.unwrap().addr, Some("[::1]:7840".parse().unwrap()));
        assert_eq!(c.devices[0].addr, Some("127.0.0.1:7841".parse().unwrap()));
    }

    #[test]
    fn rejections() {
        let (k0, k1) = (key(0), key(1));
        let many: String = std::iter::once("transport = fips\n".to_string())
            .chain((1..W as u8).map(|i| format!("device = {}\n", key(i))))
            .collect();
        assert!(Config::parse(&many).is_ok(), "W - 1 devices");
        let too_many = format!("{many}device = {}\n", key(0));
        let bad = [
            "colour = blue".to_string(),
            "device".into(),
            "device =".into(),
            "device = npub1nope".into(),
            "scan_interval = soon".into(),
            "scan_interval = 0".into(),
            "trash_days = -1".into(),
            "port = 0".into(),
            "port = 70000".into(),
            "transport = carrier-pigeon".into(),
            "port = 1\nport = 2".into(),
            "= 5".into(),
            format!("self = {k0}\nself = {k1}"),
            format!("device = {k1}\ndevice = {k1}"),
            format!("self = {k0}\ndevice = {k0}"),
            format!("device = {k1} [::1]:7840"),
            format!("transport = plain\ndevice = {k1}"),
            format!("transport = plain\nself = {k0}"),
            format!("transport = plain\ndevice = {k1} nowhere"),
            format!("transport = plain\ndevice = {k1} [::1]:1 extra"),
            too_many,
        ];
        for text in bad {
            assert!(Config::parse(&text).is_err(), "{text:?}");
        }
    }
}
