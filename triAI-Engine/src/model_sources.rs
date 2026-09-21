//! Read-only Adapter für direkte GGUF-Dateien, Ollama, LM Studio und Jan.
//!
//! Jeder Adapter liefert nur nachprüfbare Daten an den Katalog. Fehler einer
//! Quelle werden gesammelt und verhindern nicht, dass andere Quellen
//! weiterhin Modelle liefern.

use crate::{
    config::Config,
    gguf_registry,
    model_catalog::{CatalogMetadata, ModelCatalog, SourceKind},
    model_registry::ModelRegistry,
};
use serde::Deserialize;
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::RwLock,
};

const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const OLLAMA_MODEL_MEDIA_TYPE: &str = "application/vnd.ollama.image.model";

#[derive(Debug, Default)]
pub struct CatalogScan {
    pub catalog: ModelCatalog,
    pub warnings: Vec<String>,
}

pub fn scan(config: &Config) -> CatalogScan {
    let mut result = CatalogScan::default();
    scan_gguf_root(&mut result, &config.models_dir(), SourceKind::DirectGguf);
    for root in &config.paths.lm_studio_roots {
        scan_gguf_root(&mut result, root, SourceKind::LmStudio);
    }
    for root in &config.paths.jan_roots {
        scan_gguf_root(&mut result, root, SourceKind::Jan);
    }
    if let Some(root) = &config.paths.ollama_root {
        scan_ollama(&mut result, root, &config.paths.project_root);
    }
    result
}

/// Scannt alle konfigurierten Modellquellen neu und tauscht das Ergebnis
/// atomar im geteilten Katalog aus — gemeinsam genutzt vom manuellen
/// Rescan-Endpunkt (`POST /api/models/rescan`) und vom Downloadmanager nach
/// einem erfolgreichen Download. Gibt die neue Modellanzahl und die
/// Scan-Warnungen zurueck, damit der jeweilige Aufrufer sein eigenes
/// Observability-Event mit den fuer ihn passenden Zusatzfeldern (z.B.
/// `trigger`/`download_id`) emittieren kann.
pub fn rescan_and_apply(config: &Config, catalog: &RwLock<ModelCatalog>) -> (usize, Vec<String>) {
    let scan = scan(config);
    let model_count = scan.catalog.entries().len();
    *catalog.write().unwrap() = scan.catalog;
    (model_count, scan.warnings)
}

fn scan_gguf_root(result: &mut CatalogScan, root: &Path, source: SourceKind) {
    let models = match gguf_registry::scan(root) {
        Ok(models) => models,
        Err(error) => {
            result.warnings.push(format!(
                "{}-Root '{}' nicht lesbar: {error}",
                source_label(source),
                root.display()
            ));
            return;
        }
    };
    for model in models {
        let path = match gguf_registry::resolve_under(root, &model.relative_path) {
            Ok(Some(path)) => path,
            Ok(None) => continue,
            Err(error) => {
                result.warnings.push(format!(
                    "{}-Datei '{}' nicht sicher auflösbar: {error}",
                    source_label(source),
                    model.relative_path
                ));
                continue;
            }
        };
        let digest = match sha256_file(&path) {
            Ok(digest) => digest,
            Err(error) => {
                result.warnings.push(format!(
                    "{}-Datei '{}' nicht hashbar: {error}",
                    source_label(source),
                    model.relative_path
                ));
                continue;
            }
        };
        let metadata = metadata_for_file(&path, digest, Some(model.size_bytes), None);
        if let Err(error) = add_file(&mut result.catalog, source, path, None, metadata) {
            result.warnings.push(error);
        }
    }
}

fn scan_ollama(result: &mut CatalogScan, root: &Path, project_root: &Path) {
    let manifests_root = root.join("manifests");
    let manifests = match regular_files_under(&manifests_root) {
        Ok(files) => files,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            result.warnings.push(format!(
                "Ollama-Manifeste '{}' nicht lesbar: {error}",
                manifests_root.display()
            ));
            return;
        }
    };
    let aliases = ModelRegistry::load(project_root).ok();
    for manifest_path in manifests {
        let name = match ollama_name(&manifests_root, &manifest_path) {
            Some(name) => name,
            None => continue,
        };
        let manifest = match read_manifest(&manifest_path) {
            Ok(manifest) => manifest,
            Err(error) => {
                result
                    .warnings
                    .push(format!("Ollama-Manifest '{name}' ungültig: {error}"));
                continue;
            }
        };
        let model_layers: Vec<_> = manifest
            .layers
            .into_iter()
            .filter(|layer| layer.media_type == OLLAMA_MODEL_MEDIA_TYPE)
            .collect();
        if model_layers.len() != 1 {
            result.warnings.push(format!(
                "Ollama-Manifest '{name}' enthält {} Model-Layer statt genau einem",
                model_layers.len()
            ));
            continue;
        }
        let layer = &model_layers[0];
        let Some(hex_digest) = valid_sha256(&layer.digest) else {
            result.warnings.push(format!(
                "Ollama-Manifest '{name}' hat ungültigen Model-Digest"
            ));
            continue;
        };
        let expected_digest = format!("sha256:{hex_digest}");
        let blob_path = root.join("blobs").join(format!("sha256-{hex_digest}"));
        let alias = aliases
            .as_ref()
            .and_then(|registry| registry.resolve(&name))
            .and_then(|model| model.alias);
        let mut unavailable = Vec::new();
        let actual_size = match fs::metadata(&blob_path) {
            Ok(metadata) if metadata.is_file() => {
                let size = metadata.len();
                if size != layer.size {
                    unavailable.push(format!(
                        "Ollama-Blobgröße {size} stimmt nicht mit Manifestgröße {} überein",
                        layer.size
                    ));
                }
                Some(size)
            }
            Ok(_) => {
                unavailable.push("Ollama-Blob ist keine reguläre Datei".to_string());
                None
            }
            Err(error) => {
                unavailable.push(format!("Ollama-Blob fehlt oder ist nicht lesbar: {error}"));
                None
            }
        };
        if actual_size.is_some() {
            match sha256_file(&blob_path) {
                Ok(actual) if actual == expected_digest => {}
                Ok(actual) => unavailable.push(format!(
                    "Ollama-Blob-Digest {actual} stimmt nicht mit {expected_digest} überein"
                )),
                Err(error) => unavailable.push(format!("Ollama-Blob nicht hashbar: {error}")),
            }
        }
        let mut metadata = metadata_for_file(&blob_path, expected_digest, Some(layer.size), alias);
        if !unavailable.is_empty() {
            metadata.unavailable_reason = Some(unavailable.join("; "));
            metadata.family_verified = false;
        }
        if let Err(error) = result.catalog.add_ollama(blob_path, name, metadata) {
            result
                .warnings
                .push(format!("Ollama-Eintrag nicht katalogisierbar: {error}"));
        }
    }
}

fn metadata_for_file(
    path: &Path,
    digest: String,
    size_bytes: Option<u64>,
    alias: Option<String>,
) -> CatalogMetadata {
    match gguf_registry::inspect(path) {
        Ok(gguf) => CatalogMetadata {
            digest,
            alias,
            size_bytes,
            family: gguf.architecture,
            family_verified: true,
            format: Some("gguf".into()),
            parameters: gguf.parameter_count.map(|value| value.to_string()),
            quantization: gguf.quantization,
            context_tokens: gguf.context_length,
            capabilities: vec!["completion".into()],
            ..CatalogMetadata::default()
        },
        Err(error) => CatalogMetadata {
            digest,
            alias,
            size_bytes,
            format: Some("gguf".into()),
            unavailable_reason: Some(format!("GGUF-Metadaten ungültig: {error}")),
            ..CatalogMetadata::default()
        },
    }
}

fn add_file(
    catalog: &mut ModelCatalog,
    source: SourceKind,
    path: PathBuf,
    name: Option<String>,
    metadata: CatalogMetadata,
) -> Result<(), String> {
    let result = match source {
        SourceKind::DirectGguf => catalog.add_direct_gguf(path, metadata),
        SourceKind::Ollama => catalog.add_ollama(path, name.unwrap_or_default(), metadata),
        SourceKind::LmStudio => catalog.add_lm_studio(path, metadata),
        SourceKind::Jan => catalog.add_jan(path, metadata),
    };
    result.map_err(|error| format!("Katalogeintrag ungültig: {error}"))
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let output = Command::new("sha256sum").arg("--").arg(path).output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let digest = stdout
        .split_whitespace()
        .next()
        .and_then(valid_sha256)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "sha256sum ohne Digest"))?;
    Ok(format!("sha256:{digest}"))
}

fn valid_sha256(value: &str) -> Option<&str> {
    let value = value
        .trim()
        .strip_prefix("sha256:")
        .or_else(|| value.trim().strip_prefix("sha256-"))
        .unwrap_or_else(|| value.trim());
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(value)
}

#[derive(Debug, Deserialize)]
struct OllamaManifest {
    layers: Vec<OllamaLayer>,
}

#[derive(Debug, Deserialize)]
struct OllamaLayer {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
}

fn read_manifest(path: &Path) -> io::Result<OllamaManifest> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Manifest überschreitet 4 MiB",
        ));
    }
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn regular_files_under(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_regular_files(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_regular_files(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_regular_files(&entry.path(), files)?;
        } else if file_type.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn ollama_name(root: &Path, manifest: &Path) -> Option<String> {
    let mut parts: Vec<String> = manifest
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.len() < 2 {
        return None;
    }
    let tag = parts.pop()?;
    if parts
        .first()
        .is_some_and(|part| part == "registry.ollama.ai")
    {
        parts.remove(0);
        if parts.first().is_some_and(|part| part == "library") {
            parts.remove(0);
        }
    }
    (!parts.is_empty()).then(|| format!("{}:{tag}", parts.join("/")))
}

fn source_label(source: SourceKind) -> &'static str {
    match source {
        SourceKind::DirectGguf => "GGUF",
        SourceKind::Ollama => "Ollama",
        SourceKind::LmStudio => "LM-Studio",
        SourceKind::Jan => "Jan",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tri-ai-source-{tag}-{}-{now}", std::process::id()))
    }

    fn write_qwen2_gguf(path: &Path) {
        let mut bytes = Vec::from(&b"GGUF"[..]);
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&3u64.to_le_bytes());
        push_string(&mut bytes, "general.architecture", "qwen2");
        push_string(&mut bytes, "general.name", "Fixture Qwen");
        push_u32(&mut bytes, "general.file_type", 15);
        fs::write(path, bytes).unwrap();
    }

    fn push_string(bytes: &mut Vec<u8>, key: &str, value: &str) {
        bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(&8u32.to_le_bytes());
        bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }

    fn push_u32(bytes: &mut Vec<u8>, key: &str, value: u32) {
        bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn write_ollama_manifest(root: &Path, name: &str, digest: &str, size: u64) {
        let (model, tag) = name.rsplit_once(':').unwrap();
        let path = root
            .join("manifests/registry.ollama.ai/library")
            .join(model)
            .join(tag);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            serde_json::json!({
                "layers": [{
                    "mediaType": OLLAMA_MODEL_MEDIA_TYPE,
                    "digest": digest,
                    "size": size
                }]
            })
            .to_string(),
        )
        .unwrap();
    }

    fn scan_config(dir: &Path, direct_root: &Path, ollama_root: &Path) -> Config {
        let mut config = Config::default();
        config.paths.project_root = dir.to_path_buf();
        config.paths.models_dir = Some(direct_root.to_path_buf());
        config.paths.ollama_root = Some(ollama_root.to_path_buf());
        config
    }

    #[test]
    fn validates_sha256_spellings() {
        let digest = "a".repeat(64);
        assert_eq!(valid_sha256(&digest), Some(digest.as_str()));
        assert_eq!(
            valid_sha256(&format!("sha256:{digest}")),
            Some(digest.as_str())
        );
        assert_eq!(valid_sha256("abc"), None);
    }

    #[test]
    fn derives_library_and_external_ollama_names() {
        let root = Path::new("/models/manifests");
        assert_eq!(
            ollama_name(
                root,
                Path::new("/models/manifests/registry.ollama.ai/library/qwen2.5-coder/14b")
            )
            .as_deref(),
            Some("qwen2.5-coder:14b")
        );
        assert_eq!(
            ollama_name(
                root,
                Path::new("/models/manifests/hf.co/owner/model/Q4_K_M")
            )
            .as_deref(),
            Some("hf.co/owner/model:Q4_K_M")
        );
    }

    #[test]
    fn manifest_reader_rejects_oversized_input() {
        let dir = temp_dir("large-manifest");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("manifest");
        fs::write(&path, vec![0u8; MAX_MANIFEST_BYTES as usize + 1]).unwrap();
        assert_eq!(
            read_manifest(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn regular_file_walk_skips_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("symlink");
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested/manifest"), b"{}").unwrap();
        symlink("/etc/passwd", dir.join("outside")).unwrap();
        let files = regular_files_under(&dir).unwrap();
        assert_eq!(files, vec![dir.join("nested/manifest")]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sha256_helper_hashes_exact_bytes() {
        let dir = temp_dir("hash");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn valid_ollama_manifest_resolves_verified_qwen2_blob() {
        let dir = temp_dir("ollama-ok");
        let root = dir.join("ollama");
        let direct = dir.join("direct");
        fs::create_dir_all(root.join("blobs")).unwrap();
        let temporary = root.join("blobs/model.tmp");
        write_qwen2_gguf(&temporary);
        let digest = sha256_file(&temporary).unwrap();
        let hex = digest.strip_prefix("sha256:").unwrap();
        let blob = root.join("blobs").join(format!("sha256-{hex}"));
        fs::rename(&temporary, &blob).unwrap();
        let size = fs::metadata(&blob).unwrap().len();
        write_ollama_manifest(&root, "fixture:latest", &digest, size);

        let result = scan(&scan_config(&dir, &direct, &root));
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let entries = result.catalog.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, digest);
        assert_eq!(entries[0].display_name, "fixture:latest");
        assert_eq!(entries[0].family.as_deref(), Some("qwen2"));
        assert_eq!(entries[0].quantization.as_deref(), Some("q4_k_m"));
        assert!(entries[0].startable);
        assert!(entries[0].paged_supported);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_ollama_blob_is_listed_but_not_startable() {
        let dir = temp_dir("ollama-missing");
        let root = dir.join("ollama");
        let digest = format!("sha256:{}", "a".repeat(64));
        write_ollama_manifest(&root, "missing:latest", &digest, 42);

        let result = scan(&scan_config(&dir, &dir.join("direct"), &root));
        let entries = result.catalog.entries();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].startable);
        assert!(entries[0]
            .unavailable_reason
            .as_deref()
            .unwrap()
            .contains("fehlt"));
        assert_eq!(result.catalog.start_location(&digest), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn wrong_ollama_blob_digest_is_not_startable() {
        let dir = temp_dir("ollama-digest");
        let root = dir.join("ollama");
        let blobs = root.join("blobs");
        fs::create_dir_all(&blobs).unwrap();
        let expected = format!("sha256:{}", "a".repeat(64));
        let blob = blobs.join(format!("sha256-{}", "a".repeat(64)));
        write_qwen2_gguf(&blob);
        let size = fs::metadata(&blob).unwrap().len();
        write_ollama_manifest(&root, "wrong:latest", &expected, size);

        let result = scan(&scan_config(&dir, &dir.join("direct"), &root));
        let entry = &result.catalog.entries()[0];
        assert!(!entry.startable);
        assert!(entry
            .unavailable_reason
            .as_deref()
            .unwrap()
            .contains("stimmt nicht"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn same_verified_file_from_direct_and_ollama_merges_by_digest() {
        let dir = temp_dir("dedupe");
        let direct = dir.join("direct");
        let root = dir.join("ollama");
        fs::create_dir_all(&direct).unwrap();
        fs::create_dir_all(root.join("blobs")).unwrap();
        let direct_file = direct.join("same.gguf");
        write_qwen2_gguf(&direct_file);
        let digest = sha256_file(&direct_file).unwrap();
        let hex = digest.strip_prefix("sha256:").unwrap();
        let blob = root.join("blobs").join(format!("sha256-{hex}"));
        fs::copy(&direct_file, &blob).unwrap();
        let size = fs::metadata(&blob).unwrap().len();
        write_ollama_manifest(&root, "same:latest", &digest, size);

        let result = scan(&scan_config(&dir, &direct, &root));
        let entries = result.catalog.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].sources,
            vec![SourceKind::DirectGguf, SourceKind::Ollama]
        );
        assert_eq!(entries[0].display_name, "same:latest");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn lm_studio_and_jan_roots_are_scanned_read_only_and_deduplicated() {
        let dir = temp_dir("desktop-roots");
        let lm_root = dir.join("lm-studio");
        let jan_root = dir.join("jan");
        fs::create_dir_all(&lm_root).unwrap();
        fs::create_dir_all(&jan_root).unwrap();
        let lm_file = lm_root.join("shared.gguf");
        let jan_file = jan_root.join("copy.gguf");
        write_qwen2_gguf(&lm_file);
        fs::copy(&lm_file, &jan_file).unwrap();
        let before = fs::read(&lm_file).unwrap();
        let mut config = Config::default();
        config.paths.models_dir = Some(dir.join("direct"));
        config.paths.lm_studio_roots = vec![lm_root];
        config.paths.jan_roots = vec![jan_root];

        let result = scan(&config);
        let entries = result.catalog.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].sources,
            vec![SourceKind::LmStudio, SourceKind::Jan]
        );
        assert_eq!(fs::read(&lm_file).unwrap(), before);
        let _ = fs::remove_dir_all(dir);
    }
}
