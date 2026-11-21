# Kide

Content-defined hash trees, synchronised between devices. `crates/kide/` is the library. All active work is in the public `k0sti/kide` repository. `main` is the public release line; development happens on a public development branch. The earlier private repository is an archived, read-only historical record, never an active push target.

## Sources of truth

- `crates/kide/SPEC.md` describes the format and protocol the code implements; § numbers refer to it. Spec text and code change together, in the same commit.
- `crates/kide/reference/kide_ref.py` is an independent Python implementation of the format only (§2, §3.1, §5.1 to §5.3); it generates the §8 test vectors.
- Design documents live in Kota at `shared/kota-development/Kide/`, accessed with Kota's tools. Do not add design documents, drafts or plans to this repository. Keep the implemented specification, user documentation and contribution/workflow instructions here.

## Architecture rules

- The format module (`crates/kide/src/format/`) is concrete; everything else sits behind the traits in `crates/kide/src/iface.rs`. Engines use only the traits; implementations depend only on the format, never on one another.
- Every trait has a conformance suite in `crates/kide/src/conformance/`, written before the first implementation.
- Traits are blocking and standard-library only; asynchronous code lives only inside an implementation.
- The library builds for `wasm32-unknown-unknown`, except the modules that use the file system (`cfg(any(unix, windows))`). Browser-only code lives in `crates/kide-web` (`kide-web`), which keeps wasm-bindgen out of `kide`. The `kide` program is `crates/kide-cli` (`kide-cli`). The Git link is `crates/kide-git` (`kide-git`), which runs the installed `git` program and depends only on `kide`.
- Errors follow §4.4: never merge or convert `NotFound`, `Unavailable` and `Corrupt`, and never treat a missing or unreachable object as a deletion.
- No backward compatibility: a change replaces the old code, with no migrations or fallbacks.
- To change the format: edit the rule in `crates/kide/SPEC.md`, increment the version, update the reference, regenerate §8 with it, then the Rust code. Never edit a test vector to make a test pass.

## Code style

- Idiomatic, concise Rust; small files and functions; no abstraction without a second use.
- Ask before adding a dependency. Hex, the test PRNG, temporary directories and argument parsing are written by hand.
- No `unwrap` outside tests, except for proven invariants with a comment.
- Logging through the `log!` macro, enabled by `KIDE_LOG=1`.

## Commands and tests

```
nix develop
cd crates/kide
cargo test <filter>    # while working: only the tests you touch
cargo test -p kide -p kide-cli -p kide-git   # before a commit: the light suite
./ci.sh                # only when asked: release, heavy tests, clippy, fmt, reference
cargo clippy --lib --target wasm32-unknown-unknown   # the library for the browser
../kide-web/test.sh            # kide-web in headless Chromium against a native server
../kide-web/build.sh           # kide-web's package for a page (pkg/)
```

- The root `Cargo.toml` is a workspace (`kide`, `kide-cli`, `kide-git`, `kide-web` under `crates/`) with one `Cargo.lock` and `target/`.
- Write the failing test first for any non-trivial change. Tests stay small and fast; anything over a second goes behind `KIDE_FULL=1`.
- Real data never enters the repository; tests read it from `KIDE_CORPUS` and skip when it is unset.
- Use Linux CI only; do not spend hosted minutes on macOS or Windows jobs.

## Workflow

- One task, one commit, conventional message; list small decisions you made under `Decisions:`.
- Before every commit: `cargo test -p kide -p kide-cli -p kide-git` passes, clippy is clean (run it inside `nix develop`), `cargo fmt` is applied.
- Work on `dev` or a task-specific development branch. Push checked commits to that branch for backup and review; do not automatically advance the local `main` checkout or push to remote `main`.
- Merges to `main` are squash-merges only, made deliberately after Kosti's explicit OK for the reviewed changes. Before an approved squash-merge, fetch the latest target, inspect the final diff and relevant checks, and resolve integration issues on the development branch. Audit the resulting squash commit again with `python3 -I scripts/public_audit.py --check HEAD` before pushing. Passing checks alone is not approval. Earlier standing instructions to push every commit to `main` are superseded.
- The squash-merge review is a cleanliness gate as well as a security gate. Review the complete diff against the latest `main` and remove stray notes, scratch/temp files, dead or experimental code, debug leftovers, duplicated explanations, and outdated or incorrect comments and documentation. Keep only material that is needed and correct; wrong leftovers mislead future contributors and agents. Rerun affected checks after cleanup and review the final squash diff before publication.
- After the approved squash commit is verified on remote `main`, delete the merged development branch locally and remotely, then start a fresh development branch from the fetched `main`. First verify that all intended changes were included and preserve any concurrent unmerged work. Prefer deletion and recreation over resetting a published branch: do not force-push or retain merged development branches/tags just to keep the messy history alive.
- Every development push is public too. Install the tracked `.githooks/pre-push` in the common Git hooks directory as described in CONTRIBUTING.md; configure `kide.auditPolicy` with the private policy stored outside tracked files. The bounded audit scans the resulting tree, outgoing commit trees/metadata and annotated tags. Missing policy, findings or exhausted limits block the push. Never bypass the hook or treat an incomplete scan as a pass.
- Keep unnecessary files out: design drafts, local notes, credentials, editor/agent state and generated build outputs do not belong in publication trees. Private audit patterns belong in local Git metadata, not in the public source.
- Keep the private development archive and public repository histories distinct; never force-push or transplant private history into the public line.
