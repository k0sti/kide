#!/usr/bin/env bash
# Full check: heavy tests in release, clippy (also for the browser build), fmt,
# the reference's self-checks, the browser tests and kide-web's package.
set -euo pipefail
cd "$(dirname "$0")"

KIDE_FULL=1 cargo test --release -p kide -p kide-cli -p kide-git
cargo clippy -p kide -p kide-cli -p kide-git --all-targets --all-features -- -D warnings
cargo clippy -p kide-cli --all-targets --no-default-features -- -D warnings
for features in --no-default-features ""; do
    cargo clippy --lib --target wasm32-unknown-unknown $features -- -D warnings
done
cargo clippy -p kide-web --example browser_peer --features test-server -- -D warnings
cargo clippy -p kide-web --all-targets --target wasm32-unknown-unknown -- -D warnings
cargo fmt --check -p kide -p kide-cli -p kide-git -p kide-web
python3 -I reference/kide_ref.py --check
../kide-web/test.sh
../kide-web/build.sh

