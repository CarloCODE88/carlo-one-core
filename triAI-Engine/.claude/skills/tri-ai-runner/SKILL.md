---
name: tri-ai-runner
description: Betriebsregeln und Arbeitsteilung für das TRI-AI-Runner-Projekt (lokaler Rust-Model-Runner für llama.cpp/Ollama/LM Studio). Lade dies, sobald an `tri-ai-runner` gearbeitet wird oder INGRIED/qwen2.5-coder als lokale Modelle eingebunden werden sollen — legt die serielle Single-Model-Regel, die Rollenaufteilung Claude/INGRIED/qwen-coder, den aktuellen Rust-Stand und den sicheren Arbeitsablauf fest.
user-invocable: true
---

# TRI-AI Runner — Betriebsregeln

Diese Maschine nutzt lokale LLMs nur **seriell**: Zu jedem Zeitpunkt darf genau
ein Modell geladen und inference-fähig sein. Starte weder parallele
Ollama-Aufrufe noch einen zweiten `llama-server`- oder LM-Studio-Worker. Ein
globaler OS-Lock (`/tmp/tri-ai-runner-single-model.lock`, flock) erzwingt das
bereits im Runner-Code selbst — Zusatzlogik in Skripten/Agents darf diese
Regel niemals umgehen, auch nicht "nur kurz zum Testen".

## Zuständigkeiten

- **INGRIED** — Recherche, Architektur- und Code-Auswertung.
  Ollama-Modell: `goekdenizguelmez/JOSIEFIED-Qwen3:8b`
- **Qwen Coder 14B** — gezielte Rust-Implementierung und Code-Änderungen,
  ein Modul/eine eng gefasste Aufgabe pro Aufruf.
  Ollama-Modell: `qwen2.5-coder:14b`
- **Claude** — arbeitet direkt am Rust-Code, prüft/korrigiert die Vorschläge
  von INGRIED und qwen-coder, integriert sie, führt Tests aus. Startet echte
  lokale Modelle nur, wenn ein Smoke-Test ausdrücklich nötig ist.

Nicht gleichzeitig verwenden. Vor einem Rollenwechsel immer laufende
Inferenz beenden und das zuvor genutzte Modell entladen (`ollama stop
<model>` bzw. den Runner-eigenen Stop-Pfad).

### Praktisches Muster für INGRIED/qwen-coder-Aufrufe

Ollama läuft als systemd-Dienst; prüfen und bei Bedarf starten:
```bash
systemctl is-active ollama.service || sudo systemctl start ollama.service
```
Eng gefasster, einzelner Auftrag statt großer Mehrzweck-Prompts:
```bash
curl -s http://127.0.0.1:11434/api/generate -H "Content-Type: application/json" -d '{
  "model": "qwen2.5-coder:14b",
  "prompt": "<eine präzise, einzelne Aufgabe>",
  "stream": false,
  "think": false,
  "options": {"temperature": 0.1, "num_predict": 800}
}'
```
Antwort **immer** selbst gegen den tatsächlichen Code verifizieren (Typen,
Signaturen, Borrow-Checker) — lokale Modelle liefern plausibel aussehenden,
aber nicht immer kompilierbaren oder korrekten Code. Bei einem Fehler: den
konkreten Compiler-/Testfehler zurück an dasselbe Modell geben (eng gefasst,
mit Hinweis auf die falsche Ursachenanalyse falls ein erster Fix-Versuch
scheitert), nicht selbst stillschweigend improvisieren, bevor ein zweiter
gezielter Versuch gemacht wurde.

## Relevante Projektpfade

- TRI-AI Runner: `/home/carlos/Documents/Codex/2026-09-05/er/tri-ai-runner`
  (eigenes git-Repo, Branch `master`, seit 2026-09-05)
- Produkt-/Architekturplan: `/home/carlos/Documents/Codex/2026-09-05/er/outputs/INGRIED_TRI_AI_RUNNER_PLAN_REWORKED.md`
- Runner-Spezifikation (V1-Endpunkte, Modulaufteilung, Abnahmekriterien):
  `/home/carlos/Documents/Codex/2026-09-05/er/outputs/TRI_AI_RUNNER_V1_SPEC.md`
- Arbeitsteilung/Rollen im Detail: `/home/carlos/Documents/Codex/2026-09-05/er/outputs/TRI_AI_WORK_SPLIT.md`
- Weitere Prompts/Pläne: `/home/carlos/Documents/Codex/2026-09-05/er/outputs/*.md`
  (u.a. `TRI_AI_ENGINE.md`, `INGRIED_TRI_AI_RUNNER_SYNTHESIS.md`,
  `PROMPT_QWEN_CODER_PHASE4.md`, `PROMPT_CLAUDE_CODE_REVIEW.md`)
- Referenzdateien (Ollama/llama.cpp/LM-Studio/Jan-Vergleich): `/home/carlos/Documents/Codex/2026-09-05/er/work/reference`
- Vorhandener, produktiver llama.cpp-Runner (anderes Projekt, nicht
  anfassen ohne Auftrag): `/home/carlos/PROJEKTE/franz-studio-runner`

`/home/carlos/Documents/Codex/2026-09-05/er/` enthält daneben noch ein
unabhängiges GUI-Projekt (`franz-chat-gui`, eigenes `Cargo.toml`/`src/`) und
leere, nie initialisierte `.git`/`.agents`/`.codex`-Verzeichnisse auf
oberster Ebene — das sind keine aktiven Repos, nicht verwirren lassen mit
dem echten Git-Repo unter `tri-ai-runner/`.

## Aktueller Rust-Stand (bei Sessionstart hier verifizieren, nicht blind übernehmen)

Der Runner enthält:

- Ressourcenplanung für GPU, RAM und SSD (`src/main.rs::plan`, `src/resources.rs::read`);
- transaktionales SSD-Staging mit Prüfsummen, Recovery und geprüftem Restore
  (`src/staging.rs`: `stage_bytes`, `recover`, `restore_active`);
- globalen Single-Model-Lock (`/tmp/tri-ai-runner-single-model.lock`, in `src/supervisor.rs`);
- Worker-Supervisor für `llama-server` mit `--parallel 1` (`src/supervisor.rs`);
- explizite Engine-Zustände: `Idle`, `Loading`, `Ready`, `Busy`, `Stopping`, `Failed` (`src/engine.rs`);
- API-Vertrag mit `model_busy` für belegte Modell-Slots (`src/api.rs`);
- dependency-arme HTTP/1.1-Transportschicht über `std::net` (`src/http.rs`),
  seriell durch einen einzigen `Mutex<Engine>` erzwungen, HTTP 409 bei
  belegtem Slot, HTTP 202/400/500 je nach Fall.

Vor jeder Weiterarbeit `git log --oneline` und die betroffenen Dateien lesen
— dieser Stand veraltet, sobald neue Commits dazukommen.

## Sichere Arbeitsweise

1. Erst den aktuellen Code lesen, keine Architektur neu erfinden.
2. Kleine, isolierte Änderung umsetzen (ein Modul/eine Datei pro Schritt,
   wenn mehrere Agents parallel arbeiten: nicht überlappende Dateien
   zuweisen).
3. `cargo fmt --all` ausführen.
4. `cargo test --offline` ausführen — bei parallelen `cargo test`-Threads,
   die einen echten Stub-Worker-Prozess halten (denselben globalen
   `/tmp`-Lock oder dieselbe Env-Variable nutzen), Tests über ein
   `static Mutex<()>` im Testmodul serialisieren, sonst Flakiness durch
   Selbstkonkurrenz.
5. Nur bei Bedarf einen Stub-Prozess für den Supervisor benutzen (z.B. ein
   kleines `#!/bin/sh`-Skript, das alle Argumente ignoriert und `sleep`
   aufruft — echte Coreutils wie `sleep`/`cat` scheitern an den vom
   Supervisor fest vorgegebenen `--model/--host/--port/...`-Flags).
6. Keine echten Modelle automatisch starten oder herunterladen.

## Falls ein echter lokaler Modelltest nötig ist

1. Vorher prüfen, dass kein lokaler Modellprozess läuft.
2. Nur einen Runner starten, bevorzugt den vorhandenen `llama-server`
   (`vendor/bin/llama-cpp/llama-server` im Projekt).
3. Immer `--parallel 1` setzen.
4. Nach dem Test sauber stoppen und warten, bis GPU/RAM freigegeben sind
   (`nvidia-smi` zur Kontrolle).
5. Ergebnis, Modellname, Kontextgröße und GPU-Layer dokumentieren.

Für die RTX 2080 Ti (11 GB VRAM) ist bei kleineren GGUF-Modellen
vollständiges GPU-Offload möglich. Größere Modelle müssen mit harten
VRAM-/RAM-Reserven geplant werden. SSD ist ein Sicherheits- und
Staging-Puffer, kein Ersatz für unkontrolliertes Swapping.
