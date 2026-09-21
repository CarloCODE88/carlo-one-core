# triAI-Engine – Statusbericht und ausführlicher Coding-Plan

Stand: 2026-09-14

## 1. Bereits vorhanden / aktueller Stand

### 1.1 Ausgangsprojekt

Der Ausgangspunkt ist `tri-ai-runner`. Der neue Engine-Worktree liegt unter:

```text
/home/carlos/PROJEKTE/triAI-Engine
Branch: triAI-engine
```

Der ursprüngliche Runner-Worktree bleibt separat und wurde nicht zurückgesetzt.

### 1.2 Übernommener Engine-Quellcode

Der neue Worktree enthält nur die lokale Engine-Schicht:

```text
src/api.rs
src/assistant.rs
src/attachments.rs
src/coding_tools.rs
src/config.rs
src/download.rs
src/engine.rs
src/gguf_registry.rs
src/http.rs
src/mcp.rs
src/model_catalog.rs
src/model_registry.rs
src/model_sources.rs
src/observability.rs
src/openai.rs
src/performance.rs
src/persistence.rs
src/planner.rs
src/resources.rs
src/staging.rs
src/supervisor.rs
src/tool_registry.rs
```

GUI, Design-Editor, Notebook und Frontend wurden nicht übernommen. `main.rs`
ist ein schlanker Engine-Server-Entrypoint. `lib.rs` exportiert die Engine-
Module für spätere Einbettung in andere Programme.

### 1.3 Bereits implementierte Funktionen

- einzelner aktiver Modell-Worker
- kontrolliertes Starten, Stoppen und Wechseln von Modellen
- Readiness-, Timeout-, Crash- und Port-Konfliktbehandlung
- OpenAI-kompatible Chat-/Embedding-Routen
- Tool-Runden mit Pfad-/Tool-Sicherheitsprüfung
- GGUF-Scan und Modellkatalog
- Ressourcen-Lanes `GpuFast`, `RamBalanced`, `CpuSafe`, `Reject`
- Staging mit Wiederherstellung unvollständiger Transfers
- Download- und Rescan-Verwaltung
- technische JSONL-Observability
- rekursive Secret-Redaction
- `performance.rs` mit Transfer- und Pipeline-Metriken

### 1.4 Datenbanken, Skills und Modelle

Die Routing-/Knowledge-Database wurde kopiert nach:

```text
data/knowledge/tri-routing-knowledge.json
```

Das interne Nutzungs-Skill liegt unter:

```text
skills/tri-routing-db/SKILL.md
```

Lokale Mini-Modelle:

```text
models/primary-mini-ingried-qwen3-8b-q4_k_m.gguf
models/fallback-dolphin3-llama31-8b-q4_0.gguf
models/manifest.json
```

INGRIED ist das primäre Mini-Modell für Klassifikation, Routing-Beratung und
Hintergrundanalyse. Dolphin3 ist der sekundäre Fallback. Die Dateien sind
Git-ignored, ihre SHA-256-Digests stehen im Manifest.

### 1.5 Dokumentation

- `README.md`: Projektüberblick
- `docs/USAGE.md`: API-, Konfigurations-, Lifecycle-, Diagnose- und
  Integrationsanleitung
- dieses Dokument: Gesamtstatus und ausführbarer Implementierungsplan
- `docs/evidence/phase-0-inventory.md`: verifizierte Ist-Aufnahme und Gaps
- `docs/evidence/phase-1-contract-security.md`: Sicherheits-/Vertrags-Evidence

### 1.6 Zweigeteilte Engine

Die Architektur trennt verbindlich den deterministischen Control-/Execution-
Pfad vom optionalen Analyse-/Learning-Pfad. Policy, Auth, Budgets, Pfadregeln
und Zustandsübergänge liegen im harten Pfad. Das Mini-Modell darf dort nur
innerhalb eines begrenzten Advice-Vertrags Vorschläge liefern; es kann niemals
Auth, Budget, Capability oder Sicherheitsentscheidungen überschreiben. Der
Learning-Pfad arbeitet asynchron bzw. best-effort, speichert nur redigierte
abgeleitete Daten und wird bei Fehlern verworfen oder zurückgestellt.

Die technische Übergabe zwischen beiden Hälften erfolgt ausschließlich über
typisierte, begrenzte Daten: Request-/Run-ID, Modell-Digest, Policy-Version,
Budgetklasse, abgeleitete Aufgabenflags, Messwerte und Ergebnis-Klassen. Ein
Analysejob darf nur einen `Advice`-Wert erzeugen; der Control-Pfad prüft diesen
gegen Capability-, Ressourcen- und Budgetregeln. Bei Timeout, ungültigem Advice
oder fehlender Evidenz greift die deterministische Standardregel. Rohprompt,
Rohcode und Rohantwort bleiben außerhalb des Learning-Stores.

## 2. Research-Ergebnisse und Schlussfolgerungen

### 2.1 Modellarchitekturen

Qwen3-Coder 30B-A3B ist ein MoE-Modell mit 48 Layern, 128 Experten und acht
Experten pro Token. Die „aktiven Parameter“ bedeuten nicht, dass nur wenige
Dateibereiche geladen werden: Router, Shared-Teile, KV-Cache und die jeweils
benötigten Experten müssen zusammen betrachtet werden.

Qwen2.5-Coder 14B und Qwen3 14B sind dichte Modelle. Alle Layer nehmen an der
Berechnung teil; dynamische Expertencaches sind dort nicht anwendbar. Möglich
sind Layer-Gruppen, Vorladen des nächsten Layer-Fensters und CPU/RAM-Offload.

Primärquellen:

- [Qwen3-Coder config](https://huggingface.co/Qwen/Qwen3-Coder-30B-A3B-Instruct/blob/main/config.json)
- [Qwen2.5-Coder config](https://huggingface.co/Qwen/Qwen2.5-Coder-14B-Instruct/blob/main/config.json)
- [Qwen3 config](https://huggingface.co/Qwen/Qwen3-14B/blob/main/config.json)

### 2.2 GGUF, Quantisierung und Sharding

GGUF speichert Tensorname, Datentyp und Byteoffset. Daher kann ein externer
Tensorindex Bereiche gezielt adressieren. `gguf-split` unterstützt
Tensorgrenzen-aware Shards; Byte-Splitting, das Tensoren mitten im Block teilt,
ist ungeeignet.

Quantisierung spart den größten Speicherplatz. Eine imatrix-Kalibrierung kann
Qualitätsverluste reduzieren. Ein nachträgliches monolithisches ZIP ist für
`mmap` und Random Access jedoch schlecht, weil vor Nutzung das gesamte Archiv
oder große Teile entpackt werden müssten.

Primärquellen:

- [llama.cpp Quantisierung](https://github.com/ggml-org/llama.cpp/blob/master/tools/quantize/README.md)
- [GGUF-Splitting](https://github.com/ggerganov/llama.cpp/discussions/6404)
- [GGUF-Format](https://github.com/ggml-org/llama.cpp/blob/master/ggml/include/gguf.h)

Schlussfolgerung: GGUF bleibt das Laufzeitformat. Kalte Tensor-/Layer-Chunks
dürfen zusätzlich unabhängig komprimiert werden, müssen aber einen Index,
Prüfsumme und eine asynchrone Entpackstufe besitzen.

### 2.3 Speicher- und Transferpfad

Für die RTX 2080 Ti mit 11 GB VRAM ist der sinnvolle Pfad:

```text
SSD-Cold-Chunk → RAM → pinned RAM → VRAM-Hotset
```

SSD-Zugriffe pro Decode-Token würden die Latenz dominieren. Pinned RAM ist für
asynchrone Host→Device-Transfers geeignet, muss aber begrenzt werden, damit
das Betriebssystem nicht unter Speicherdruck gerät. Der KV-Cache hat Vorrang
vor spekulativem Prefetch.

Die CUDA-Dokumentation beschreibt asynchrone Ausführung und Host-Speicher-
Transfers ([CUDA Programming Guide](https://docs.nvidia.com/cuda/cuda-programming-guide/02-basics/asynchronous-execution.html)).

### 2.4 MoE-Prefetch und Lernen

MoE-Infinity zeigt, dass Aktivierungstracing, Expertencaching und Prefetching
große Vorteile bringen können, die Wirkung aber von Router-Lokalität und
Workload abhängt ([MoE-Infinity](https://arxiv.org/abs/2401.14361)). Deshalb
muss TRI reale Router-IDs, Cache-Hits, Reuse-Distance, H2D-Bytes und Qualität
aufzeichnen, statt Aktivierungsbereiche zu behaupten.

### 2.5 SSD-Dateisystem

Für eine dedizierte Runner-Partition ist ext4 die konservative Baseline:
vorhersehbare Page-Cache-/mmap-Eigenschaften, mature Recovery und gute
Extent-Allokation für große Modelldateien ([ext4 allocator documentation](https://docs.kernel.org/filesystems/ext4/allocators.html)).
Trim soll periodisch erfolgen, nicht bei jedem Cache-Ereignis
([fstrim manual](https://man7.org/linux/man-pages/man8/fstrim.8.html)).

Partitionierung und `mkfs` bleiben ein separater, interaktiv bestätigter
Administrationsschritt.

## 3. Finaler Coding-Plan

Der folgende Plan ist so aufgeteilt, dass ein Agent ihn ohne weiteren
Gesprächskontext abarbeiten kann.

### Phase A – Projektbasis stabilisieren

1. In `/home/carlos/PROJEKTE/triAI-Engine` arbeiten.
2. Branch `triAI-engine` beibehalten.
3. Keine Dateien aus `models/`, `data/evidence/`, `runtime/`, `target/` oder
   Eventlogs committen.
4. Prüfen, dass `Cargo.toml` den Paketnamen `tri-ai-engine` verwendet und
   `src/main.rs` `tri_ai_engine` importiert.
5. Fehlende Config als `config/example.toml` dokumentieren, nicht als
   produktive Secret-/Maschinenkonfiguration committen.
6. Ausführen:

```bash
cargo fmt --all
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Akzeptanz: Build, Tests und Clippy grün; kein GUI-Modul wird kompiliert.

### Phase B – Runtime-Evidence-Datenbank

1. Eine lokale SQLite-Schicht oder zunächst append-only JSONL unter
   `data/evidence/` implementieren.
2. Tabellen/Records anlegen:

```text
prompt_runs
  run_id, timestamp, prompt_hmac, model_id, model_digest
  knowledge_area, task_type, tools, context_quality
  context_tokens, requested_quality, latency_budget

transfer_samples
  run_id, kind, bytes, elapsed_us, queued_us, stall_us
  source_region, destination_region, chunk_id

residency_samples
  run_id, layer_id, expert_id, state, hit, reuse_distance
  vram_mb, pinned_ram_mb, ram_mb, ssd_bytes

outcomes
  run_id, tokens_in, tokens_out, tokens_per_second
  first_token_ms, p95_latency_ms, tool_calls, retry_count
  quality_score, user_correction, error_code

policies
  policy_id, model_id, flags, prefetch_depth, cache_budget_mb
  cpu_threads, confidence, created_at, parent_policy_id
```

3. Keine Rohprompts, API-Keys oder Toolinhalte persistieren.
4. Alle Records mit Modell-Digest und Policy-ID versionieren.
5. Rotation/Retention implementieren, z. B. Rohtelemetrie 14 Tage,
   aggregierte Muster dauerhaft.

Akzeptanz: Ein Chatlauf erzeugt eine geheime-freie Evidence-Kette, die mit
`jq`/SQLite auswertbar ist.

### Phase C – Live-Monitoring und Trigger

1. `PipelineWindow` und `TransferSample` in die tatsächlichen Ladepfade
   einbauen.
2. Alle 100 ms Ressourcen-Snapshot erfassen, aber niemals synchron auf SSD
   warten.
3. Trigger aus `data/knowledge/tri-routing-knowledge.json` implementieren:

```text
VRAM kritisch       → Prefetch stoppen, kalte Region evicten
VRAM fällt schnell   → Prefetch reduzieren, Residency neu planen
KV wächst stark      → KV budgetieren, optionalen Cache verdrängen
MoE-Hit-Rate niedrig → vorhergesagte Experten laden
Prefetch falsch      → spekulatives Prefetch deaktivieren
Queue hoch           → Transfers bündeln
SSD im Decode        → synchronen SSD-Read abbrechen
Kontext fast voll    → Kontext komprimieren/zusammenfassen
```

4. Hysterese und Cooldowns verwenden, damit kein Thrashing entsteht.
5. Harte Regeln vor Mini-Modell-Regeln auswerten.

Akzeptanz: Jeder Trigger ist deterministisch testbar und erzeugt eine
Policy-/Transferentscheidung ohne Decode-Blockierung.

### Phase D – Storage- und Chunk-Loader

1. `tri-model-pack` als separates Binary oder Modul erstellen.
2. Original-GGUF read-only als Rollback registrieren.
3. Tensoren per GGUF-Metadaten klassifizieren:

```text
metadata/tokenizer, embeddings/output, norms/biases,
attention, dense-FFN, MoE-router, MoE-expert, KV-runtime
```

4. Tensorgrenzen-aware Shards/Chunks erzeugen.
5. Für kalte Chunks unabhängige zstd-Frames, Offsetindex und Checksummen
   schreiben.
6. Kein vollständiges Entpacken beim Start.
7. Warmup lädt Metadaten, Tokenizer, Router, Norms und erste vorhergesagte
   Regionen.
8. Bei fehlerhaften Checksummen auf Original-GGUF zurückfallen.

Akzeptanz: Der Loader kann einen einzelnen Chunk laden, ohne das ganze Modell
zu entpacken; GGUF-Digest und Tensorreihenfolge bleiben validierbar.

### Phase E – Primär-/Fallback-Mini-Modell

1. Manifest unter `models/manifest.json` laden.
2. Primärmodell INGRIED starten, wenn VRAM/RAM-Lane verfügbar ist.
3. Fallback Dolphin3 starten, wenn:
   - Primärmodell nicht readiness-fähig wird
   - Mini-Modell timeoutet
   - Antwort ungültiges JSON/Schema liefert
   - VRAM-Druck die Primärnutzung verhindert
4. Fallback darf nicht gleichzeitig mit dem Primärmodell laufen, außer ein
   späterer Ressourcenplan weist explizit zwei Slots aus.
5. Advice-Schema strikt validieren:

```json
{
  "knowledge_area":"software_engineering",
  "task_type":"code_review",
  "required_tools":["read_file"],
  "context_quality":"repository_grounded",
  "active_area_estimate":"dense_layer_window",
  "prefetch_depth":1,
  "confidence":0.0
}
```

6. Timeoutbudget maximal 20 ms für Live-Advice; bei Überschreitung
   deterministische Policy verwenden.

Akzeptanz: Primary-Ausfall führt ohne Prozesschaos zum Fallback; harte
Sicherheitsregeln bleiben aktiv.

### Phase F – Hintergrundanalyse

1. Asynchronen Analyst-Thread/Jobqueue einrichten.
2. Jobs ausführen:
   - Prompt-Cluster alle 15 Minuten
   - Transferengpässe alle 30 Minuten
   - MoE-Hotset alle 10 Minuten
   - Modellkalibrierung alle 30 Minuten
   - Anomalieerkennung alle 5 Minuten
   - modellübergreifende Capability-Matrix alle 6 Stunden
3. Nur aggregierte Fenster an das Mini-Modell geben.
4. Neue Policies zunächst als Vorschlag speichern.
5. Promotion erst bei Mindestanzahl an Läufen und zwei erfolgreichen
   Modellbeobachtungen erlauben.
6. Rollback nach drei regressiven Fenstern oder 20 % P95-Verschlechterung.

Akzeptanz: Hintergrundanalyse blockiert keinen Chat und erzeugt reproduzierbare
Policy-Versionen.

### Phase G – Benchmarks und Qualitätsgates

Für jedes Modell und jede Policy messen:

```text
Disk-Ersparnis
Startup-Zeit
First-token-Latenz
Decode-P50/P95
Tokens/s
SSD-/H2D-/D2H-Bytes
Prefetch-Hit-Rate
VRAM-Peak
RAM-/pinned-RAM-Peak
Qualität/Perplexity/Task-Score
```

Gates:

- mindestens 15 % Speicherersparnis
- maximal 10 % Startup-Verlust
- maximal 5 % Decode-Verlust nach Warmup
- maximal 5 % Qualitätsverlust
- kein SSD-Read im Decode
- kein OOM in 30-minütigem Dauerlauf

Bei Fehlschlag bleibt das Original aktiv.

### Phase H – SSD-Preparator

1. Eigenes Tool `tri-storage-prepare` erstellen.
2. Standardmodus ist Dry-Run.
3. Gerät anhand Pfad, Seriennummer, Root-/Boot-Status, Mounts und SMART
   prüfen.
4. GPT/1-MiB-Ausrichtung, ext4, Label `TRI_RUNNER`, Mountpoint
   `/var/lib/tri-ai-runner` planen.
5. Erst nach expliziter Bestätigung partitionieren/formatieren/mounten.
6. Runner-User und Rechte `0750`/`0640` setzen.
7. Kein `CAP_SYS_ADMIN` im normalen Runner.
8. `fio` read-only, mmap-Smoke-Test, fstrim-Prüfung und Cold/Warm-Benchmark
   ausführen.

Akzeptanz: Kein falsches oder bereits genutztes Gerät kann automatisch
überschrieben werden.

### Phase I – Integration, Dokumentation und Release

1. API-Schema und Rust-Typen dokumentieren.
2. Beispiele für Python, Rust und curl ergänzen.
3. systemd-Unit ohne privilegierte Runtime-Rechte bereitstellen.
4. Health-/Metrics-Endpunkt ergänzen.
5. Secret-Redaction und Pfadschutz als Integrationstests absichern.
6. Release-Artefakte ohne Modelle bauen; Modelle separat über Manifest/Digest
   beziehen.
7. Abschlussprüfung:

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

## 4. Offene Punkte zum aktuellen Stand

- Runtime-Evidence ist noch nicht in den HTTP-/Workerpfad integriert.
- `performance.rs` ist vorhanden, aber noch nicht an jeden realen Transfer
  angeschlossen.
- Kompressions-/Chunk-Loader ist spezifiziert, aber noch nicht implementiert.
- SSD-Preparator ist nur geplant und wurde nicht ausgeführt.
- Mini-/Fallback-Modellmanifest ist vorhanden; automatisches Starten und
  Umschalten muss noch in Supervisor/Engine verdrahtet werden.
- Der neue Worktree benötigt einen abschließenden unabhängigen Volltest.

## 5. Zweite Learning-Database

Die optionale Ausführungsdatenbank liegt unter
`data/learning/model-learning-database.json`. Sie ist absichtlich von der
Routing-Seed-Datenbank getrennt. `src/learning.rs` stellt den typisierten
`LearningRecord` und einen deaktivierten-by-default JSONL-Store bereit.

Erfasst werden können neben Speicherwerten auch komplette technische
Trajektorien: Toolreihenfolge, Argument-Hashes, Exitcodes, Fehlerklassen,
Tests, Lint-Ergebnisse, Patch-Merkmale, Korrekturen, Modellqualität,
Kontextmerkmale, Ressourcen und modellübergreifende Vergleiche.

Die Datenbank enthält bereits die Zielschemata für spätere Datensätze:

- Routing-Supervision
- Tool-Use-SFT
- Code-Repair-SFT
- Preference-Paare für DPO/Reranking
- modellübergreifende Capability-Matrix

Rohprompt, Rohcode und Rohantwort bleiben standardmäßig ausgeschlossen. Ein
späterer Collector muss Consent, Secret-Scan, Retention und Löschung erzwingen.
