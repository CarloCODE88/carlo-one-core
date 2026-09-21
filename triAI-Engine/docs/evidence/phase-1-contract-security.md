# Evidence – Phase 1: Verträge und Sicherheitsgrundlage

Datum: 2026-09-14

## Änderungen

- `server.auth_token` als optionale Konfiguration ergänzt.
- Externe Bind-Adressen ohne nichtleeren Token werden abgelehnt.
- `Authorization: Bearer <token>` und `X-TRI-Auth` unterstützt.
- Tokenvergleich erfolgt byteweise in konstanter Schleifenlänge.
- Request-ID wird aus `X-Request-ID` übernommen, wenn sie validiert ist;
  andernfalls lokal erzeugt.
- `request_received`-Event enthält nur Request-ID, Methode und Pfad.
- Request-Body auf 16 MiB begrenzt.
- Ungültige `Content-Length`-Werte werden abgelehnt.
- Aktive Verbindungen auf 64 begrenzt; neue Verbindungen werden bei voller
  Kapazität verworfen.
- `/metrics` liefert ein versioniertes, geheime-freies JSON-Metrics-Schema und
  bleibt bei gesetztem Token authentifizierungspflichtig.
- `/health` bleibt bewusst öffentlich, damit Prozess-/Readiness-Probes nicht
  von Anwendungsauthentisierung abhängen.
- Evidence-Zeilen tragen `schema_version` und eine monotone `event_id`; die
  technische Redaction läuft vor der JSONL-Ausgabe.

## Verifikation

```text
cargo fmt --all -- --check                         PASS
cargo test config::tests                         6 passed
cargo test http::tests::bearer_and_tri_auth...   1 passed
cargo test http::tests::invalid_content_length... 1 passed
cargo clippy --all-targets --all-features -- -D warnings PASS
```

Vollständiger privilegierter Library-Lauf am 2026-09-14: 220 von 220 Tests
bestanden. Ein unprivilegierter Sandbox-Lauf scheitert bei sieben
Download-Integrationstests an `PermissionDenied` beim lokalen Test-Socket in
`src/download.rs:878`; dies ist eine Testumgebungsgrenze, kein Assertion- oder
Compile-Fehler. Formatierung und Clippy waren ebenfalls erfolgreich.

## Bewusste Grenzen

- Loopback ohne Token bleibt für lokale Entwicklung kompatibel.
- `/health` bleibt für lokale Process-/Readiness-Probes öffentlich; alle
  anderen geschützten Routen benötigen den konfigurierten Token.
- Cancellation des laufenden Worker-Streams ist noch nicht implementiert;
  aktuell greifen Read-/Inference-Timeouts.
- Correlation-ID ist zunächst im Empfangsevent verankert; die vollständige
  Weitergabe an jede Downstream-Evidence folgt in Phase 2.
