# triAI-Engine – vollständiges Usage-Schema

## Zweck und Architektur

`triAI-Engine` ist die lokale Engine-Schicht zwischen einem Hostprogramm und
einem kompatiblen `llama-server`. Sie verwaltet einen überwachten Worker,
plant VRAM/RAM/SSD und bietet OpenAI-kompatible Routen.

```text
Host → HTTP/API → Planner → Engine/Supervisor → llama-server → CUDA/VRAM
                                      └→ Transfer-/Pipeline-Telemetrie
```

GUI, Design-Editor, Notebook, Frontend und Modellartefakte sind nicht Teil
dieses Worktrees.

## Build und Voraussetzungen

- Rust stable und Cargo
- lokaler kompatibler `llama-server`
- GGUF-Modell in einem erlaubten Pfad
- CUDA-Treiber für GPU-Betrieb

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

## Konfiguration und Start

Standard ist `tri-ai-runner.toml` im aktuellen Arbeitsverzeichnis. Alternativ:

```bash
TRI_AI_CONFIG=/etc/tri-ai-engine/config.toml cargo run --release
```

```toml
[server]
listen_addr = "127.0.0.1:8900"
inference_timeout_secs = 120
auth_token = ""

[paths]
project_root = "."
models_dir = "./models"
stage_dir = "./staging"
llama_server = "/usr/local/bin/llama-server"
event_log = "./tri-ai-events.jsonl"

[worker]
port = 8901
readiness_timeout_secs = 60
default_context = 4096
default_gpu_layers = 999

[reserves]
vram_mb = 1024
ram_mb = 2048
ssd_mb = 5120
```

```bash
./target/release/tri-ai-engine
```

Bei einer LAN-/externen Bind-Adresse ist ein nichtleerer Token zwingend:

```toml
[server]
listen_addr = "192.168.1.10:8900"
auth_token = "in-einer-secret-datei-setzen"
```

Clients senden entweder `Authorization: Bearer TOKEN` oder
`X-TRI-Auth: TOKEN`. Secrets gehören nicht in Git, Logs oder Evidence.

## Lifecycle

```text
Idle → Loading → Ready → Busy → Ready
                 ├────────────→ Degraded → Rollback → Ready
                 └────────────→ Failed
Ready → Draining → Stopping → Idle
```

Es gibt genau einen aktiven Modell-Slot. Ein Wechsel stoppt den alten Worker
vollständig und wartet auf dessen Ende, bevor der neue startet.

## Engine-Aktionen

```json
{"action":"status"}
{"action":"resources"}
{"action":"list_models"}
{"action":"start_model","model":"MODEL_ID"}
{"action":"start_model_planned","model":"MODEL_ID","plan_id":"PLAN_ID"}
{"action":"stop_model"}
{"action":"chat","model":"MODEL_ID","prompt":"TEXT"}
{"action":"stage_status"}
{"action":"rescan_models"}
```

Die typisierten Verträge stehen in `src/api.rs`.

## HTTP-API

Standardadresse: `http://127.0.0.1:8900`.

```text
GET  /v1/models
POST /v1/chat/completions
POST /v1/embeddings
GET  /health
GET  /metrics
GET  /api/engine/status       (Alias: /api/status)
GET  /api/engine/memory       (Alias: /api/resources)
GET  /v1/models               (Alias: /api/models)
POST /api/engine/start        (Alias: /api/engine; requires model + current plan_id)
POST /api/engine/plan
POST /api/engine/plan/auto
POST /api/models/rescan
GET  /api/stage/status
POST /api/downloads
GET  /api/downloads/{id}
POST /api/downloads/{id}/cancel
```

```bash
curl -s http://127.0.0.1:8900/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"MODEL_ID","messages":[{"role":"user","content":"Hallo lokal"}],"stream":false}'
```

Bei `stream: true` wird, sofern der Worker Streaming unterstützt, OpenAI-SSE
weitergereicht. Tool-Runden sind intern begrenzt und validiert.

## Ressourcenplanung

Der Planner verwendet VRAM, RAM, SSD, Modellgröße, GPU-Layer und Kontext/KV-
Cache. Die sicheren Lanes sind:

```text
GpuFast → RamBalanced → CpuSafe → Reject
```

`Reject` bedeutet, dass das Hostprogramm Modell, Quantisierung oder Kontext-
größe anpassen muss.

## Storage-Planung (Dry-Run)

`tri_ai_engine::storage::plan_chunks` erstellt nur einen überprüfbaren,
reversiblen Layout-Plan. Das aktive Original-GGUF bleibt erhalten. Es werden
keine Partitionen, Mounts oder Modelldateien automatisch verändert. Zstd-
Kompression darf erst nach Tensorgrenzen-Index, Checksummen- und
Qualitätsbenchmark als separates Artefakt aktiviert werden.

## Transfer- und Pipeline-Metriken

`src/performance.rs` quantifiziert SSD→RAM, RAM→pinned-RAM,
pinned-RAM→VRAM, VRAM→pinned-RAM, pinned-RAM→RAM und RAM→SSD.

```text
bandwidth_gbps = bytes / elapsed_us / 1000
payload_ratio  = elapsed_us / (elapsed_us + queued_us)
predicted_us   = elapsed_us × requested_bytes / measured_bytes
vram_pressure  = clamp(1 - free_vram / usable_vram, 0, 1)
```

Ein `PipelineWindow` misst zusätzlich Klassifikation, Kontextladen, Toolzeit,
Decode und Finalisierung. Die Schnellentscheidung schützt zuerst VRAM,
verbietet SSD-Reads im Decode, schützt den KV-Cache und bündelt erst danach
Transfers oder startet Prefetch. Das Mini-Modell darf diese Regeln nicht
überstimmen.

## Modelle, Kompression und Storage

GGUF bleibt das Laufzeitformat; Originaldateien bleiben als Rollback erhalten.
Metadaten, Tokenizer, Router und Norms werden heiß gehalten. MoE-Experten
werden über Router-Trace, Hit-Rate und Reuse-Distance vorgeladen. Dichte
Modelle erhalten Next-Layer-Prefetch. SSD ist Cold-/Warm-Storage und wird
nicht synchron pro Token gelesen.

Die Knowledge-Database liegt unter
`data/knowledge/tri-routing-knowledge.json`; die Nutzungsregeln liegen in
`skills/tri-routing-db/SKILL.md`.

## Sicherheit und Fehlercodes

Der normale Prozess benötigt keine Rechte für Partitionierung, `mkfs` oder
Mounting. Pfade, Tools und Tool-Runden werden geprüft; Secrets werden vor der
JSONL-Ausgabe maskiert.

```text
model_busy, stale_plan, invalid_request, not_ready, resource_denied,
worker_timeout, worker_crashed, out_of_memory, port_conflict,
corrupt_model, not_found, internal
```

Bei `resource_denied` Kontext/Quantisierung/Modell reduzieren; bei OOM
Prefetch stoppen und kalte Regionen evicten; bei `stale_plan` Ressourcen neu
lesen und erneut planen.

## Rust-Einbettung

```rust
use tri_ai_engine::{engine::Engine, performance::PipelineWindow};

let engine = Engine::new("./staging")?;
let window = PipelineWindow {
    classify_us: 100, context_load_us: 2_000, decode_us: 50_000,
    tool_us: 0, finalize_us: 100, vram_free_mb: 4_000,
    usable_vram_mb: 10_000, kv_cache_mb: 1_000,
    h2d_bytes: 16 * 1024 * 1024, d2h_bytes: 0, ssd_bytes: 0,
};
println!("pressure={:.2}", window.vram_pressure());
drop(engine);
```

## Diagnose und Release

```bash
curl -s http://127.0.0.1:8900/api/engine/status | jq
curl -s http://127.0.0.1:8900/api/engine/memory | jq
tail -f tri-ai-events.jsonl | jq
```

Vor Integration: Konfigurationspfad, `llama-server`-Bibliotheken, erlaubte
Modell-Roots, Eventlog-Redaction, realistische Reserven und Start→Chat→Stop
testen. Danach fmt, test, clippy und `cargo build --release` ausführen.
