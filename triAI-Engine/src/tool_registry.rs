//! Registrierung der verfügbaren Coding-Werkzeuge für den Chat-Tool-Loop.
//!
//! `ToolRegistry` hält dieselbe Tool-Liste wie `coding_tools`, aber als
//! strukturierte Konstanten (`name`, `description`, `parameters`, `required`)
//! statt als rohe JSON-Blöcke. Die eigentliche Ausführungslogik wird bewusst
//! nicht dupliziert: `dispatch` leitet jeden Aufruf an
//! `coding_tools::execute` weiter. Damit bleiben Definition und Ausführung
//! immer synchron.
//!
//! Sicherheitsgrenze: Jeder Tool-Name wird vor dem Dispatch über
//! `crate::attachments::is_allowed_tool_name` geprüft, sodass ausschließlich
//! das freigegebene Coding-Set ausführbar ist.

use crate::attachments::is_allowed_tool_name;
use serde_json::{Map, Value};
use std::path::Path;

/// Platzhalter für eine künftige MCP-Tool-Spezifikation. Noch nicht
/// implementiert — nur der Typ bzw. das Feld existiert, damit der
/// `ToolRegistry`-Datentyp von vornherein einen Hook für MCP-Anbindung hat.
pub struct McpToolSpec {
    /// Reserviert für spätere MCP-Erweiterungen (nicht implementiert).
    _private: (),
}

/// Ein registriertes Tool: Name, Beschreibung, JSON-Schema-Parameter und die
/// Liste der erforderlichen Argumente.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Eindeutiger Tool-Name (matcht `coding_tools::execute`).
    pub name: String,
    /// Menschlich lesbare Beschreibung, identisch zu `coding_tools`.
    pub description: String,
    /// JSON-Schema-Objekt der `parameters` (ohne `type`/`required`-Rahmen).
    pub parameters: Value,
    /// Namen der Pflichtargumente aus dem Parameterschema.
    pub required: Vec<String>,
}

impl ToolSpec {
    /// Konstruiert ein `ToolSpec` aus einer rohen OpenAI-Tool-Definition
    /// (`{"type":"function","function":{...}}`) und extrahiert Name,
    /// Beschreibung, Parameterschema und Pflichtargumente. Liefert `None`,
    /// wenn die Struktur nicht dem erwarteten Muster entspricht.
    fn from_tool_definition(value: &Value) -> Option<ToolSpec> {
        let function = value.get("function")?.as_object()?;
        let name = function.get("name")?.as_str()?.to_owned();
        let description = function.get("description")?.as_str()?.to_owned();
        let parameters = function.get("parameters")?.clone();
        let required = parameters
            .get("required")
            .and_then(Value::as_array)
            .map(|reqs| {
                reqs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        Some(ToolSpec {
            name,
            description,
            parameters,
            required,
        })
    }
}

/// Registrierung aller freigegebenen Coding-Werkzeuge.
pub struct ToolRegistry {
    /// Strukturierte Definitionen der verfügbaren Tools (feste Liste von
    /// `coding_tools::tool_definitions()`).
    tools: Vec<ToolSpec>,
    /// Platzhalter für MCP-Tools — derzeit immer `None`, nicht implementiert.
    #[allow(dead_code)]
    mcp: Option<McpToolSpec>,
}

impl ToolRegistry {
    /// Erstellt das Registry mit exakt denselben Tools wie
    /// `coding_tools::tool_definitions()` (gleiche Namen und Beschreibungen),
    /// damit bestehende Tests und die GUI-Erwartungen unverändert bleiben.
    pub fn new() -> Self {
        let tools = crate::coding_tools::tool_definitions()
            .iter()
            .filter_map(ToolSpec::from_tool_definition)
            .collect();
        Self { tools, mcp: None }
    }

    /// Gibt alle registrierten Tool-Definitionen als rohe OpenAI-Liste zurück
    /// (gleiches Format wie `coding_tools::tool_definitions()`).
    pub fn tool_definitions(&self) -> Vec<Value> {
        crate::coding_tools::tool_definitions()
    }

    /// Liefert die strukturierte Tool-Liste (Namen, Beschreibungen, Parameter).
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// Führt ein Tool über `coding_tools::execute` aus. Der Name wird zuvor
    /// über die Attachment-Whitelist validiert; unbekannte Namen führen zu
    /// einem Fehler.
    pub fn dispatch(
        &self,
        project_root: &Path,
        name: &str,
        args: &Map<String, Value>,
    ) -> Result<Value, String> {
        if !is_allowed_tool_name(name) {
            return Err(format!("Tool '{name}' ist nicht freigegeben"));
        }
        crate::coding_tools::execute(project_root, name, args)
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tool-registry-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Die Namen und Beschreibungen des Registry müssen exakt mit denen aus
    /// `coding_tools::tool_definitions()` übereinstimmen — nur so bleiben die
    /// bestehenden Tests und die Tool-Auswahl der GUI konsistent.
    #[test]
    fn tool_names_match_coding_tools() {
        let registry = ToolRegistry::new();
        let raw = crate::coding_tools::tool_definitions();
        let mut raw_names = raw
            .iter()
            .filter_map(|d| d.pointer("/function/name").and_then(Value::as_str))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        raw_names.sort();

        let mut registry_names = registry
            .tools()
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<_>>();
        registry_names.sort();

        assert_eq!(registry_names, raw_names);
        assert!(!registry_names.is_empty());
    }

    /// `dispatch` leitet `read_file` und `write_file` an `coding_tools`
    /// weiter: eine Schreibaktion erzeugt die Datei, eine Leseaktion liefert
    /// den gespeicherten Inhalt zurück.
    #[test]
    fn dispatch_forwards_read_and_write_to_temp_file() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_root("dispatch");
        let registry = ToolRegistry::new();

        let write_args = json!({"path": "a/b/note.txt", "content": "registry content"})
            .as_object()
            .unwrap()
            .clone();
        let write = registry
            .dispatch(&root, "write_file", &write_args)
            .expect("write_file sollte weitergeleitet werden");
        assert_eq!(write["bytes_written"], "registry content".len());

        let read_args = json!({"path": "a/b/note.txt"}).as_object().unwrap().clone();
        let read = registry
            .dispatch(&root, "read_file", &read_args)
            .expect("read_file sollte weitergeleitet werden");
        assert_eq!(read["content"], "registry content");

        let _ = fs::remove_dir_all(&root);
    }

    /// Ein unbekannter Tool-Name wird abgelehnt, bevor die Ausführungslogik
    /// überhaupt berührt wird.
    #[test]
    fn unknown_tool_name_is_rejected() {
        let registry = ToolRegistry::new();
        let root = temp_root("unknown");
        let args = json!({}).as_object().unwrap().clone();
        let err = registry
            .dispatch(&root, "does_not_exist", &args)
            .expect_err("unbekannter Name muss abgelehnt werden");
        assert!(err.contains("does_not_exist"));
        assert!(err.contains("nicht freigegeben"));
        let _ = fs::remove_dir_all(&root);
    }
}
