# Evidence – Phase 4: Ressourcen- und Triggeranalyse

Datum: 2026-09-14

## Implementierter Kern

`performance::TriggerController` ist eine deterministische Schutzmatrix mit:

- Eintritts- und Austrittsschwelle für VRAM-Hysterese
- Cooldown gegen wiederholte Eviction-Aktionen
- expliziter Entscheidung (`action`, `triggered`, `reason`)
- messbarer Zeitübergabe statt nicht reproduzierbarer Sleeps

Die vorhandene Transferquantifizierung umfasst SSD→RAM, RAM→Pinned, Pinned→VRAM
und Rückwege einschließlich Bytes, Queue-Zeit, Stall-Zeit, Bandbreite und
prognostizierter Transferdauer. SSD wird während Decode weiterhin nicht als
synchroner Tokenpfad empfohlen.

## Architekturgrenze

Die Triggerentscheidung ist im Control-Pfad deterministisch. Ein späterer
Analyst darf daraus nur eine Empfehlung ableiten. Analysejobs müssen außerhalb
des Decode-/Request-Locks laufen; bei Timeout oder Fehler gilt `Keep` bzw. die
jeweilige harte Schutzentscheidung.

## Verifikation

```text
cargo test performance::tests --lib       PASS (5)
cargo clippy --all-targets --all-features -- -D warnings  PASS
```

## Noch offen

- periodischer Hintergrund-Sampler mit Queue und Abbruchsignal
- reale nvidia-smi-/RAM-/Filesystem-Snapshots in diesen Controller einspeisen
- Promotion nach zwei Beobachtungsfenstern und automatisches Regression-
  Rollback anhand echter Modellläufe
