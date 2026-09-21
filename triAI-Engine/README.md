# triAI-Engine

Eigenständige, einbettbare Local-LLM-Engine aus dem tri-ai-runner. Sie
verwaltet lokale `llama-server`-Worker, Ressourcenplanung, Modellkatalog,
OpenAI-kompatible Requests, Staging und technische Telemetrie.

GUI, Design-Editor, Notebook, Frontend und Modellartefakte sind absichtlich
nicht Bestandteil dieses Worktrees.

Die vollständige Betriebs- und Integrationsdokumentation steht in
[`docs/USAGE.md`](docs/USAGE.md).

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Der Worktree liegt auf dem Branch `triAI-engine`. Der ursprüngliche Runner
bleibt unabhängig.
