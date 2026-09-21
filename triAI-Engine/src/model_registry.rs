//! Liest die statischen Modell-Metadaten aus `models/roles.json` und
//! `models/ollama-inventory.json` und bietet einen einfachen Lookup an.
//! Reine Leseoperation, kein Netzwerk, kein Prozessstart.

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RolesFile {
    roles: HashMap<String, RoleEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RoleEntry {
    alias: Option<String>,
    model: String,
    purpose: String,
    #[serde(default)]
    constraints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InventoryFile {
    source: String,
    base_url: String,
    models: Vec<InventoryModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InventoryModel {
    name: String,
    alias: Option<String>,
    digest: String,
    size_bytes: u64,
    family: String,
    parameters: String,
    quantization: String,
    context_tokens: u32,
    capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    pub name: String,
    pub alias: Option<String>,
    pub context_tokens: u32,
    pub size_bytes: u64,
}

pub struct ModelRegistry {
    roles: RolesFile,
    inventory: InventoryFile,
}

impl ModelRegistry {
    pub fn load(project_root: &Path) -> io::Result<Self> {
        let roles_path = project_root.join("models/roles.json");
        let inventory_path = project_root.join("models/ollama-inventory.json");

        let roles = read_json::<RolesFile>(&roles_path)?;
        let inventory = read_json::<InventoryFile>(&inventory_path)?;

        Ok(ModelRegistry { roles, inventory })
    }

    /// Sucht ein Modell im Ollama-Inventar über den echten Namen oder
    /// dessen `alias`.
    pub fn resolve(&self, model_id: &str) -> Option<ResolvedModel> {
        self.inventory
            .models
            .iter()
            .find(|m| m.name == model_id || m.alias.as_deref() == Some(model_id))
            .map(|m| ResolvedModel {
                name: m.name.clone(),
                alias: m.alias.clone(),
                context_tokens: m.context_tokens,
                size_bytes: m.size_bytes,
            })
    }

    /// Welches Modell ist für eine gegebene Rolle (z.B. `"code_writer"`)
    /// zuständig?
    pub fn model_for_role(&self, role: &str) -> Option<&str> {
        self.roles.roles.get(role).map(|r| r.model.as_str())
    }

    /// Alle im Ollama-Inventar bekannten Modelle, unabhängig von einer
    /// konkreten Anfrage.
    pub fn list(&self) -> Vec<ResolvedModel> {
        self.inventory
            .models
            .iter()
            .map(|m| ResolvedModel {
                name: m.name.clone(),
                alias: m.alias.clone(),
                context_tokens: m.context_tokens,
                size_bytes: m.size_bytes,
            })
            .collect()
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let content = fs::read_to_string(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Konnte '{}' nicht lesen: {e}", path.display()),
        )
    })?;
    serde_json::from_str(&content).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "'{}' ist kein gültiges JSON für dieses Schema: {e}",
                path.display()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "tri-ai-registry-{tag}-{}-{now}",
            std::process::id()
        ))
    }

    fn write_fixture(dir: &Path) {
        fs::create_dir_all(dir.join("models")).unwrap();
        fs::write(
            dir.join("models/roles.json"),
            r#"{
                "roles": {
                    "code_writer": { "model": "qwen2.5-coder:14b", "purpose": "Rust" },
                    "fast_chat": { "model": "josie:8b", "alias": "INGRIED", "purpose": "Chat" }
                }
            }"#,
        )
        .unwrap();
        fs::write(
            dir.join("models/ollama-inventory.json"),
            r#"{
                "source": "ollama-api",
                "base_url": "http://127.0.0.1:11434",
                "models": [
                    {
                        "name": "josie:8b",
                        "alias": "INGRIED",
                        "digest": "abc",
                        "size_bytes": 5000000000,
                        "family": "qwen3",
                        "parameters": "8B",
                        "quantization": "Q4_K_M",
                        "context_tokens": 40960,
                        "capabilities": ["completion", "thinking"]
                    }
                ]
            }"#,
        )
        .unwrap();
    }

    #[test]
    fn loads_both_files_and_resolves_by_name() {
        let dir = unique_dir("ok");
        write_fixture(&dir);
        let registry = ModelRegistry::load(&dir).unwrap();
        assert_eq!(registry.resolve("josie:8b").unwrap().context_tokens, 40960);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolves_by_alias() {
        let dir = unique_dir("alias");
        write_fixture(&dir);
        let registry = ModelRegistry::load(&dir).unwrap();
        assert_eq!(registry.resolve("INGRIED").unwrap().name, "josie:8b");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_model_id_resolves_to_none() {
        let dir = unique_dir("unknown");
        write_fixture(&dir);
        let registry = ModelRegistry::load(&dir).unwrap();
        assert!(registry.resolve("nope").is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn model_for_role_looks_up_roles_file() {
        let dir = unique_dir("role");
        write_fixture(&dir);
        let registry = ModelRegistry::load(&dir).unwrap();
        assert_eq!(
            registry.model_for_role("code_writer"),
            Some("qwen2.5-coder:14b")
        );
        assert_eq!(registry.model_for_role("unknown_role"), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_inventory_file_is_a_clean_error_not_a_panic() {
        let dir = unique_dir("missing");
        fs::create_dir_all(dir.join("models")).unwrap();
        fs::write(dir.join("models/roles.json"), r#"{"roles": {}}"#).unwrap();
        // ollama-inventory.json bewusst nicht angelegt.
        let result = ModelRegistry::load(&dir);
        assert!(result.is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
