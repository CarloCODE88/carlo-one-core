#!/usr/bin/env bash
# Final local readiness check. It needs real three-run model measurements;
# passing synthetic values is intentionally not supported by this script.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: scripts/engine-ready-check.sh <real-benchmark-results.json>" >&2
  exit 2
fi

cargo fmt --all -- --check
cargo test --locked
cargo bench --no-run --locked
cargo build --release --locked

binary=target/release/tri-ai-engine
size_bytes=$(stat -c '%s' "$binary")
if (( size_bytes > 50 * 1024 * 1024 )); then
  echo "FAIL: release binary exceeds 50 MiB" >&2
  exit 1
fi

scripts/quality-gates.sh "$1"
echo "ENGINE READY FOR INTEGRATION (subject to the supplied real benchmark evidence)"
