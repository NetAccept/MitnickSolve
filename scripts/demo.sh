#!/usr/bin/env bash
# One-command interactive demo for Linux/macOS.
set -euo pipefail
cd "$(dirname "$0")/.."
repo=$PWD

if [[ ! -x target/debug/envd && ! -x target/debug/envd.exe ]]; then
  (cd tools/envd && go build -o ../../target/debug/envd ./cmd/envd)
fi
cargo build --workspace --bins

rm -rf .msk/demo
python3 bench/mock_llm.py 18080 >/tmp/msk-demo-mock.log 2>&1 &
mock=$!
cleanup() { kill "$mock" 2>/dev/null || true; }
trap cleanup EXIT

sleep 0.6
export MSK_LLM_BASE_URL=http://127.0.0.1:18080/v1
export MSK_LLM_MODEL=mock
export MSK_LLM_API_KEY=demo-key
export MSK_PROVIDER=openai

echo
echo "MitnickSolve demo"
echo "  Type: exec      and press Enter; approve the exec tool with y."
echo "  Then try: file, secret, flag, board, spawn, or /help."
echo "  q quits the TUI."
echo
target/debug/msk-tui --root .msk/demo --provider openai
