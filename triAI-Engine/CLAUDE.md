# Claude session handoff: triAI-Engine

## Scope and invariants

- Worktree: `/home/carlos/PROJEKTE/triAI-Engine`
- Branch: `triAI-engine`; do not switch branches.
- Backend/API only. Do not add GUI or frontend code.
- Target: RTX 2080 Ti 11 GB, six CPU cores, local `llama.cpp` server.
- Preserve the dirty worktree. Existing modified, deleted and untracked files are intentional user work; never reset or revert them wholesale.
- Never inspect or modify `/home/carlos/PROJEKTE/Der WEGWEISER`.
- No secrets, raw prompts or tokens in code, logs or evidence.
- One active GPU inference worker. Worker host stays `127.0.0.1`.

## Verified current state

- Package builds as `tri-ai-engine`.
- Latest focused verification completed before this handoff:
  - `cargo test --lib`: 226 passed
  - `cargo clippy --all-targets --all-features -- -D warnings`: passed
  - `cargo build --release`: passed
- Runtime aliases were added in `src/http.rs`:
  - `/api/status`, `/api/resources`, `/api/models`, `/api/engine`
- Runtime planning in `src/planner.rs` now accounts for parallel slots, KV-cache dtype and compute reserve, and rejects sentinel/full-offload layer counts as real metadata.
- `src/http.rs` resolves real GGUF layer counts before worker configuration.
- `src/supervisor.rs` starts llama.cpp with one slot, flash attention, F16 KV, six threads and 512 batch/ubatch.
- Current local models are described in `models/manifest.json`: INGRIED primary and Dolphin3 fallback. GGUF files are intentionally ignored by Git.

## Known technical gap

Auto-planning currently derives INGRIED's native context as 40960, while the actual worker resolver starts it with configured context 4096. Unify plan and start configuration before relying on plan estimates as exact runtime evidence. Add a regression test first.

## Working-tree warning

The repository contains substantial pre-existing edits and removals, including the removal of vendored binaries and old Ollama inventory files. Inspect `git status --short` before every change. Do not commit unrelated files and do not perform destructive cleanup.

## Runtime paths

- llama-server: `/home/carlos/PROJEKTE/franz-studio-runner/llama.cpp/build/bin/llama-server`
- Required library path: `/home/carlos/PROJEKTE/franz-studio-runner/llama.cpp/build/bin`
- Engine API: `127.0.0.1:8765`
- Worker API: `127.0.0.1:8766`
- SearXNG wrapper: `http://127.0.0.1:8889/api/search`

## Interrupted research checkpoint

An iterative INGRIED/SearXNG investigation of removed or endangered coding models was paused for this handoff. Generic and direct `t.me`/Discord queries produced no reliable public hits. Leads requiring provenance and license verification are FastContext-1.0-4B-SFT and WizardLM-2-7B; NewHope/SLAM-group should not be installed because provenance is problematic. Do not download a community mirror until origin, license, digest and GGUF metadata are verified.

## Recommended next sequence

1. Run `cargo fmt --all -- --check`, `cargo test --lib`, and strict Clippy.
2. Fix the 40960-vs-4096 planning/runtime context mismatch using TDD.
3. Re-run a clean INGRIED boot and two warm benchmark requests; treat the first post-idle request separately because of GPU P8-to-P2 wake-up latency.
4. Resume model research only with traceable sources; install Qwen coder candidates only after exact quantization and license checks.
5. Continue the phase-by-phase comparison in `docs/STATUS_AND_PLAN.md` and record evidence under `docs/evidence/`.

## Commands

```bash
cargo fmt --all -- --check
cargo test --lib
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

Read `PROJECT.md`, `docs/USAGE.md`, and `docs/STATUS_AND_PLAN.md` before implementation.
