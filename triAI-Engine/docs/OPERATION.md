# Operations

Run locally with `TRI_AI_CONFIG=config/example.toml cargo run --release --
--no-network`. `--no-network` makes the intended startup mode explicit: no
model download or external service is needed to bind the API.

Mount GGUF models and a trusted `llama-server` separately; do not add models,
tokens, or JSONL evidence to a container image. Keep the API loopback-bound
unless an authentication token is configured.

Before release, collect three warm runs on the target hardware into the schema
used by `tri-quality-gates`, then run:

```bash
scripts/engine-ready-check.sh real-benchmark-results.json
```

The check rejects variance above 5%, startup over 500 ms, inference over
300 ms, warm/cold chunk regressions, warmup over five seconds, and compression
below 15%. It is intentionally not a substitute for a loaded-model benchmark.
