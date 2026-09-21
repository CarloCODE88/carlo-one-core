# Evidence – Phase 2: Evidence und Observability

Datum: 2026-09-14

## Implementierter Vertrag

- Technische JSONL-Events sind Schema-Version 1 und besitzen eine monotone
  Prozess-Event-ID.
- Felder werden rekursiv vor dem Schreiben redigiert; Schlüssel mit
  `api_key`, `pin`, `token`, `password` oder `secret` erhalten keinen
  Originalwert.
- Der HTTP-Eingang schreibt eine begrenzte `request_received`-Evidence mit
  Request-ID, Methode und Pfad. Rohprompt und Rohantwort sind darin nicht
  enthalten.
- Der optionale Learning-Store schreibt nur append-only JSONL und ist
  standardmäßig deaktiviert.
- Learning-Records benötigen Schema-Version, begrenzte Run-ID, erfolgreichen
  Secret-Scan und dürfen keine Rohprompt-, Rohcode- oder Rohantwort-Flags
  setzen. Parallele Schreiber werden serialisiert.

## Zweigeteilte Engine

Der Evidence-/Learning-Pfad ist absichtlich nicht erforderlich, damit der
Control-/Execution-Pfad einen Request annimmt oder ein Modell ausführt. Ein
Schreibfehler, ein zu großer Datensatz oder ein Privacy-Verstoß führt nur zum
Verwerfen dieses Analyse-Records. Kein Analyse-Record kann Auth, Capability,
Budget, Pfadregeln oder Supervisor-Zustände verändern.

## Verifikation

```text
cargo test observability::tests --lib                         PASS (3)
cargo test learning::tests --lib                              PASS (3)
cargo test http::tests::health_is_public... --lib             PASS (1)
cargo clippy --all-targets --all-features -- -D warnings      PASS
cargo fmt --all -- --check                                    PASS
```

Die JSONL-Zeilen sind mit `jq` auswertbar, zum Beispiel:

```bash
jq -c 'select(.event == "request_received") | {id:.event_id, request:.fields.request_id, path:.fields.path}' tri-ai-events.jsonl
```

## Noch offen

- vollständige Run-ID-Weitergabe bis in jede Downstream-Operation
- HMAC-/Schlüsselverwaltung für Prompt-Fingerprints statt bloßer Feldstruktur
- konfigurierbare Rotation und Export ohne Überschreiben alter Evidence
- echte asynchrone Writer-Queue für sehr hohe Eventraten
