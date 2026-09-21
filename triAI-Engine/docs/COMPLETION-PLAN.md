# triAI-Engine — Abschlussplan (Phasen F–K)

## Production-Readiness vor jeder Integration

Stand: 2026-09-14 · Kanon v3 · user_id: frst-9F3K

## Ehrliches Status-Inventory

| Phase | Commits | Scope | Status |
|---|---|---|---|
| B: Evidence | `4fc1c52`, `89df877` | JSONL, Rotation, Redaction | abgeschlossen |
| C: Monitor | `9316e15` | Snapshot, Trigger, Hysterese | abgeschlossen |
| D1–D5: Storage | `1660ec9` → `af89bbf` | GGUF, Packer, Loader, Warmup, Rollback | abgeschlossen |
| E: Supervisor | `5297a28` | INGRIED/Dolphin3, Failover, Advice | abgeschlossen |

Referenzstand: 267 Unit-Tests sowie vier Integrationstests grün.

Offen sind F (Background-Analyse und Policy-Promotion), G (Benchmarks und
Quality-Gates), H (HTTP/Streaming-API), I (Prompt-Optimizer und Kanon-Hooks),
J (Multi-Model-Concurrency-Guard) und K (Release-Readiness). Geschätzter
Gesamtaufwand: 38–51 Stunden über drei Wochen.

## Definition of Done

Die Engine ist erst production-ready für Leiter 1, wenn alle folgenden Punkte
nachweislich erfüllt sind:

- Phasen B–K einschließlich aller Tests grün.
- Stabile HTTP-API mit Streaming.
- Reproduzierbare Benchmarks: drei Läufe mit weniger als 5 % Varianz.
- Quality-Gates in CI: Startup unter 500 ms und P95 unter 300 ms.
- Single-Slot-Policy hart durchgesetzt (Kanon G7).
- Evidence-Store frei von Secrets (Kanon G3) und Anti-Gierschlund aktiv.
- Failover INGRIED → Dolphin3 dokumentiert.
- Air-gapped-Start ohne externe Calls möglich.
- Release-Binary gebaut.

Erst nach diesem Check darf eine Integration in ein Zielprojekt begonnen werden.

## Phase J — Multi-Model Concurrency Guard (zuerst)

Zweck: die Single-Slot-Policy hart durchsetzen (Kanon G7).

Deliverables:

- `src/supervisor/slot.rs`: `AtomicSlot` mit Mutex.
- `src/supervisor/policy.rs`: Parallelbetrieb verboten.
- `src/supervisor/shutdown.rs`: Graceful Shutdown bei Slot-Konflikt.

Mindestens sechs Tests decken zweiten Start, Start nach Stop, Force-Stop während
Inferenz, Serialisierung paralleler Requests, atomaren INGRIED→Dolphin3-Wechsel
und konsistenten Crash-Recovery-State ab. Kanon: G7, G4. Aufwand: 3–4 Stunden.

J ist Fundament für HTTP-API, Benchmarks und Hintergrundanalyse.

## Phase I — Prompt-Optimizer und Kanon-Hooks

Zweck: Anti-Gierschlund direkt auf der Inferenz-Ebene.

Deliverables:

- `src/prompt/compress.rs`: Filler-Wörter, URL-Strip, Dedup.
- `src/prompt/estimate.rs`: Token-Schätzung mit ASCII- und CJK-Pfad.
- `src/prompt/caps.rs`: `TASK_CAP=1200`, Run- und Call-Caps.
- `src/prompt/hooks.rs`: deterministische Pre-/Post-Inference-Hooks.

Mindestens zehn Tests prüfen DE/EN-Filler, URL-Strip, Code-Platzhalter,
Zeilen-Dedup, harte Caps, ASCII- und CJK-Schätzung, Pre-Inference-Caps,
deterministische Hook-Kette und secret-freien Output. Kanon: G3, G10.
Aufwand: 5–7 Stunden.

## Phase H — HTTP/Streaming-API

Zweck: eine OpenAI-kompatible API für externe Clients.

Deliverables:

- `src/http/server.rs`: Hyper/Axum-Server auf Port 8765.
- `src/http/chat.rs`: `/v1/chat/completions` mit SSE-Streaming.
- `src/http/models.rs`: `/v1/models` und `/health`.
- `src/http/plan.rs`: `/api/engine/plan` und `/api/engine/start`.
- `src/http/auth.rs`: Session-Token zu serverseitiger `user_id`.
- `src/http/budget.rs`: Token-Limits pro Request.

Mindestens zwölf Tests: Health ohne/mit Modell, Chat non-streaming und SSE,
Manifest-Modellliste, ungültige Session (401), Cap-Verletzung (429), blockierte
Queue (503), Plan-/Start-Weitergabe, fehlende `user_id` sowie serialisierte
Concurrent Requests. Kanon: G9, G3, G4. Aufwand: 10–14 Stunden.

Abhängigkeit: Phase J.

## Phase F — Background-Analyse und Policy-Promotion

Zweck: aus aggregierten Evidence-Fenstern ausschließlich versionierte
Policy-Vorschläge erzeugen.

Deliverables:

- `src/analysis/job.rs`: asynchroner Analyse-Scheduler.
- `src/analysis/policy.rs`: Vorschlags-Generator.
- `src/analysis/gates.rs`: Promotions-Gates.
- `src/analysis/store.rs`: Lern-Store ausschließlich für Metriken.

Mindestens acht Tests: nur aggregierter Input, monotone Policy-Versionierung,
Regression blockiert Promotion, idempotente Jobs, keine Secrets, mindestens
15 % Disk-Ersparnis und höchstens 10 % Decode-Loss, Confidence mindestens 0,8,
kein Rohprompt-Zugriff. Kanon: G3, G9. Aufwand: 6–8 Stunden.

## Phase G — Benchmarks und Quality-Gates

Zweck: reproduzierbare Performance-Baselines und CI-Gates.

Deliverables:

- `benches/startup.rs`: Startup-Latenz, Ziel unter 500 ms.
- `benches/inference.rs`: P95 für 256 Token, Ziel unter 300 ms.
- `benches/chunk_load.rs`: warm/cold Chunk-Ladezeit.
- `benches/warmup.rs`: eager-Warmup.
- `scripts/quality-gates.sh`: Pass/Fail-Integration.

Harte Gates:

| Kennzahl | Gate |
|---|---:|
| `startup_p95_ms` | ≤ 500 |
| `inference_p95_ms` | ≤ 300 |
| `chunk_load_ms` warm/kalt | ≤ 50 / ≤ 200 |
| `warmup_s` | ≤ 5 |
| `disk_savings_pct` | ≥ 15 |

Mindestens sechs Tests prüfen Build, künstliche Regression, Baseline,
Varianz über drei Läufe, air-gapped Betrieb und deterministische Seeds.
Abhängigkeit: Phase H. Aufwand: 8–10 Stunden.

## Phase K — Release-Readiness

Deliverables:

- `docs/API.md`, `docs/OPERATION.md`, `docs/ARCHITECTURE.md`.
- Release-Profil (LTO, strip), Multi-Stage-`Dockerfile`, CI-Workflow.
- Dieser Abschlussplan und ein automatisierter Readiness-Check.

Tests: Release-Binary unter 50 MB, Docker-Start unter fünf Sekunden,
CI unter zehn Minuten und fehlerfrei rendernde Dokumentation. Aufwand:
6–8 Stunden. K ist die letzte Phase und hängt von allen anderen ab.

## Reihenfolge und Abhängigkeiten

```text
J ─┬─→ I ──┐
   │       │
   └─→ H ──┼─→ G ──→ K
           │
B ─────────┴─→ F
```

Woche 1: J, dann I. Woche 2: H und F. Woche 3: G, dann K.

## Automatisierter DoD-Check

Der abschließende Check führt mindestens diese Schritte mit fehlerschlagender
Shell aus: `cargo test`, Benchmark-Gates, `cargo build --release`,
Binary-Größenprüfung und einen air-gapped Smoke-Test. Jede Gate-Verletzung
beendet den Check mit einem Fehlercode; nur ein vollständig grüner Durchlauf
meldet „ENGINE READY FOR INTEGRATION“.

## Harte Regeln

1. Keine Integration vor grünem DoD-Check.
2. Keine spekulativen Zielprojektpfade.
3. Keine parallelen Integrations- und Engine-Phasen.
4. Keine „100 % fertig“-Aussage ohne DoD-Evidence.

Nächster Schritt nach dem Commit dieses Dokuments ist ausschließlich Phase J.
