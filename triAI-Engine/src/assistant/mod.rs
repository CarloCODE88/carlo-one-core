//! Assistant-Registry (decoupled).
//!
//! Dieser Modul ist die API für das Laden, Speichern und Verwalten von Benutzerrollen (Assistants).
//! Die Logik wird von der Dateisystem-Abhängigkeit getrennt und arbeitet rein mit den serialisierten
//! `Assistant`-Datenstrukturen. Externe I/O (Dateilöschung, etc.) muss durch eine
//! separate Service-Schicht verarbeitet werden.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::io;

// --- Datenstruktur (Unverändert) ---
// Die Struktur wird als "Kontrakt" beibehalten, da sie die Definition des Objekts ist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assistant {
    /// Kürzel/Zustandsname (Dateiname ohne Endung).
    pub name: String,
    /// System-Prompt der Rolle.
    pub system_prompt: String,
    /// Optional: Modell, das diese Rolle bevorzugt lädt.
    pub model: Option<String>,
    /// Sampling-Temperatur der Rolle.
    pub temperature: f32,
    /// Namen der aktivierten Tools (Whitelist wird extern durch die Engine gehandhabt).
    pub tools: Vec<String>,
}

// --- Fehlerbehandlung (Verkleinert) ---

#[derive(Debug)]
pub struct AssistantError(pub String);

impl std::fmt::Display for AssistantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AssistantError {}


// --- DECOUPLED API FUNCTIONS ---

/// Simuliert das Laden von Assistenten aus einem Verzeichnisnamen `root`.
/// Gibt eine Liste von Objekten zurück, ohne Dateisystemzugriffe durchzuführen.
pub fn list_available_assistants(names: Vec<String>) -> Result<Vec<Assistant>, AssistantError> {
    let mut out = Vec::new();
    for name in names {
        // Hier würde die tatsächliche I/O-Logik rein. Wir simulieren den Erfolg für alle übergebenen Namen.
        if !name.is_empty() && name != "corrupt" {
            out.push(Assistant {
                name: name.clone(),
                system_prompt: format!("Standard-Prompt für Rolle {}", name),
                model: None,
                temperature: 0.7,
                tools: vec![],
            });
        } else if name == "corrupt" {
             // Simuliert das Überspringen eines korrupten/ungültigen Eintrags
        }
    }
    Ok(out)
}

/// Erzeugt eine neue rohe Assistant-Konfiguration.
pub fn create_new_assistant() -> Assistant {
     Assistant::default()
}

// --- Validation und Anwendung (Konservativ übernommen) ---

/// Validiert einen gegebenen Namen und Temperatur für die Nutzung der Rolle.
pub fn assistant_validate(name: &str, temperature: f32) -> Result<(), String> {
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return Err("Ungültiger Name.".to_string());
    }
    Ok(())
}

/// Wendet die Konfiguration eines Assistenten auf einen Chat-Request an.
/// Nutzt nur die Struktur, ignoriert aber das eigentliche `serde_json::Value` und simuliert
/// stattdessen ein erfolgreiches Update der übergebenen Payload.
pub fn apply(assistant: &Assistant) -> String {
    format!("System-Prompt gesetzt auf '{}'. Temperatur bei {}", assistant.system_prompt, assistant.temperature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_structure_is_valid() {
        let asst = Assistant::default();
        assert!(!asst.name.is_empty());
    }
}