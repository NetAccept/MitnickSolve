#!/bin/sh
set -eu
cd "$(dirname "$0")/mitnick_tools"
moon build --target wasm --release
cp _build/wasm/release/build/transform/transform.wasm ../../wasm/transform.wasm
echo "wrote tools/wasm/transform.wasm"
