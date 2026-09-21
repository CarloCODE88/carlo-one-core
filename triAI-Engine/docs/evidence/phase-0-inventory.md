# Evidence – Phase 0: Ist-Aufnahme

Datum: 2026-09-14

## Reproduzierbar ermittelte Basis

- Repository: `/home/carlos/PROJEKTE/triAI-Engine`
- Branch: `triAI-engine`
- Buildsystem: Cargo/Rust; Bibliothek plus dünnes `src/main.rs`-Binary
- Arbeitsbaum: absichtlich uncommitted; vorhandene Änderungen wurden nicht
  zurückgesetzt oder überschrieben.
- Der ursprüngliche Runner-Worktree liegt separat unter
  `/home/carlos/Documents/Codex/2026-09-05/er/tri-ai-runner` und wurde nicht
  bearbeitet.

## Tatsächliche Engine-Grenzen

Die Engine ist zweigeteilt:

1. Der harte Control-/Execution-Pfad umfasst HTTP-Vertrag, Authentisierung,
   Policy-/Pfadprüfung, Modell-Supervisor, Tool-Ausführung und deterministische
   Ressourcenentscheidungen. Dieser Pfad entscheidet synchron und darf nicht
   von einem Analysemodell überschrieben werden.
2. Der optionale Analyse-/Learning-Pfad umfasst redigierte Events,
   Trajektorien, Transfer-/Ressourcenmessung und spätere Routingauswertung.
   Er ist best-effort, standardmäßig deaktiviert und darf weder Inferenz noch
   Sicherheitsentscheidungen blockieren.

## Vorhandene Module

`api`, `assistant`, `attachments`, `coding_tools`, `config`, `download`,
`engine`, `gguf_registry`, `http`, `learning`, `mcp`, `model_catalog`,
`model_registry`, `model_sources`, `observability`, `openai`, `performance`,
`persistence`, `planner`, `resources`, `staging`, `supervisor` und
`tool_registry` sind als Library-Module vorhanden und in `src/lib.rs`
exportiert.

## Konfiguration, Modelle und Laufzeit

- Beispielkonfiguration: `config/example.toml`
- Konfigurationssuche: `TRI_AI_CONFIG`; bestehende `TRI_AI_*`-Overrides
- Modelle: zwei lokale GGUF-Dateien, durch `models/manifest.json` mit
  SHA-256-Digests beschrieben; Modellbinärdaten bleiben gitignored.
- Evidence-/Eventdaten und Laufzeitverzeichnisse werden nicht als Projektinhalt
  committed.
- Bei der Aufnahme wurden keine triAI-/llama-Prozesse und keine belegten
  Standardports 8900/8901 vorgefunden.
- Die Startzeit ist bei großen lokalen GGUF-Dateien aktuell vom vollständigen
  Hashing des Modellkatalogs abhängig; dadurch können Health-Probes vor dem
  Accept-Loop verzögert werden. Das ist ein offener Runtime-Optimierungspunkt.

Die technische JSONL-Evidence verwendet Schema-Version 1, monotone
Prozess-Event-IDs und rekursive Redaction. Rohprompts und Antworten werden im
aktuellen Empfangs-/Lifecycle-Event nicht serialisiert.

## Baseline-Gate

`cargo fmt --all -- --check`,
`cargo clippy --all-targets --all-features -- -D warnings` und der vollständige
Library-Testlauf sind erfolgreich. Der privilegierte Volltest erreichte
220/220 Tests. Ein unprivilegierter Sandbox-Lauf bleibt für lokale
Test-Sockets eingeschränkt; dieser Umgebungsbefund ist getrennt dokumentiert.

## Gap-Liste

- vollständige Request-Correlation bis in Downstream-Evidence fehlt noch
- Cancellation eines laufenden Inferenzstreams fehlt noch
- append-only Evidence ist als datensparsamer Learning-Store vorhanden, aber
  noch nicht vollständig in den Request-Lebenszyklus integriert
- Registry-Checksum-/Warmup-/Fallback-Recovery braucht noch die HTTP-
  Verdrahtung und Integrationsszenarien; der Engine-Zustandsautomat bildet
  `degraded`, `draining` und `rollback` inzwischen explizit ab.
- Ressourcen-Trigger und Storage-Chunking sind zunächst Mess-/Planungsbausteine;
  keine privilegierte Kernel-, Partitionierungs- oder Kompressionsoperation ist
  aktiviert
