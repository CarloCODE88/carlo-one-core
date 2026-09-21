# Evidence – Phase 5: Storage- und Chunk-Optimierung

Datum: 2026-09-14

## Implementierter, sicherer Umfang

`storage::plan_chunks` erzeugt einen versionierten und serialisierbaren
Dry-Run-Plan mit vollständigen, nicht überlappenden Bytebereichen. Der Plan
unterstützt die Modi `OriginalGguf` und `ZstdPerChunk`, führt aber selbst keine
Kompression, Partitionierung, Mount- oder Rewrite-Operation aus.

Das ist absichtlich konservativ: Ein GGUF darf nicht mitten in Tensordaten
geteilt werden. Vor einer echten Promotion braucht es einen GGUF-Tensorindex,
pro Chunk einen verifizierten Digest, einen Recovery-Test und einen Vergleich
gegen das unveränderte Original.

## Verifikation

```text
cargo test storage::tests --lib       PASS (2)
cargo clippy --all-targets --all-features -- -D warnings  PASS
cargo fmt --all -- --check            PASS
```

## Nicht aktiviert

- keine echte Zstd-Kompression
- kein mmap-/Paged-Backend auf Chunkbasis
- keine SSD-Partitionierung oder Mount-Änderung
- keine automatische Promotion eines komprimierten Artefakts

Diese Punkte bleiben bis zu einem Benchmark mit Kaltstart, Warmstart, TTFT,
Decode, RAM/VRAM, Recovery und Qualitätsvergleich absichtlich deaktiviert.
