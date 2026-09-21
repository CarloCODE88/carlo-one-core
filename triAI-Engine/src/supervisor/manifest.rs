//! Verified, path-safe mini-model manifest contract.

use crate::chunk::sha256_of_file;
use crate::error::{Result, TriAIError};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiniModelManifest {
    pub schema_version: u32,
    pub primary_mini_model: MiniModelSpec,
    pub fallback_mini_model: MiniModelSpec,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiniModelSpec {
    pub id: String,
    #[serde(default)]
    pub alias: Option<String>,
    pub path: PathBuf,
    pub quantization: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub role: String,
    pub policy: String,
}

impl MiniModelManifest {
    pub fn load(project_root: impl AsRef<Path>) -> Result<Self> {
        let root = project_root.as_ref();
        let path = root.join("models/manifest.json");
        let manifest: Self = serde_json::from_slice(&fs::read(path).map_err(TriAIError::Io)?)
            .map_err(TriAIError::Serialization)?;
        manifest.validate(root)?;
        Ok(manifest)
    }

    pub fn validate(&self, project_root: &Path) -> Result<()> {
        if self.schema_version != 1 {
            return Err(TriAIError::Other(
                "unsupported mini-model manifest schema".into(),
            ));
        }
        self.primary_mini_model.validate(project_root)?;
        self.fallback_mini_model.validate(project_root)?;
        if self.primary_mini_model.sha256 == self.fallback_mini_model.sha256 {
            return Err(TriAIError::Other("primary and fallback must differ".into()));
        }
        Ok(())
    }
}

impl MiniModelSpec {
    pub fn resolved_path(&self, project_root: &Path) -> Result<PathBuf> {
        if self.path.is_absolute() || self.path.components().any(|part| part.as_os_str() == "..") {
            return Err(TriAIError::Other(
                "mini-model path escapes project root".into(),
            ));
        }
        Ok(project_root.join(&self.path))
    }

    pub fn validate(&self, project_root: &Path) -> Result<()> {
        if self.id.is_empty() || self.size_bytes == 0 || !is_sha256(&self.sha256) {
            return Err(TriAIError::Other(
                "invalid mini-model manifest entry".into(),
            ));
        }
        let path = self.resolved_path(project_root)?;
        let meta = fs::metadata(path).map_err(TriAIError::Io)?;
        if !meta.is_file() || meta.len() != self.size_bytes {
            return Err(TriAIError::Other(
                "mini-model size does not match manifest".into(),
            ));
        }
        Ok(())
    }

    pub fn verify_digest(&self, project_root: &Path) -> Result<()> {
        let actual = sha256_of_file(self.resolved_path(project_root)?).map_err(TriAIError::Io)?;
        if actual != self.sha256 {
            return Err(TriAIError::Other(
                "mini-model digest does not match manifest".into(),
            ));
        }
        Ok(())
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn spec(path: &str, digest: &str) -> MiniModelSpec {
        MiniModelSpec {
            id: "model".into(),
            alias: None,
            path: path.into(),
            quantization: "Q4".into(),
            size_bytes: 1,
            sha256: digest.into(),
            role: "advice".into(),
            policy: "safe".into(),
        }
    }

    #[test]
    fn rejects_path_escape_and_bad_digest() {
        let root = TempDir::new().unwrap();
        let bad = spec("../outside.gguf", &"a".repeat(64));
        assert!(bad.validate(root.path()).is_err());
        let bad_digest = spec("models/a.gguf", "not-a-digest");
        assert!(bad_digest.validate(root.path()).is_err());
    }
}
