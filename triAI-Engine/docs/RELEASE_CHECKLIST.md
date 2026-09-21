# triAI-Engine Release- und Rollback-Checkliste

## Vor dem Release

- [ ] Clean-Checkout oder bewusst dokumentierter Worktree-Stand
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --lib` ohne Sandbox-Socketblocker
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] Contract-, Security-, Recovery- und Performance-Suite
- [ ] Modellmanifest und SHA-256-Digests geprüft
- [ ] Keine GGUF-Dateien, Logs, Evidence, Secrets oder `target/` in Git
- [ ] Baseline gegen Kaltstart, Warmstart, TTFT, Decode, P95, RAM/VRAM,
      Fehlerquote und 30-Minuten-Stabilität dokumentiert
- [ ] Komprimierte Artefakte nur bei bestandenem Qualitäts-/Recovery-Gate

## Installation ohne Root

```bash
cargo build --release
mkdir -p ~/.config/systemd/user
cp deploy/tri-ai-engine.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now tri-ai-engine.service
```

Die Unit fordert keine Kernel-, Geräte-, Mount- oder Partitionsrechte. Für
LAN-Betrieb muss ein Token über eine nicht versionierte Konfiguration oder
`TRI_AI_AUTH_TOKEN` gesetzt sein.

## Rollback

1. `systemctl --user stop tri-ai-engine.service`
2. Release-Verzeichnis auf die vorherige verifizierte Version zurückstellen.
3. Das unveränderte Original-GGUF und die vorherige Konfiguration aktivieren.
4. `cargo build --release` bzw. das vorherige reproduzierte Binary verwenden.
5. `systemctl --user start tri-ai-engine.service`
6. `/health`, `/metrics` und einen autorisierten Contract-Smoke-Test prüfen.

Storage-/Kompressionspläne werden nicht automatisch zurückgerollt, weil sie
bis zum separaten Promotion-Gate nur Dry-Run-Metadaten sind.
