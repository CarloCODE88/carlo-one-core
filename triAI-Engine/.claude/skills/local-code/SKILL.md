---
name: local-code
description: Delegiert eine eng gefasste Rust-Implementierungsaufgabe (ein Modul oder eine Funktion) an den lokalen Coder qwen2.5-coder:14b (Ollama) für das tri-ai-runner-Projekt. Liefert einen Codevorschlag, der zwingend von Claude geprüft, kompiliert und getestet wird, bevor er übernommen gilt. NICHT für offene Architektur-/Rechercheentscheidungen — dafür /local-research.
user-invocable: true
---

# /local-code — qwen2.5-coder:14b für eine Implementierungsaufgabe nutzen

Voraussetzung: siehe [[tri-ai-runner]]-Skill für die projektweiten
Betriebsregeln (serielle Single-Model-Nutzung). Vor diesem Aufruf
sicherstellen, dass kein anderes lokales Modell gerade Inferenz durchführt
(insbesondere kein laufender `/local-research`-Aufruf) und dass kein echter
Worker-Prozess des Runners selbst aktiv ist.

## Grenzen für qwen2.5-coder in diesem Projekt

- genau ein Rust-Modul/eine Funktion pro Auftrag, nicht mehrere Dateien
  gleichzeitig anfassen lassen;
- keine Änderungen an Ollama, Franz Studio oder anderen Originalquellen
  vorschlagen lassen;
- keine GPU-/SSD-Mutationen im generierten Testcode (Stub-Prozesse statt
  echter Modelle/Worker, siehe Muster in `src/http.rs`-Tests);
- Vorschlag ist ein **Entwurf**, niemals ungeprüft übernehmen.

## Ablauf

1. Ollama-Dienst prüfen, bei Bedarf starten:
   ```bash
   systemctl is-active ollama.service || sudo systemctl start ollama.service
   ```
2. Vor dem Prompt: die betroffene(n) bestehende(n) Datei(en) selbst lesen.
   Den Prompt so eng wie möglich fassen — exakte Typen/Signaturen aus dem
   echten Code einfügen, nicht aus dem Gedächtnis paraphrasieren. Eine
   einzelne, klar umrissene Aufgabe pro Aufruf.
3. Aufruf:
   ```bash
   curl -s http://127.0.0.1:11434/api/generate -H "Content-Type: application/json" -d '{
     "model": "qwen2.5-coder:14b",
     "prompt": "<enger Implementierungsauftrag inkl. exaktem bestehenden Code>",
     "stream": false,
     "think": false,
     "options": {"temperature": 0.1, "num_predict": 1200}
   }'
   ```
4. **Verifikationspflicht, kein Ausnahmefall:**
   - Antwort gegen die tatsächlichen Typen/Signaturen im Projekt prüfen
     (erfundene Methoden, falsche `Default`-Annahmen, falsche Ownership/
     Borrow-Nutzung sind beim bisherigen Einsatz in diesem Projekt bereits
     vorgekommen).
   - Übernahme manuell per Edit, nicht blind einfügen.
   - Danach immer: `cargo fmt --all && cargo test --offline`.
   - Bei Kompilier-/Testfehler: den *exakten* Fehlertext plus die *exakt*
     betroffenen Zeilen in einem zweiten, noch enger gefassten Prompt an
     dasselbe Modell zurückgeben (siehe Beispiel unten). Nach maximal zwei
     erfolglosen gezielten Versuchen übernimmt Claude die Korrektur selbst,
     statt weiter zu raten.
   - Beispiel für einen präzisen zweiten Versuch (funktioniert erfahrungsgemäß
     deutlich besser als eine allgemeine "beheb den Fehler"-Bitte):
     ```
     Dein letzter Vorschlag ist falsch: <konkrete Ursache in einem Satz>.
     Der korrekte Fix: <konkrete Korrekturidee in einem Satz>.
     Gib NUR den korrigierten <Funktions-/Codeblock-Name> zurück, sonst nichts.
     ```
5. Änderungsliste und Testergebnis dem Nutzer berichten, inklusive: was vom
   lokalen Modell kam, was Claude korrigieren musste.
