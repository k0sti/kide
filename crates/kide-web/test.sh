#!/usr/bin/env bash
# Browser tests: starts the native test server (crates/kide-web/tests/support/browser_peer), then
# runs this crate's tests in headless Chromium against it. Needs
# wasm-bindgen-test-runner, chromedriver and chromium, as in the flake.
set -euo pipefail
cd "$(dirname "$0")"

out=$(mktemp)
cargo build -q -p kide-web --example browser_peer --features test-server
../../target/debug/examples/browser_peer >"$out" &
server=$!
trap 'kill $server; rm -f "$out"' EXIT
for _ in $(seq 100); do
    [ -s "$out" ] && break
    sleep 0.1
done

# The flake's Chromium, by path: chromedriver would otherwise take a Chrome
# it finds installed (as on CI runners), whose version it may not support.
binary=""
if chromium=$(command -v chromium); then
    binary=", \"binary\": \"$chromium\""
fi
printf '{"goog:chromeOptions": {"args": ["--no-sandbox"]%s}}\n' "$binary" >webdriver.json

KIDE_PEER=$(head -n 1 "$out") \
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
    cargo test --target wasm32-unknown-unknown "$@"
