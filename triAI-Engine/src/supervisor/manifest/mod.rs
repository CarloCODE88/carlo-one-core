//! Mini-Model Manifest Contract (decoupled).
//!
//! Dieses Modul definiert die statischen, persistierenden Datenstrukturen für das Modellmanagement.
//! Es enthält keine Lauffehler bei der Kommunikation mit dem eigentlichen Worker oder dem
//! Engine-State-System; es ist rein auf Datenintegrität und Struktur validiert.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
// Wir verwenden hier nur Standard-Bibliotheken und keine spezifischen 'crate::' Imports.

/// Fehlerkatalog für Manifest-Validierungen.
#[derive(Debug)]
pub enum ModelManifestError {
    Io(std::io::Error),
    Serialization(serde_json::Error),
    Other(String),
}

impl std::fmt::Display for ModelManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelManifestError::Io(e) => write!(f, "IO Error: {}", e),
            ModelManifestError::Serialization(e) => write!(f, "JSON Serialization Error: {}", e),
            ModelManifestError::Other(s) => write!(f, "Validation Error: {}", s),
        }
    }
}

impl std::error::Error for ModelManifestError {}


/// Die Root-Struktur des gesamten Manifests. Verhindert das direkte Parsen von einzelnen Spezifikationen.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiniModelManifest {
    // Schema Version ist ein stabiler Metadaten-Punktspunkt.
    pub schema_version: u32,
    // Behält die Referenz auf die primäre und Fallback-Instanz bei.
    pub primary_mini_model: MiniModelSpec,
    pub fallback_mini_model: MiniModelSpec,
}

impl MiniModelManifest {
    /// Versucht, das Manifest von einem gegebenen Root-Pfad zu laden und validiert es sofort.
    pub fn load(project_root: impl AsRef<Path>) -> Result<Self> {
        let root = project_root.as_ref();
        let path = root.join("models/manifest.json");
        // Hier wird die I/O-Abhängigkeit (fs::read) simuliert, muss aber bei der Endimplementierung
        // durch eine abstrakte `ResourceLoader` Schnittstelle ersetzt werden.
        let manifest: Self = serde_json::from_slice(&std::fs::read(path).map_err(|e| ModelManifestError::Io(e))?)
            .map_err(|e| ModelManifestError::Serialization(e))?;

        manifest.validate(root)?;
        Ok(manifest)
    }

    /// Überprüft die interne Konsistenz der Manifest-Definition.
    fn validate(&self, project_root: &Path) -> Result<()> {
        if self.schema_version != 1 {
            return Err(ModelManifestError::Other(
                "Unsupported mini-model manifest schema version".into(),
            ));
        }
        self.primary_mini_model.validate(project_root)?;
        self.fallback_mini_model.validate(project_root)?;
        if self.primary_mini_model.sha256 == self.fallback_mini_model.sha256 {
            return Err(ModelManifestError::Other("Primary and fallback models must have different digests".into()));
        }
        Ok(())
    }
}

/// Spezifikation für ein einzelnes mini-Modell (das Kernstück des Manifests).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiniModelSpec {
    // Statische Identifikatoren. Keine Pfadabhängigkeiten im JSON.
    pub id: String,
    pub alias: Option<String>,
    // Der Pfad ist nun eine relative Angabe, die zur Laufzeit gelöst werden muss (lokal/relativ).
    pub path: String,
    pub quantization: String,
    pub size_bytes: u64,
    pub sha256: String, // Muss immer 64 Zeichen sein.
    pub role: String,
    pub policy: String,
}

impl MiniModelSpec {
    /// Berechnet den *absoluten* Pfad des Modells basierend auf dem Projekt-Root.
    /// Achtung: Diese Funktion simuliert die Pfadauflösung und kann daher nicht
    /// ohne das reale `ProjectRoot` ausgeführt werden. Wir werfen nur einen Fehler,
    /// wenn der Pfad zu "escapen" versucht wird.
    pub fn resolved_path(&self, project_root: &Path) -> Result<PathBuf> {
        let path = PathBuf::from(self.path);

        if path.is_absolute() || path.components().any(|part| part.as_os_str() == "..") {
            return Err(ModelManifestError::Other("Mini-model path escapes project root".into()));
        }
        // Die Pfadauflösung erfolgt hier, anstatt sie im JSON zu speichern.
        Ok(project_root.join(&path))
    }

    /// Validiert die Existenz und Größe des Modells am gelösten Pfad.
    fn validate(self, project_root: &Path) -> Result<Self> {
        let path = self.resolved_path(project_root)?;
        if !std::path::Path::new(&path).exists() {
            return Err(ModelManifestError::Other("Mini-model file not found at resolved path".into()));
        }
        // Die eigentliche Größenprüfung wird später durch eine dedizierte ResourceLoader Schicht übernommen.
        Ok(self)
    }

    /// Prüft, ob der SHA256-Digest des tatsächlichen Files mit dem Manifest übereinstimmt.
    pub fn verify_digest(&self, project_root: &Path) -> Result<()> {
        let actual = sha256_of_file(self.resolved_path(project_root)?).map_err(|e| ModelManifestError::Io(e))?;
        if actual != self.sha256 {
            return Err(ModelManifestError::Other("Mini-model digest mismatch".into()));
        }
        Ok(())
    }
}

// --- UTILITY FUNCTIONS (Extern, aber stabil) ---

/// Überprüft die SHA-256 Hex-Struktur.
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// MOCK: Simuliert die Berechnung eines SHA256-Digests von einer Datei.
/// In der finalen Engine wird dies durch einen sicheren, asynchrone Service ersetzt.
pub fn sha256_of_file(_path: &Path) -> Result<String> {
    // Für die Decoupling-Phase simulieren wir Erfolg mit einem Standarddigest
    Ok("a".repeat(64))
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::tempfile::TempDir;

    // Wir müssen ein Mocking Environment für Tests bereitstellen, da reale I/O blockiert.
    fn setup_mock_manifest() -> MiniModelManifest {
        MiniModelManifest {
            schema_version: 1,
            primary_mini_model: MiniModelSpec {
                id: "prime".into(),
                alias: None,
                path: "models/prime.gguf".into(),
                quantization: "Q4".into(),
                size_bytes: 1024 * 1024,
                sha256: "a".repeat(64), // Dummy hash
                role: "primary".into(),
                policy: "strict".into(),
            },
            fallback_mini_model: MiniModelSpec {
                id: "fail".into(),
                alias: None,
                path: "models/fail.gguf".into(),
                quantization: "Q4".into(),
                size_bytes: 1024 * 1024,
                sha256: "b".repeat(64), // Different hash
                role: "fallback".into(),
                policy: "lenient".into(),
            },
        }
    }

    #[test]
    fn manifest_load_and_validate() {
        let temp_dir = TempDir::new().unwrap();
        // Wir erstellen nur die Manifest.json, aber Mocken das sha256_of_file
        std::fs::write(temp_dir.path().join("models/manifest.json"), r#"{
            "schema_version": 1,
            "primary_mini_model": {"id": "prime", "alias": null, "path": "models/prime.gguf", "quantization": "Q4", "size_bytes": 1024*1024, "sha256": "a".repeat(64), "role": "primary", "policy": "strict"},
            "fallback_mini_model": {"id": "fail", "alias": null, "path": "models/fail.gguf", "quantization": "Q4", "size_bytes": 1024*1024, "sha256": "b".repeat(64), "role": "fallback", "policy": "lenient"}
        }"#).unwrap();

        // Da sha256_of_file intern simuliert, sollte die Validierung des Contracts funktionieren.
        let manifest = MiniModelManifest::load(temp_dir.path()).unwrap();
        assert_eq!(manifest.primary_mini_model.id, "prime");
    }

    #[test]
    fn primary_and_fallback_must_be_different() {
        // Simuliere einen Fall, in dem die Hashes gleich sind (eigentlich unmöglich durch das Manifest)
        let temp_manifest = MiniModelManifest {
            schema_version: 1,
            primary_mini_model: MiniModelSpec { id: "p".into(), alias: None, path: "".into(), quantization: "".into(), size_bytes: 1, sha256: "a".repeat(64), role: "".into(), policy: "".into() },
            fallback_mini_model: MiniModelSpec { id: "f".into(), alias: None, path: "".into(), quantization: "".into(), size_bytes: 1, sha256: "a".repeat(64), role: "".into(), policy: "".into()},
        };

        let result = temp_manifest.validate(TempDir::new().unwrap()).unwrap_err();
        assert!(matches!(result, ModelManifestError::Other(_)));
    }
}