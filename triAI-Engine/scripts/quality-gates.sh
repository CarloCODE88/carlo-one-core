#!/usr/bin/env bash
# Validate recorded benchmark evidence. The caller supplies measurements from
# three warm runs on the target machine; this script never fabricates a pass.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: scripts/quality-gates.sh <benchmark-results.json>" >&2
  exit 2
fi

cargo run --quiet --release --bin tri-quality-gates -- "$1"
