# Kide

Content-defined hash trees, synchronised between devices. `crates/kide/` is the library. Public snapshots start from the current tree; earlier development history is retained separately.

## Sources of truth

- `crates/kide/SPEC.md` describes the format and protocol the code implements; § numbers refer to it. Spec text and code change together, in the same commit.
- `crates/kide/reference/kide_ref.py` is an independent Python implementation of the format only (§2, §3.1, §5.1 to §5.3); it generates the §8 test vectors.

## Architecture rules

- The format module (`crates/kide/src/format/`) is concrete; everything else sits behind the traits in `crates/kide/src/iface.rs`. Engines use only the traits; implementations depend only on the format, never on one another.
- Every trait has a conformance suite in `crates/kide/src/conformance/`, written before the first implementation.
- Traits are blocking and standard-library only; asynchronous code lives only inside an implementation.
- The library builds for `wasm32-unknown-unknown`, except the modules that use the file system (`cfg(any(unix, windows))`). Browser-only code lives in `crates/kide-web` (`kide-web`), which keeps wasm-bindgen out of `kide`. The `kide` program is `crates/kide-cli` (`kide-cli`).
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
cargo test -p kide -p kide-cli   # before a commit: the light suite
./ci.sh                # only when asked: release, heavy tests, clippy, fmt, reference
cargo clippy --lib --target wasm32-unknown-unknown   # the library for the browser
../kide-web/test.sh            # kide-web in headless Chromium against a native server
../kide-web/build.sh           # kide-web's package for a page (pkg/)
```

- The root `Cargo.toml` is a workspace (`kide`, `kide-cli`, `kide-web` under `crates/`) with one `Cargo.lock` and `target/`.
- Write the failing test first for any non-trivial change. Tests stay small and fast; anything over a second goes behind `KIDE_FULL=1`.
- Real data never enters the repository; tests read it from `KIDE_CORPUS` and skip when it is unset.

## Workflow

- One task, one commit, conventional message; list small decisions you made under `Decisions:`.
- Before every commit: `cargo test -p kide -p kide-cli` passes, clippy is clean (run it inside `nix develop`), `cargo fmt` is applied.
- After every commit that passes these checks, bring it into the local `main` (fast-forward the main checkout to it) and push `main` to `origin` right away, so every session and the backup on GitHub have it. Never force-push.
