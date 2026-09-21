---
name: local-research
description: Delegiert eine Recherche-, Architektur- oder Code-Auswertungsfrage an INGRIED (lokales Ollama-Modell goekdenizguelmez/JOSIEFIED-Qwen3:8b) für das tri-ai-runner-Projekt. Nutzen für Ollama/llama.cpp/LM-Studio/Jan-Verhaltensvergleiche, API-/Lifecycle-Vertragsfragen, Speicher-/Worker-Risikoeinschätzungen, Planbegründungen, Testfallvorschläge oder offene Forschungsfragen. NICHT für Rust-Code-Erzeugung — dafür /local-code.
user-invocable: true
---

# /local-research — INGRIED befragen

Voraussetzung: siehe [[tri-ai-runner]]-Skill für die projektweiten
Betriebsregeln (serielle Single-Model-Nutzung). Vor diesem Aufruf sicherstellen,
dass kein anderes lokales Modell gerade Inferenz durchführt (insbesondere kein
laufender `/local-code`-Aufruf und kein aktiver Worker-Prozess des Runners
selbst) — sonst erst beenden/entladen.

## Ablauf

1. Ollama-Dienst prüfen, bei Bedarf starten:
   ```bash
   systemctl is-active ollama.service || sudo systemctl start ollama.service
   ```
2. Die vom Nutzer übergebene Aufgabenbeschreibung (Argument dieses Befehls)
   zu einem **eng gefassten** Forschungsauftrag zuspitzen — keine
   Mehrkriterien-Sammelfrage in einem Durchlauf, lieber mehrmals eng fragen
   als einmal breit. Bei Bedarf zuerst relevanten Code/Doku-Ausschnitt selbst
   lesen (z.B. aus `outputs/`, `work/reference/`, `src/`) und als Kontext in
   den Prompt legen statt INGRIED raten zu lassen.
3. Aufruf:
   ```bash
   curl -s http://127.0.0.1:11434/api/generate -H "Content-Type: application/json" -d '{
     "model": "goekdenizguelmez/JOSIEFIED-Qwen3:8b",
     "prompt": "<zugespitzter Forschungsauftrag inkl. nötigem Kontext>",
     "stream": false,
     "think": false,
     "options": {"temperature": 0.3, "num_predict": 1500}
   }'
   ```
   `think: false` immer setzen (dieses Modell ist thinking-fähig und würde
   sonst ggf. das Token-Budget im internen Reasoning verbrauchen, ohne
   sichtbare Antwort zu liefern). Falls die Antwort trotzdem einen
   `<think>...</think>`-Block enthält: defensiv alles bis `</think>`
   abschneiden, bevor die Antwort weiterverwendet wird.
4. INGRIEDs Antwort ist **Grundlage, kein fertiges Ergebnis** — INGRIED darf
   laut Rollenteilung keine Dateien löschen, keine Modellablage verändern und
   keine produktiven Prozesse starten. Claude prüft die Aussagen gegen den
   tatsächlichen Code/Dateisystem, bevor sie als Fakt weitergegeben oder in
   eine Entscheidung übernommen werden — insbesondere Datei-/Zeilenangaben
   und Behauptungen über Modellverhalten.
5. Ergebnis dem Nutzer zusammengefasst zurückgeben, mit klarer Kennzeichnung,
   was INGRIED beigetragen hat und was Claude selbst verifiziert hat.
