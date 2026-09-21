# Evidence – Phase 3: Stabiler Runtime-Pfad

Datum: 2026-09-14

## Zustandsvertrag

`EngineState` serialisiert nun explizit:

```text
idle, loading, ready, busy, degraded, draining, rollback, stopping, failed
```

Ein Worker-Ausfall führt zu `degraded`, kontrolliertes Beenden beginnt in
`draining`, und ein explizit angeforderter Fallback-Recovery-Versuch läuft in
`rollback`. Nur ein erfolgreicher Readiness-Check kann wieder zu `ready`
führen. Ein fehlgeschlagener Recovery-Versuch bleibt `failed`.

## Sicherheitsgrenze

`recover_with_fallback` nimmt ausschließlich eine vom Control-Pfad übergebene
Worker-Konfiguration entgegen. Es gibt keinen impliziten Cloud-Fallback und
keine Möglichkeit für Learning-/Advice-Daten, Authentisierung, Pfadregeln,
Budgets oder Capability-Prüfungen zu überschreiben.

## Verifikation

```text
cargo test engine::tests --lib     PASS (5)
cargo test api::tests --lib        PASS (4)
cargo clippy --all-targets --all-features -- -D warnings  PASS
```

Der privilegierte vollständige Lauf ist mit 220/220 Tests grün. Nur ein
unprivilegierter Sandbox-Lauf hat die bekannte lokale Socketbeschränkung.

## Noch offen

- HTTP-Endpunkt und Policy-Manifest für die Auswahl eines erlaubten Fallbacks
- echter erfolgreicher Crash-/Fallback-Integrationstest mit lokalem Worker
- persistenter Rollback-Checkpoint und automatische Regressionserkennung
- Katalogscan/Hashing darf den Listener- und Health-Start nicht blockieren;
  die aktuelle Implementierung scannt vor dem Accept-Loop und braucht bei den
  lokalen Multi-GiB-Modellen entsprechend lange.
