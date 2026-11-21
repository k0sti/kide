//! The installed `git` program, run on the link's bare repository with its
//! hooks disabled. The user's own configuration (credentials, SSH) applies.

use kide::{Error, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

/// A bare repository.
pub(crate) struct Git {
    repo: PathBuf,
}

impl Git {
    pub fn new(repo: &Path) -> Git {
        Git { repo: repo.into() }
    }

    /// Creates the bare repository.
    pub fn init(repo: &Path) -> Result<Git> {
        let out = bare()
            .arg("init")
            .arg("-q")
            .arg("--bare")
            .arg(repo)
            .output();
        check(out, "init")?;
        Ok(Git::new(repo))
    }

    fn cmd(&self) -> Command {
        let mut c = bare();
        c.arg("--git-dir").arg(&self.repo);
        // A directory that never exists: no hook runs.
        c.arg("-c").arg(format!(
            "core.hooksPath={}",
            self.repo.join("no-hooks").display()
        ));
        c
    }

    /// Runs a local command; its trimmed standard output.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = check(self.cmd().args(args).output(), args[0])?;
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// Runs a local command whose failure means "no": `merge-base`,
    /// `rev-parse --verify`.
    pub fn try_run(&self, args: &[&str]) -> Result<Option<String>> {
        let out = self.cmd().args(args).output().map_err(spawn)?;
        Ok(match out.status.success() {
            true => Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string()),
            false => None,
        })
    }

    /// Runs a command with standard input; its raw standard output.
    pub fn run_with(
        &self,
        args: &[&str],
        input: &mut dyn FnMut(&mut ChildStdin) -> Result<()>,
    ) -> Result<Vec<u8>> {
        let mut child = self
            .cmd()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(spawn)?;
        // `spawn` with piped stdin always gives one.
        let mut stdin = child.stdin.take().expect("piped stdin");
        let fed = input(&mut stdin);
        drop(stdin);
        let out = check(child.wait_with_output(), args[0])?;
        fed?;
        Ok(out.stdout)
    }

    /// Runs a command that talks to a remote: a failure to reach it is
    /// `Unavailable`, a rejected update `Refused`.
    pub fn remote(&self, args: &[&str]) -> Result<String> {
        let out = self.cmd().args(args).output().map_err(spawn)?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string());
        }
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = format!("git {}: {err}", args[0]);
        Err(
            match err.contains("[rejected]") || err.contains("non-fast-forward") {
                true => Error::Refused(msg),
                false => Error::Unavailable(msg),
            },
        )
    }

    /// Streams blobs out of the repository (`cat-file --batch`).
    pub fn blobs(&self) -> Result<Blobs> {
        let mut child = self
            .cmd()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(spawn)?;
        // Both are piped above.
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Blobs {
            child,
            stdin: Some(stdin),
            stdout,
        })
    }
}

/// `git` outside any repository, isolated from the caller's `GIT_DIR`.
fn bare() -> Command {
    let mut c = Command::new("git");
    for v in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
    ] {
        c.env_remove(v);
    }
    c
}

fn spawn(e: std::io::Error) -> Error {
    Error::Unavailable(format!("cannot run git: {e}"))
}

fn check(out: std::io::Result<Output>, what: &str) -> Result<Output> {
    let out = out.map_err(spawn)?;
    if out.status.success() {
        return Ok(out);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(Error::Invalid(format!("git {what}: {}", err.trim())))
}

/// A running `cat-file --batch`.
pub(crate) struct Blobs {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Blobs {
    /// Passes the blob `id` to `sink` in pieces; returns its size.
    pub fn read(&mut self, id: &str, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<u64> {
        let stdin = self.stdin.as_mut().expect("open until dropped");
        writeln!(stdin, "{id}")?;
        stdin.flush()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        let fields: Vec<&str> = header.split_whitespace().collect();
        let size = match fields.as_slice() {
            [_, "blob", size] => size.parse::<u64>().ok(),
            _ => None,
        };
        let Some(size) = size else {
            return Err(Error::Invalid(format!(
                "git cat-file {id}: {}",
                header.trim()
            )));
        };
        let mut left = size;
        let mut buf = vec![0; 64 * 1024];
        while left > 0 {
            let n = buf.len().min(left as usize);
            self.stdout.read_exact(&mut buf[..n])?;
            sink(&buf[..n])?;
            left -= n as u64;
        }
        let mut lf = [0];
        self.stdout.read_exact(&mut lf)?;
        Ok(size)
    }
}

impl Drop for Blobs {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}
