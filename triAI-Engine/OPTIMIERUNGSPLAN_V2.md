# triAI-Engine — Vollständiger Optimierungsplan V2.0
## Integration huxxel-cpp + Kernel-Worker + Research
## Stand: 2026-09-15 · Kanon v4 · user_id: frst-9F3K

---

## 📊 Aktueller Zustand (nach O1 + O5 + Kernel-Worker)

| Bereich | Status | Gate | Gemessen |
|---------|--------|------|----------|
| Tests | ✅ 300/300 grün | — | — |
| Worker-Binary | ✅ huxxel-cpp | — | 0.4.1-dev |
| Q8_0 KV-Cache | ✅ Aktiv | — | ~50% VRAM |
| V2-Packer | ✅ Funktioniert | — | 0.29% (Q4_K_M) |
| ExpertTracker | ✅ Implementiert | — | Socket-basiert |
| KernelWorker | ✅ Implementiert | — | VRAM/RAM/Sync |
| Async-Prefetch | ✅ Implementiert | — | Expert-getrieben |
| Startup-P95 | ❌ ROT | 3000ms | 14,48s |
| Chunk-Warm | ❌ ROT | 50ms | 252ms |
| Disk-Ersparnis | ❌ ROT | 10% | 0.29% |

---

## 🎯 Optimierungs-Phasen (vereint)

### Phase A: Basismigration (abgeschlossen ✅)
- [x] O1.1: Worker-Binary → huxxel-cpp
- [x] O1.2: Q8_0 KV-Cache aktiviert
- [x] O5: V2-Packer Pipeline (packer.rs fix)
- [x] Kernel-Worker Modul implementiert
- [x] ExpertTracker implementiert
- [x] Async-Prefetch implementiert

### Phase B: Startup-Optimierung ⭐ GATE-KRITISCH
**Ziel:** 14,48s → < 3000ms warm, < 8000ms cold
**Aufwand:** 5-7h

#### B.1: Kalt/Warm-Trennung
- Benchmark-Split mit `sync && echo 3 > /proc/sys/vm/drop_caches`
- Separate Messung für kalte und warme Starts
- Seiten-Cache-Flushing für echte Kalt-Messungen

#### B.2: Warmup-Strategie verfeinern
- Nur kritische Tensoren eager laden (3 Chunks statt alle)
- Metadata → Tokenizer → Router als eager
- Attention/FFN/MoE-Experten lazy

#### B.3: Lazy-Loading für nicht-eager Tensoren
- `load_on_demand()` für Attention/FFN/Experten
- Prefetch-Events vom ExpertTracker triggern
- KernelWorker.promote_to_vram() für Hot-Experts

#### B.4: Memory-Worker Optimierung
- KernelWorker.vram_pages Tracking für evictions
- Hot-Experts in VRAM pinen (kein Eviction)
- Cold-Experts auf RAM/Disk auslagern
- Unified Memory Hints über CUDA-API

**Akzeptanz:**
- Warmup < 5s
- Startup warm ≤ 3000ms
- Startup cold ≤ 8000ms
- Startup-Varianz ≤ 10%

### Phase C: Chunk-Load-Optimierung ⭐ GATE-KRITISCH
**Ziel:** Chunk-Warm 252ms → < 5ms, Chunk-Cold 1137ms → < 500ms
**Aufwand:** 4-6h
**Abhängigkeit:** Phase B

#### C.1: LRU-Cache-Effizienz
- Cache-Hit-Rate messen
- `load_chunk()` Cache-Check VOR Dekompression
- Hot-Chunks im Cache halten

#### C.2: mmap für Warm-Load
- `memmap2::Mmap` statt `fs::read` für warme Chunks
- Zero-copy Loading für gecachte Chunks
- KernelWorker.sync_expert_events() für Prefetch

#### C.3: Parallel Chunk Loading (rayon)
- Bereits implementiert in `load_eager()`
- `ids.par_iter().map(|id| self.load_chunk(id)).collect()`
- KernelWorker.load_to_vram() für jede Chunk parallel

**Akzeptanz:**
- Chunk-Warm < 5ms (Cache-Hit)
- Chunk-Cold < 500ms (mit Prefetch)

### Phase D: Disk-Ersparnis ⭐ GATE-KRITISCH
**Ziel:** ≥ 10% Disk-Ersparnis
**Aufwand:** 3-4h
**Abhängigkeit:** Keine (Code korrekt, Modell limitiert)

#### D.1: Modell-Format-Wechsel
- INGRIED Q4_K_M → FP16/FP32 Referenzmodell packen
- `compress_only_compressible: true` ist korrekt
- Unquantisierte Tensoren (Embeddings, Norms, Output) profitieren

#### D.2: Re-Pack mit Referenzmodell
```bash
./target/release/tri-model-pack models/ingried_fp16.gguf models/packed-v2
```

#### D.3: Disk-Ersparnis-Monitoring
- KernelWorker.get_memory_state() für VRAM/Größe
- Manifest `disk_savings_percent` prüfen
- Evidence-Store Eintrag

**Akzeptanz:**
- Disk-Ersparnis ≥ 10% (mit FP16-Referenz)

### Phase E: RTX 2080 Ti Optimierungen ⭐ HOHER ROI
**Ziel:** 200-250% Performance-Steigerung via Hardware-Optimierung
**Aufwand:** Laufend

#### E.1: Memory-OC auf 8000 MHz
```bash
nvidia-settings -a "[gpu:0]/GPUMemoryTransferRateOffset[3]=1500"
```
- +15-25% tokens/s
- Sofort anwendbar

#### E.2: KV-Cache auf Q8_0 quantisieren ✅ ERFOLGT
- Bereits in O1.2 konfiguriert
- 50% KV-VRAM-Ersparnis

#### E.3: CPU-Thread-Optimierung
- `--threads 8` statt `--threads 12` (nur physische Cores)
- `--cpu-strict 1` für Thread-Pinning
- +20-30% bei Hybrid-CPUs

#### E.4: XMP/DOCP im BIOS aktivieren
- RAM läuft mit JEDEC → XMP
- +10-15% Memory-Bandbreite

#### E.5: CUDA Unified Memory Hints
- KernelWorker.VramPage.is_hot → CUDA MemAdvise
- `cudaMemAdviseSetPreferredLocation` für Hot-Tensoren
- `cudaMemPrefetchAsync` für bevorzugte Layer

#### E.6: Speculative Decoding (INGRIED als Draft)
- INGRIED generiert Draft-Tokens
- Größerer Modell verifiziert in 1 Forward-Pass
- 2-3x Dekodier-Geschwindigkeit

#### E.7: Paged Weights für MoE
- Experten als Pages laden
- Nur aktivierte Experten in VRAM
- LRU-Eviction für nicht genutzte Experten

### Phase F: MoE-Expert-Optimierung (langfristig)
**Ziel:** Hot/Cold-Expert-Tracking, Expert-Residency
**Abhängigkeit:** Phase B, E

#### F.1: Expert-Residency-Tracking
- KernelWorker.record_access() für Hot/Cold-Classification
- Access-Counts über ExpertTracker-Events
- Top 20% → Hot, Rest → Cold

#### F.2: Hot-Experts in VRAM halten
- `KernelWorker.promote_to_vram()` für Hot-Experts
- `Engine.pin_tensor_in_vram()` für residente Tensoren

#### F.3: MoE-Hit-Rate > 70%
- Expert-Prefetch via `prefetch_for_experts()`
- ChunkLoader.cache_stats() für Hit-Rate messen

### Phase G: Spekulative Dekodierung (langfristig)
**Ziel:** 2-3x Speedup für Generation
**Abhängigkeit:** Phase E

#### G.1: Draft-Verifier-Architektur
- INGRIED (Draft) → 5 Kandidaten-Tokens
- 34B-Modell (Verifier) → 1 Forward-Pass
- Akzeptanz-Logik

---

## 📋 Reihenfolge

```
AKTUELL → Phase B (Startup) → Phase C (Chunk-Load) → Phase D (Disk) → Phase E (GPU)
         → Phase F (MoE) → Phase G (Speculative Decoding)
```

**Kurzfristig (Woche 1):** B + E (GPU-Optimierungen parallel)
**Mittelfristig (Woche 2):** C + D
**Langfristig (Monat 2):** F + G

---

## 🚦 Akzeptanzkriterien (nach allen Phasen)

```
□ Startup warm:     ≤ 3000ms   (vorher: 14,48s)
□ Startup cold:     ≤ 8000ms   (vorher: 14,48s)
□ Startup-Varianz:  ≤ 10%      (vorher: 34,6%)
□ Chunk-Warm:       ≤ 5ms      (vorher: 252ms)
□ Chunk-Cold:       ≤ 500ms    (vorher: 1137ms)
□ Warmup:           ≤ 5s       (vorher: 12,09s)
□ Disk-Ersparnis:   ≥ 10%      (vorher: 0.29%)
□ VRAM-Druck:       ≤ 85%      (vorher: 87,6%)
□ Inferenz-P95:     ≤ 4000ms   (bereits grün: 3,05s)
□ Alle 300 Tests:   grün       (bereits grün)
□ Memory-OC:        8000 MHz   (vorher: stock)
□ Speculative:      2x speedup (Ziel)
```

---

## ⚠️ Bullshit-Detektor

- 🚩 "50% VRAM savings" — gilt nur KV-Cache, nicht gesamtes Modell
- 🚩 "2x speedup" — nur mit Speculative Decoding, nicht ohne
- 🚩 "50% latency reduction" — Zielwert, nicht Ist-Zustand
- 🚩 Core-OC bringt viel — Memory-OC bringt 10x mehr

**Echt:**
- ✅ Q8_0 KV-Cache: messbarer Gewinn
- ✅ Memory-OC auf 8000 MHz: +15-25%
- ✅ CPU-Thread-Optimierung: +20-30%
- ✅ Speculative Decoding: mathematisch fundiert

---

## 🔧 Sofort-Nächste-Schritte

1. **Phase B.1:** Benchmark-Split-Skript erstellen
2. **Phase B.2:** Warmup-Strategie anpassen (nur 3 kritische Chunks)
3. **Phase B.4:** KernelWorker VRAM-Tracking mit realen Metriken
4. **Phase E.1:** Memory-OC auf 8000 MHz setzen
5. **Phase E.3:** Thread-Anzahl auf physische Cores reduzieren
6. **Benchmark:** Re-Startup-Messung nach jeder Änderung

**Starte sofort mit Phase B.** 🫡
