#!/usr/bin/env bash
# Packages kide-web for web pages: pkg/kide_web.js, an ES module to import,
# and pkg/kide_web_bg.wasm, which it loads. Needs wasm-bindgen, as in the flake.
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release -p kide-web --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir pkg \
    ../../target/wasm32-unknown-unknown/release/kide_web.wasm
