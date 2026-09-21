# triAI-Engine — Abschlussbericht

Stand: 2026-09-15  
Branch: `triAI-engine`  
Letzter Commit: `defe81a`

## Ergebnis

Die Engine-Implementierung der Phasen B–K ist technisch durchgezogen,
versioniert und testbar. Die reale Betriebs-Evidence zeigt jedoch, dass die
strengen Performance-Gates derzeit nicht erfüllt sind. Deshalb ist die Engine
noch nicht als production-ready für eine externe Integration freigegeben.

## Umgesetzte Bausteine

| Bereich | Ergebnis |
|---|---|
| Evidence | JSONL, Rotation, Redaction und Secret-Schutz |
| Monitoring | Ressourcen-Snapshots, Trigger, Hysterese |
| Storage | GGUF-Parser, Tensorindex, zstd-Packer, LRU-Loader, Warmup, read-only Rollback |
| Supervisor | Primary/Fallback, Advice-Validierung, Single-Slot-Guard |
| Prompt | Kompression, Token-Schätzung, Caps und deterministische Hooks |
| HTTP | Health, Modellliste, Chat/SSE, Plan/Start, serverseitige Identität |
| Analyse | Aggregierte Fenster, Policy-Versionierung, Promotions-/Rollback-Gates |
| Release | Release-Profil, Dockerfile, CI, API-/Betriebs-/Architekturdokumentation |
| Benchmarks | Quality-Gate-CLI, Messskripte, Chunk-Benchmark |

## Wichtige Fixes

- GGUF-Metadaten mit 40960 Kontext wurden nicht mehr unkritisch übernommen;
  automatische Planung ist auf den konfigurierten Default 4096 begrenzt.
- Der Supervisor wartet jetzt auf HTTP-Health `200`, statt einen nur offenen
  TCP-Port als inference-ready zu akzeptieren.
- Voll-Offload lädt die Output-Projektion mit; dadurch stieg die gemessene
  Geschwindigkeit deutlich.
- Verwaiste Mess-Worker wurden bereinigt; der vorherige `model_busy`-Flake war
  ein Prozessartefakt, kein persistenter Slotfehler.

## Test-Evidence

- `cargo test --locked`: **296 Unit-Tests**, **1 dynamischer Integrationstest**
  und **3 Warmup-Integrationstests** erfolgreich.
- Release-Build erfolgreich; Binary-Größe: **5.054.328 Bytes**.
- `git diff --check`: erfolgreich.

## Reale Messungen

Modell: INGRIED Qwen3 8B Q4_K_M, SHA-256 `1de498fe…344dcf69d`, RTX 2080 Ti,
`ctx_size=4096`, Voll-Offload mit 37 Layer-Slots.

| Messung | Läufe |
|---|---:|
| Worker-Startup | 14484 / 6523 / 2102 ms |
| 256-Token-End-to-End | 2114 / 2971 / 3052 ms |
| Generierung | 82.31 / 76.40 / 74.38 Tok/s |
| Chunk-Cold-Load | 826 / 953 / 1015 ms |
| Chunk-Warm-Load | 222 / 225 / 215 ms |
| Eager-Warmup | 9.772 / 10.041 / 9.590 s |
| Disk-Ersparnis | 2.159 % |

Die vollständige temporäre Rohdatei war `/tmp/tri-real-benchmark-results.json`.
Sie wurde nicht ins Repository übernommen, da sie maschinen- und laufbezogene
Evidence ist.

## Gate-Bewertung

Das Quality-Gate wurde absichtlich mit den echten Werten ausgeführt und endet
mit Exit-Code 1:

- Startup-P95: 14,48 s (Grenze 500 ms)
- Inferenz-P95: 3,05 s (Grenze 300 ms)
- Chunk warm/kalt: 225 / 1015 ms (Grenzen 50 / 200 ms)
- Warmup-P95: 10,04 s (Grenze 5 s)
- Disk-Ersparnis: 2,16 % (Mindestwert 15 %)
- Drei-Lauf-Varianz Startup: 34,6 % (Grenze 5 %)

Ein grünes Gate zu behaupten wäre daher nicht evidenzbasiert.

## Freigabestatus

**Engine-intern:** abgeschlossen und regressionsgetestet.  
**Production-Readiness:** offen.  
**CarloCODE-/externe Integration:** bewusst nicht freigegeben.

## Empfohlene nächste Arbeiten

1. Performance-Gates fachlich auf Tok/s bzw. realistische P95-Ziele für diese
   GPU überprüfen und begründen; Grenzwerte nicht einfach überschreiben.
2. Startup-Varianz getrennt nach kaltem und warmem Dateicache erfassen.
3. Chunk-Kompression für quantisierte GGUF-Daten untersuchen; 15 % sind mit
   der aktuellen zstd-Strategie nicht erreicht.
4. Warmup auf tatsächlich benötigte Eager-Tensoren begrenzen und erneut messen.
5. Erst nach einem grünen, realen Readiness-Check die Integration beginnen.
