//! Hand-written argument parsing: a command, positional arguments,
//! `--name` flags and the global `-C <dir>`.

use kide::Error;
use kide::hash::Hash;
use std::path::PathBuf;

pub enum Fail {
    Usage(String),
    Run(Error),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Run(e)
    }
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Fail::Run(e.into())
    }
}

pub type Out<T> = std::result::Result<T, Fail>;

pub fn usage<T>(msg: impl Into<String>) -> Out<T> {
    Err(Fail::Usage(msg.into()))
}

pub fn failed<T>(msg: impl Into<String>) -> Out<T> {
    Err(Fail::Run(Error::Invalid(msg.into())))
}

pub struct Args {
    pub cmd: String,
    pub pos: Vec<String>,
    flags: Vec<String>,
    dir: Option<PathBuf>,
}

impl Args {
    pub fn parse(mut it: impl Iterator<Item = String>) -> Out<Args> {
        let (mut cmd, mut pos, mut flags, mut dir) = (None, Vec::new(), Vec::new(), None);
        while let Some(a) = it.next() {
            if a == "-C" {
                let Some(d) = it.next() else {
                    return usage("-C needs a directory");
                };
                dir = Some(PathBuf::from(d));
            } else if let Some(name) = a.strip_prefix("--") {
                flags.push(name.to_string());
            } else if cmd.is_none() {
                cmd = Some(a);
            } else {
                pos.push(a);
            }
        }
        let Some(cmd) = cmd else {
            return usage("no command");
        };
        Ok(Args {
            cmd,
            pos,
            flags,
            dir,
        })
    }

    /// Checks the number of positional arguments and the flag names.
    pub fn expect(&self, npos: std::ops::RangeInclusive<usize>, allowed: &[&str]) -> Out<()> {
        if !npos.contains(&self.pos.len()) {
            return usage(format!("wrong number of arguments for {}", self.cmd));
        }
        match self.flags.iter().find(|f| !allowed.contains(&f.as_str())) {
            Some(f) => usage(format!("{} has no flag --{f}", self.cmd)),
            None => Ok(()),
        }
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    /// The folder: `-C <dir>`, or the current directory.
    pub fn dir(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(|| PathBuf::from("."))
    }
}

pub fn hash_arg(s: &str) -> Out<Hash> {
    Hash::from_hex(s).or_else(|_| usage(format!("not an object id: {s:?}")))
}
