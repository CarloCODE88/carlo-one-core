# Architecture

The engine keeps inference local and single-slot. The supervisor owns the
worker lifecycle and atomic model slot; monitor triggers and the prompt hooks
stay deterministic and fail closed. Evidence is redacted JSONL. Chunk storage
parses GGUF metadata, packs independent zstd frames, verifies checksums, warms
eager tensors, and falls back read-only to the verified original GGUF.

Background analysis consumes only aggregate numeric windows. It emits bounded,
versioned policy proposals; gates require confidence, independent evidence,
disk savings, and no decode regression before promotion. It never reads or
stores prompts, code, completions, or secrets.

The HTTP boundary exposes the OpenAI-compatible API, authenticates external
binds, attributes requests to a configured server identity, and serializes
work through the single model slot.
