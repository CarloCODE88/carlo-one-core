# TRI Routing Database Skill

## Zweck

Dieses interne Skill beschreibt die Nutzung der Knowledge-Database für
Modellwahl, Speicherresidenz, Prefetch und Transferentscheidungen.

## Entscheidungsreihenfolge

1. Sicherheitsgrenzen und Live-Trigger anwenden.
2. Prompt nur in Flags umwandeln: Wissensbereich, Tasktyp, Tools,
   Kontextqualität und Kontextgröße.
3. Modellprofil über ID und unveränderlichen Digest laden.
4. Runtime-Messungen, Modellprior, lokale Evidenz und danach Mini-Modell-
   Beratung fusionieren.
5. MoE-Experten über Router-Trace planen; dichte Modelle nur über Layer-
   Residency und Next-Layer-Prefetch behandeln.
6. SSD-Reads während des Decodings verbieten.
7. Nur aggregierte, geheime-freie Evidence speichern.

## Lernregeln

- Rohprompts und Secrets werden nie gespeichert.
- Seed-Priors bleiben bis zur Mindestanzahl von Evidenzläufen erhalten.
- Neue Policies werden zuerst als Vorschlag mit Rollback geführt.
- Modellübergreifende Muster brauchen Evidenz aus mindestens zwei Modellen.
- Qualitäts- oder Latenzregressionen verwerfen die neue Policy.

## Mini-Modell

Das Mini-Modell darf asynchron Cluster, Anomalien, Transferengpässe und
Modell-Fit auswerten. Es darf niemals VRAM-Sicherheitsreserve, KV-Schutz,
SSD-Decode-Verbot oder Pfad-/Tool-Sicherheitsregeln überstimmen. Bei Timeout
oder Konflikt gilt die deterministische Runtime-Policy.
