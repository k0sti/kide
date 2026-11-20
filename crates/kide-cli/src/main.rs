//! The `kide` command-line program: the local folder commands, `gc`,
//! `serve`, `pull` and the daemon `run`.

mod args;
mod cmds;

use args::{Args, Fail, Out, usage};
use std::process::ExitCode;

const USAGE: &str = "usage: kide [-C <dir>] <command> ...

commands, in the folder given by -C (default: the current directory):
  init [dir]                  make a Kide folder
  renew-tls                   renew TLS credentials; exchange the new pin with peers
  move <from> <to>            move a file or directory (directory operands end in /)
  restore-tree <root> --replace restore a snapshot forward, replacing intervening edits
  scan                        record local changes and print the new HEAD
  ls [--live]                 list the manifest of HEAD
  log <path>                  the version history of a path in HEAD, newest first
  verify [--repair [device]]  re-hash every object and check HEAD's manifest; with
                              --repair, drop corrupt objects and fetch what is
                              missing from a device
  checkout <root|HEAD> <dir>  write a root's folder into an empty directory
  gc                          remove objects no root needs, old trash and old roots
  serve                       serve HEAD to the configured devices until interrupted
  pull [device]               pull from one configured device (npub1...), or from all
  confirm                     let the next scan record the deletions it refused
  restore                     write files the scan finds gone back from the store
  reset [device]              take a device's state: every path gets its version,
                              local changes go (files to the trash)
  join [device]               join a device's folder without history here: where
                              they differ its version wins, the local one is a copy
  run                         the daemon: serve, scan and pull on the configured
                              intervals until killed (a kill is safe at any moment)

QUIC with device keys and pinned TLS certificates is the default. Plain TCP is
for local testing only. FIPS is experimental until tested between two machines.
With transport fips, the host must drop packets addressed to its
own FIPS address that arrive on any interface other than the FIPS one, loopback
included, for example with an nftables rule. Kide cannot check this; serve and
run warn about it at start-up and print the rule with the address filled in.";

fn main() -> ExitCode {
    match Args::parse(std::env::args().skip(1)).and_then(|a| run(&a)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fail::Usage(m)) => {
            eprintln!("kide: {m}\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Fail::Run(e)) => {
            eprintln!("kide: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(a: &Args) -> Out<()> {
    match a.cmd.as_str() {
        "init" => cmds::init(a),
        "renew-tls" => cmds::renew_tls(a),
        "scan" => cmds::scan(a),
        "move" => cmds::move_path(a),
        "restore-tree" => cmds::restore_tree(a),
        "ls" => cmds::ls(a),
        "log" => cmds::log(a),
        "verify" => cmds::verify(a),
        "checkout" => cmds::checkout(a),
        "serve" => cmds::serve(a),
        "pull" => cmds::pull(a),
        "confirm" => cmds::confirm(a),
        "restore" => cmds::restore(a),
        "reset" => cmds::reset(a),
        "join" => cmds::join(a),
        "gc" => cmds::gc(a),
        "run" => cmds::run(a),
        other => usage(format!("unknown command {other:?}")),
    }
}
