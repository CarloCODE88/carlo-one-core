//! Assistant-Registry: wiederverwendbare, nutzerdefinierte Rollen.
//!
//! Jan-app bietet „Custom Assistants" (nutzerdefinierte Rollen/System-Prompts,
//! die über Sessions hinweg wiederverwendbar sind). Dieser Strang schließt
//! dieselbe Lücke für die lokale Lösung: Ein Assistent ist eine benannte
//! Konfiguration aus System-Prompt, Modell, Temperatur und aktivierten Tools.
//!
//! Die Konfiguration lebt in `<project_root>/assistants/<name>.json` (eine
//! Datei je Rolle), damit sie von der GUI und dem CLI gleichermaßen geteilt
//! wird. Der Loader ist tolerant: Fehlende Dateien füllen eine leere Liste,
//! ungültige Einträge werden übersprungen statt hart abzubrechen — so kann ein
//! defekter Assistent den Start nicht blockieren.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, path::Path};

/// Eine wiederverwendbare, nutzerdefinierte Rolle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Assistant {
    /// Kürzel/Zustandsname (Dateiname ohne Endung).
    pub name: String,
    /// System-Prompt der Rolle.
    pub system_prompt: String,
    /// Optional: Modell, das diese Rolle bevorzugt lädt.
    pub model: Option<String>,
    /// Sampling-Temperatur der Rolle.
    pub temperature: f32,
    /// Namen der aktivierten Tools (Freigabe zusätzlich gegen die
    /// `attachments::is_allowed_tool_name`-Whitelist erzwungen).
    pub tools: Vec<String>,
}

impl Default for Assistant {
    fn default() -> Self {
        Self {
            name: String::new(),
            system_prompt: String::new(),
            model: None,
            temperature: 0.7,
            tools: Vec::new(),
        }
    }
}

/// Fehler beim Laden der Assistant-Konfigurationen.
#[derive(Debug)]
pub struct AssistantError(pub String);

impl std::fmt::Display for AssistantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AssistantError {}

/// Liest alle Assistenten unterhalb des Verzeichnisses `root` ein. Leere
/// Verzeichnisliste ist ein leeres Ergebnis, kein Fehler.
pub fn list(root: &Path) -> Result<Vec<Assistant>, AssistantError> {
    let mut out = Vec::new();
    match fs::read_dir(root) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                    match load(root, name) {
                        Ok(assistant) => out.push(assistant),
                        Err(_) => continue, // defekte Rolle überspringen
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(AssistantError(format!(
                "Assistenten-Verzeichnis nicht lesbar: {e}"
            )))
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Lädt einen einzelnen Assistenten per Name (Dateiname ohne `.json`).
/// Fehlende Datei oder defektes JSON ist ein Fehler.
pub fn load(root: &Path, name: &str) -> Result<Assistant, AssistantError> {
    if !is_valid_assistant_name(name) {
        return Err(AssistantError(format!(
            "Ungültiger Assistenten-Name '{name}'"
        )));
    }
    let path = root.join(format!("{name}.json"));
    let text = fs::read_to_string(&path)
        .map_err(|e| AssistantError(format!("Assistent '{}' nicht lesbar: {e}", name)))?;
    let mut assistant: Assistant = serde_json::from_str(&text)
        .map_err(|e| AssistantError(format!("Assistent '{}' ungültig: {e}", name)))?;
    if assistant.name.is_empty() {
        assistant.name = name.to_string();
    }
    Ok(assistant)
}

/// Gibt einen geladenen Assistenten zurück (Kurzform zum Wiederverwenden).
pub fn get<'a>(assistants: &'a [Assistant], name: &str) -> Option<&'a Assistant> {
    assistants.iter().find(|a| a.name == name)
}

/// Wendet die Konfiguration eines Assistenten auf einen Chat-Request an:
/// System-Prompt, Temperatur und (sobald Tool-Routing existiert) Model/Tools.
pub fn apply(assistant: &Assistant, body: &mut Value) {
    let object = body.as_object_mut();
    let Some(object) = object else { return };
    if !assistant.system_prompt.is_empty() {
        // Entferne einen eventuell vorhandenen System-Prompt und setze den der
        // Rolle; behält alle übrigen Nachrichten.
        if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
            messages.retain(|m| m.get("role").and_then(Value::as_str) != Some("system"));
            messages.insert(
                0,
                serde_json::json!({"role": "system", "content": assistant.system_prompt}),
            );
        }
    }
    object.insert(
        "temperature".to_string(),
        serde_json::json!(assistant.temperature),
    );
    if let Some(model) = &assistant.model {
        object.insert("model".to_string(), serde_json::json!(model));
    }
}

/// Ein Assistenten-Name ist ein Datei-Stem: nicht leer, ≤ 64 Zeichen, nur
/// ASCII-Alphanumerik plus `-`/`_` (kein Pfadzugriff).
pub fn is_valid_assistant_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Zentrale Sicherheits-/Zustands-Prüfung eines Assistenten vor Einsatz
/// (Rollenvalidierung). Garantiert, dass ein geladener Name nicht nur ein
/// gültiger Datei-Stem ist, sondern auch kein Pfad-Marker, keine übermäßig
/// lange Bezeichnung und dass die Temperatur im erlaubten Bereich liegt.
/// Liefert bei Erfolg `Ok(())`, sonst eine beschreibbare Fehlerquelle.
pub fn assistant_validate(name: &str, temperature: f32) -> Result<(), String> {
    if !is_valid_assistant_name(name) {
        return Err(format!(
            "Ungültiger Assistenten-Name '{name}' (nur alphanumerisch, '-', '_', ≤ 64 Zeichen)"
        ));
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(format!(
            "Assistenten-Name '{name}' enthält einen Pfadmarker"
        ));
    }
    if !(0.0..=2.0).contains(&temperature) || temperature.is_nan() {
        return Err(format!(
            "Temperatur {} liegt außerhalb des erlaubten Bereichs [0.0, 2.0]",
            temperature
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "assist-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = dir.join("assistants");
        fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    #[test]
    fn loads_multiple_assistants_sorted() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (dir, root) = temp_dir("multi");
        fs::write(
            root.join("alpha.json"),
            r#"{"name":"alpha","system_prompt":"Du bist Alpha","temperature":0.2}"#,
        )
        .unwrap();
        fs::write(
            root.join("beta.json"),
            r#"{"name":"beta","system_prompt":"Du bist Beta","model":"coder.gguf"}"#,
        )
        .unwrap();
        let items = list(&root).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "alpha");
        assert_eq!(items[1].name, "beta");
        assert_eq!(items[1].temperature, 0.7); // Default, nicht gesetzt
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_is_empty_not_error() {
        let (dir, root) = temp_dir("empty");
        fs::remove_dir_all(&root).unwrap();
        assert!(list(&root).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_assistant_is_skipped() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (dir, root) = temp_dir("corrupt");
        fs::write(root.join("good.json"), r#"{"system_prompt":"ok"}"#).unwrap();
        fs::write(root.join("bad.json"), r#"{nicht json"#).unwrap();
        let items = list(&root).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "good");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn name_validation_blocks_paths() {
        assert!(is_valid_assistant_name("coder"));
        assert!(is_valid_assistant_name("research-1"));
        assert!(!is_valid_assistant_name(""));
        assert!(!is_valid_assistant_name("a/b"));
        assert!(!is_valid_assistant_name(".."));
        assert!(!is_valid_assistant_name(&"x".repeat(65)));
    }

    #[test]
    fn assistant_validate_rejects_paths_and_out_of_range_temp() {
        assert!(assistant_validate("coder", 0.7).is_ok());
        assert!(assistant_validate("research-1", 1.5).is_ok());
        // Pfadmarker / ungültige Zeichen.
        assert!(assistant_validate("../x", 0.7).is_err());
        assert!(assistant_validate("a/b", 0.7).is_err());
        assert!(assistant_validate("a\\b", 0.7).is_err());
        assert!(assistant_validate("", 0.7).is_err());
        // Temperatur außerhalb des Bereichs bzw. NaN.
        assert!(assistant_validate("coder", -0.1).is_err());
        assert!(assistant_validate("coder", 2.5).is_err());
        assert!(assistant_validate("coder", f32::NAN).is_err());
    }

    #[test]
    fn apply_sets_system_prompt_and_temperature() {
        let asst = Assistant {
            name: "t".into(),
            system_prompt: "Strikt kurz antworten".into(),
            temperature: 0.4,
            ..Default::default()
        };
        let mut body =
            serde_json::json!({ "model": "m", "messages": [{"role":"user","content":"hi"}] });
        apply(&asst, &mut body);
        assert_eq!(body["messages"][0]["content"], "Strikt kurz antworten");
        assert_eq!(body["messages"][0]["role"], "system");
        // serde_json serialisiert das f32 0.4 als 0.4000000059604645 (f64) —
        // vergleiche deshalb tolerant statt exakt.
        let temp = body["temperature"].as_f64().unwrap_or_default();
        assert!((temp - 0.4).abs() < 1e-6, "Temperatur {temp} != 0.4");
        assert_eq!(body["model"], "m"); // kein Modell im Assistenten -> unverändert
    }
}
