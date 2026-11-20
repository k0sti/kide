# Contributing to Kide

Kide is pre-release and changes fast; open an issue before a large change so it can be shaped with the format and the plan in mind.

## Build and test

```
nix develop          # the pinned toolchain (Rust with wasm32, wasm-bindgen-cli, Chromium for browser tests)
cd crates/kide
cargo test <filter>  # while working: the tests you touch
cargo test -p kide -p kide-cli   # before a commit: the light suite, a few seconds
./ci.sh              # everything CI runs
```

Without Nix, a stable Rust ≥ 1.89 builds and tests the `kide` crate; the browser crates need the `wasm32-unknown-unknown` target and `wasm-bindgen-cli` at the exact version `crates/kide-web/Cargo.toml` pins.

Tests stay small and fast: anything over a second goes behind `KIDE_FULL=1`. Tests that read real data take it from `KIDE_CORPUS` and skip when it is unset; real data never enters the repository.

## Rules the code follows

- `crates/kide/SPEC.md` describes the format and the protocol the code implements, and § numbers in the code refer to it. Spec text and code change in the same commit.
- To change the format: change the rule in `SPEC.md`, increment the version, update `crates/kide/reference/kide_ref.py`, regenerate the §8 test vectors with it, then change the Rust code. Never edit a test vector to make a test pass.
- The format module is concrete; everything else sits behind the traits in `crates/kide/src/iface.rs`. Engines use only the traits; implementations depend only on the format, never on one another. Every trait has a conformance suite in `crates/kide/src/conformance/`.
- Traits are blocking and standard-library only; asynchronous code lives inside an implementation.
- The library builds for `wasm32-unknown-unknown`, except the modules that touch the file system. Browser-only code lives in `crates/kide-web`.
- Errors follow §4.4: never merge or convert `NotFound`, `Unavailable` and `Corrupt`, and never treat a missing or unreachable object as a deletion.
- No backward compatibility before 1.0: a change replaces the old code, with no migrations or fallbacks.
- Idiomatic, concise Rust; small files and functions; no abstraction without a second use; no `unwrap` outside tests except for proven invariants, with a comment. Few dependencies: propose a new one in an issue first.
- Write the failing test first for any non-trivial change.

## Commits

All active work happens in the public `k0sti/kide` repository; the private
historical archive is read-only and must not be an active push remote. Develop on
public `dev` or a task branch and submit changes for review. `main` is the public
release line: squash-merges are deliberate and require Kosti's explicit OK for
the reviewed changes. Passing checks does not authorize a merge. Push work to its
development branch rather than directly to `main`; never force-push or attach
private development history to a public branch.

Design documents and plans live in Kota at `shared/kota-development/Kide/`, not
in this repository. The implemented specification and user documentation remain
with the code.

One change per commit, with a conventional message (`feat: …`, `fix: …`, `docs: …`). Before committing: `cargo test -p kide -p kide-cli` passes, `cargo clippy -p kide -p kide-cli --all-targets --all-features -- -D warnings` is clean, `cargo fmt` is applied. CI runs the same checks on every pull request.

## Audit before publishing

Development branches are public. Every push must pass the bounded audit in
`scripts/public_audit.py`. It checks the full resulting tree, all outgoing commit
trees and metadata, and annotated tag metadata, including files deleted in a
later outgoing commit. It rejects unrelated history, shallow repositories,
credentials and unnecessary files, and applies private-data patterns from local
policy. It never prints matching content. This is a bounded pattern check, not
a proof that arbitrary secrets cannot be present; review the diff as well.

Configure `kide.auditPolicy` to the absolute path of a private JSON policy outside
tracked files. Its `public_base` is the full ID of an already-audited public
commit; all published work must descend from it. Its nonempty `patterns` object
maps neutral check names to regular expressions for private project names,
abbreviations, local paths and hosts. Keep those patterns private, including
when writing tests. A missing policy blocks publication. Only advance the audited
base after reviewing a public release; never use a private-history commit.

Git does not install hooks on clone. In each checkout, configure the local policy
and install the tracked hook in the common Git directory so all worktrees use it.
Preserve any existing hooks when installing:

```sh
git config --local kide.auditPolicy /absolute/path/to/private-policy.json
hook_dir="$(git rev-parse --path-format=absolute --git-common-dir)/hooks"
install -m 755 .githooks/pre-push "$hook_dir/pre-push"
git config --local core.hooksPath "$hook_dir"
```

The audit requires Python 3 and permits only the public Kide push destination.
Its bounds are 20 seconds, 300 commits, 3,000 distinct file entries, 4 MiB per
object and 64 MiB of unique blob bytes. A finding, unsupported entry or exhausted
bound blocks the push; do not bypass the hook. Review and adjust the audit
deliberately if legitimate repository growth exceeds these bounds.

After Kosti approves a squash-merge, inspect the final diff, run the applicable
checks, create the squash commit on the public main history, and audit it again:

```sh
python3 -I scripts/public_audit.py --check HEAD
```

The merge review also removes unnecessary or incorrect material: scratch files,
dead/experimental code, debug leftovers, stale comments/docs and duplicated
explanations. After the squash commit is verified on remote main, delete the
merged local/remote development branch and start fresh from fetched main,
preserving any concurrent unmerged work. Do not force-push a branch reset.

The pre-push hook also runs on that push. Run the audit's regression tests with
`python3 -I scripts/test_public_audit.py` when changing the publication checks.

## Licensing and sign-off

Kide is licensed under either of the Apache License, Version 2.0 or the MIT license, at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in Kide by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

Every commit carries a `Signed-off-by` line, which certifies the [Developer Certificate of Origin](https://developercertificate.org): that you wrote the change, or otherwise have the right to submit it under these licences. `git commit -s` adds it, with the name and email of your git configuration:

```
Signed-off-by: Your Name <you@example.org>
```

Security issues go to the address in [SECURITY.md](SECURITY.md), not to a public issue.

## Using Kide from another repository

The workspace has three crates under `crates/`: `kide`, `kide-cli` and `kide-web`.
Each carries both license files; keep them when vendoring. The public repository
starts with the current source tree; earlier development history is retained
separately.

For a pinned library checkout, vendor `crates/kide/` (including its `Cargo.toml`,
`src/` and licenses). Older revisions used `v2/`; update that archive path and
strip two path components when adopting the current layout. Existing pins still
refer to their original layout. Do not change a pin without checking its callers.
