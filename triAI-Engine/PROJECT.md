# triAI-Engine – sichtbares Projekt

Dieses Verzeichnis ist das vollständige, eigenständige Engine-Projekt für
CarloCODE. Es kann als Projektwurzel in einer Entwicklungs- oder Agenten-App
geöffnet werden.

## Projektumfang

- lokale `llama-server`-Worker und Supervisor
- OpenAI-kompatibles Runtime-Gateway
- deterministische Policy-, Pfad- und Ressourcenprüfungen
- primäre/fallback-fähige Modellverwaltung
- redigierte Evidence und optionaler Learning-Store
- VRAM/RAM/SSD-Transfermetriken und Trigger-Hysterese
- reversible Staging-/Storage-Planung
- Release-, systemd- und Rollback-Dokumentation

## Zentrale Einstiegspunkte

| Zweck | Pfad |
|---|---|
| Build/Tests | `README.md` |
| Vollständige Nutzung | `docs/USAGE.md` |
| Status und Coding-Plan | `docs/STATUS_AND_PLAN.md` |
| Evidence | `docs/evidence/` |
| Release/Rollback | `docs/RELEASE_CHECKLIST.md` |
| Beispielkonfiguration | `config/example.toml` |
| Engine-API | `src/api.rs`, `src/http.rs` |
| Supervisor | `src/engine.rs`, `src/supervisor.rs` |
| Analyse/Messung | `src/learning.rs`, `src/performance.rs` |
| Storage-Dry-Run | `src/storage.rs` |

## Sicherheitsgrenze

Die Engine ist bewusst zweigeteilt. Der Control-/Execution-Pfad entscheidet
über Auth, Capabilities, Budgets, Pfade, Modellzustände und Ausführung. Der
Analyse-/Learning-Pfad erhält nur begrenzte, redigierte Daten und kann keine
dieser Entscheidungen überschreiben.

## Nicht enthalten

GGUF-Modellbinärdaten, Laufzeitlogs, Evidence-Daten, Secrets, `target/`,
Partitionierungszustände und privilegierte Systemänderungen sind kein
Projektbestandteil des Git-Repositories. Lokale Modelle werden über
`models/manifest.json` beschrieben und bleiben gitignored.

## Schnellstart

```bash
cargo build --release
cargo test --lib
cargo clippy --all-targets --all-features -- -D warnings
```
