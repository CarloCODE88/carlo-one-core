//! Rekursive Dateisystem-Suche nach `.gguf`-Dateien für den llama.cpp-
//! Backend-Pfad. Bewusst getrennt von `model_registry` (Ollama-Inventar) —
//! zwei unterschiedliche Namensräume, siehe TODOs dort.

use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GgufModel {
    pub relative_path: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GgufMetadata {
    pub version: u32,
    pub tensor_count: u64,
    pub architecture: Option<String>,
    pub name: Option<String>,
    pub quantization: Option<String>,
    pub context_length: Option<u64>,
    pub parameter_count: Option<u64>,
    /// Anzahl der Transformer-Bloecke (`{arch}.block_count`), z.B. fuer die
    /// automatische Ressourcenplanung in `POST /api/engine/plan/auto` — der
    /// Planner verteilt GPU-Layer proportional zur Gesamtzahl.
    pub layers: Option<u64>,
}

/// Durchsucht `dir` rekursiv nach `.gguf`-Dateien (Groß-/Kleinschreibung der
/// Endung egal). Ein fehlendes Verzeichnis ist kein Fehler — frische
/// Installationen ohne lokale GGUF-Dateien sollen `ListModels` nicht kaputt
/// machen — andere IO-Fehler (z.B. fehlende Leserechte) werden durchgereicht.
pub fn scan(dir: &Path) -> io::Result<Vec<GgufModel>> {
    let mut models = Vec::new();
    match scan_into(dir, dir, &mut models) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    }
    models.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(models)
}

/// Löst ausschließlich eine Datei auf, die der Scanner unterhalb von `dir`
/// gefunden hat. Der zurückgegebene Pfad ist kanonisch und liegt nach einer
/// erneuten Prüfung noch unterhalb des kanonischen Modellverzeichnisses.
/// Dadurch kann ein Symlink in `dir` nicht zum Worker-Prozess hinausführen.
pub fn resolve_under(dir: &Path, relative_path: &str) -> io::Result<Option<PathBuf>> {
    let base = match fs::canonicalize(dir) {
        Ok(path) => path,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let found = scan(&base)?;
    if !found
        .iter()
        .any(|model| model.relative_path == relative_path)
    {
        return Ok(None);
    }
    let candidate = fs::canonicalize(base.join(relative_path))?;
    if !candidate.starts_with(&base) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "GGUF-Datei verweist über einen Symlink außerhalb des Modellverzeichnisses",
        ));
    }
    Ok(Some(candidate))
}

/// Liest ausschließlich die GGUF-Metadaten vor den Tensoren. Unbekannte
/// Schlüssel werden typsicher übersprungen; Größenlimits verhindern, dass
/// beschädigte Dateien unbeschränkt Speicher oder Laufzeit verbrauchen.
pub fn inspect(path: &Path) -> io::Result<GgufMetadata> {
    let mut file = fs::File::open(path)?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)?;
    if &magic != b"GGUF" {
        return Err(invalid_data("Datei besitzt keine GGUF-Signatur"));
    }
    let version = read_u32(&mut file)?;
    if !(2..=3).contains(&version) {
        return Err(invalid_data(format!(
            "nicht unterstützte GGUF-Version {version}"
        )));
    }
    let tensor_count = read_u64(&mut file)?;
    let metadata_count = read_u64(&mut file)?;
    if metadata_count > 1_000_000 {
        return Err(invalid_data("zu viele GGUF-Metadateneinträge"));
    }

    let mut metadata = GgufMetadata {
        version,
        tensor_count,
        architecture: None,
        name: None,
        quantization: None,
        context_length: None,
        parameter_count: None,
        layers: None,
    };
    for _ in 0..metadata_count {
        let key = read_string(&mut file, 1024 * 1024)?;
        let value_type = read_u32(&mut file)?;
        let value = read_metadata_value(&mut file, value_type)?;
        match (key.as_str(), value) {
            ("general.architecture", MetadataValue::String(value)) => {
                metadata.architecture = Some(value)
            }
            ("general.name", MetadataValue::String(value)) => metadata.name = Some(value),
            ("general.file_type", MetadataValue::Unsigned(value)) => {
                metadata.quantization = Some(quantization_name(value).to_string())
            }
            ("general.parameter_count", MetadataValue::Unsigned(value)) => {
                metadata.parameter_count = Some(value)
            }
            (key, MetadataValue::Unsigned(value)) if key.ends_with(".context_length") => {
                metadata.context_length = Some(value)
            }
            (key, MetadataValue::Unsigned(value)) if key.ends_with(".block_count") => {
                metadata.layers = Some(value)
            }
            _ => {}
        }
    }
    Ok(metadata)
}

fn scan_into(dir: &Path, base: &Path, out: &mut Vec<GgufModel>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            scan_into(&path, base, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if !is_gguf(&path)? {
            continue;
        }
        let size_bytes = fs::metadata(&path)?.len();
        // `strip_prefix` kann hier nicht scheitern: `path` stammt immer aus
        // einem `read_dir` innerhalb von `base` (rekursiv), ist also
        // garantiert ein Nachfahre. Trotzdem defensiv statt `.unwrap()`, um
        // bei einem unerwarteten Layout einen klaren Fehler statt Panic zu
        // liefern.
        let relative_path = path
            .strip_prefix(base)
            .map_err(io::Error::other)?
            .to_string_lossy()
            .into_owned();
        out.push(GgufModel {
            relative_path,
            size_bytes,
        });
    }
    Ok(())
}

/// Akzeptiert reguläre `.gguf`-Dateien sowie extensionlose GGUF-Blobs, wie
/// sie Ollama unter `blobs/sha256-*` speichert. Die Endung allein reicht nie:
/// in beiden Fällen entscheidet die vier Byte lange GGUF-Signatur.
fn is_gguf(path: &Path) -> io::Result<bool> {
    let has_gguf_extension = path
        .extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("gguf"));
    if !has_gguf_extension && path.extension().is_some() {
        return Ok(false);
    }
    let mut magic = [0u8; 4];
    let mut file = fs::File::open(path)?;
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == b"GGUF"),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}

#[derive(Debug)]
enum MetadataValue {
    Unsigned(u64),
    String(String),
    Other,
}

fn read_metadata_value(reader: &mut impl Read, value_type: u32) -> io::Result<MetadataValue> {
    match value_type {
        0 => Ok(MetadataValue::Unsigned(read_fixed::<1>(reader)?[0] as u64)),
        1 => {
            read_fixed::<1>(reader)?;
            Ok(MetadataValue::Other)
        }
        2 => Ok(MetadataValue::Unsigned(
            u16::from_le_bytes(read_fixed(reader)?) as u64,
        )),
        3 => {
            read_fixed::<2>(reader)?;
            Ok(MetadataValue::Other)
        }
        4 => Ok(MetadataValue::Unsigned(read_u32(reader)? as u64)),
        5 | 6 => {
            read_fixed::<4>(reader)?;
            Ok(MetadataValue::Other)
        }
        7 => {
            read_fixed::<1>(reader)?;
            Ok(MetadataValue::Other)
        }
        8 => Ok(MetadataValue::String(read_string(
            reader,
            16 * 1024 * 1024,
        )?)),
        9 => {
            let element_type = read_u32(reader)?;
            let count = read_u64(reader)?;
            if count > 1_000_000 || element_type == 9 {
                return Err(invalid_data("ungültiges oder zu großes GGUF-Array"));
            }
            for _ in 0..count {
                read_metadata_value(reader, element_type)?;
            }
            Ok(MetadataValue::Other)
        }
        10 => Ok(MetadataValue::Unsigned(read_u64(reader)?)),
        11 | 12 => {
            read_fixed::<8>(reader)?;
            Ok(MetadataValue::Other)
        }
        other => Err(invalid_data(format!(
            "unbekannter GGUF-Metadatentyp {other}"
        ))),
    }
}

fn read_string(reader: &mut impl Read, max_len: u64) -> io::Result<String> {
    let len = read_u64(reader)?;
    if len > max_len {
        return Err(invalid_data("GGUF-String überschreitet das Größenlimit"));
    }
    let mut bytes = vec![0; len as usize];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(invalid_data)
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    Ok(u32::from_le_bytes(read_fixed(reader)?))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    Ok(u64::from_le_bytes(read_fixed(reader)?))
}

fn read_fixed<const N: usize>(reader: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn invalid_data(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn quantization_name(file_type: u64) -> &'static str {
    match file_type {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn unique_dir(tag: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tri-ai-gguf-{tag}-{}-{now}", std::process::id()))
    }

    fn write_minimal_gguf(path: &Path) {
        let mut bytes = Vec::from(&b"GGUF"[..]);
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        fs::write(path, bytes).unwrap();
    }

    fn push_string_value(bytes: &mut Vec<u8>, key: &str, value: &str) {
        bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(&8u32.to_le_bytes());
        bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }

    fn push_u32_value(bytes: &mut Vec<u8>, key: &str, value: u32) {
        bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u64_value(bytes: &mut Vec<u8>, key: &str, value: u64) {
        bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn empty_dir_yields_empty_list() {
        let d = unique_dir("empty");
        fs::create_dir_all(&d).unwrap();
        assert_eq!(scan(&d).unwrap(), Vec::new());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn nonexistent_dir_yields_empty_list_not_error() {
        let d = unique_dir("missing");
        assert!(!d.exists());
        assert_eq!(scan(&d).unwrap(), Vec::new());
    }

    #[test]
    fn finds_root_level_gguf_with_correct_size() {
        let d = unique_dir("root");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("model.gguf"), b"GGUFdata").unwrap();
        let found = scan(&d).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].relative_path, "model.gguf");
        assert_eq!(found[0].size_bytes, 8);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn finds_nested_gguf_with_relative_subpath() {
        let d = unique_dir("nested");
        let nested = d.join("subdir").join("deeper");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("model.gguf"), b"GGUF1").unwrap();
        let found = scan(&d).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].relative_path, "subdir/deeper/model.gguf");
        assert_eq!(found[0].size_bytes, 5);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn ignores_non_gguf_files() {
        let d = unique_dir("other-ext");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("readme.txt"), b"hi").unwrap();
        assert_eq!(scan(&d).unwrap(), Vec::new());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        let d = unique_dir("case");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("model.GGUF"), b"GGUF").unwrap();
        let found = scan(&d).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].relative_path, "model.GGUF");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn finds_extensionless_ollama_style_gguf_blob_by_magic() {
        let d = unique_dir("blob-magic");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("sha256-example"), b"GGUF\x03\0fixture").unwrap();
        let found = scan(&d).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].relative_path, "sha256-example");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn results_are_sorted_by_relative_path() {
        let d = unique_dir("sorted");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("zeta.gguf"), b"GGUFa").unwrap();
        fs::write(d.join("alpha.gguf"), b"GGUFb").unwrap();
        let found = scan(&d).unwrap();
        let paths: Vec<_> = found.iter().map(|m| m.relative_path.as_str()).collect();
        assert_eq!(paths, vec!["alpha.gguf", "zeta.gguf"]);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn resolve_returns_canonical_file_only_when_scanned() {
        let d = unique_dir("resolve");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("model.gguf"), b"GGUF").unwrap();
        assert!(resolve_under(&d, "model.gguf").unwrap().is_some());
        assert_eq!(resolve_under(&d, "missing.gguf").unwrap(), None);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn rejects_gguf_extension_with_wrong_magic() {
        let d = unique_dir("wrong-magic");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("fake.gguf"), b"not a model").unwrap();
        assert!(scan(&d).unwrap().is_empty());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn inspects_required_metadata_without_reading_tensors() {
        let d = unique_dir("metadata");
        fs::create_dir_all(&d).unwrap();
        let path = d.join("model.gguf");
        let mut bytes = Vec::from(&b"GGUF"[..]);
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&42u64.to_le_bytes());
        bytes.extend_from_slice(&6u64.to_le_bytes());
        push_string_value(&mut bytes, "general.architecture", "qwen2");
        push_string_value(&mut bytes, "general.name", "Qwen Coder");
        push_u32_value(&mut bytes, "general.file_type", 15);
        push_u32_value(&mut bytes, "qwen2.context_length", 32_768);
        push_u64_value(&mut bytes, "general.parameter_count", 14_800_000_000);
        push_u32_value(&mut bytes, "qwen2.block_count", 48);
        fs::write(&path, bytes).unwrap();

        let metadata = inspect(&path).unwrap();
        assert_eq!(metadata.version, 3);
        assert_eq!(metadata.tensor_count, 42);
        assert_eq!(metadata.architecture.as_deref(), Some("qwen2"));
        assert_eq!(metadata.name.as_deref(), Some("Qwen Coder"));
        assert_eq!(metadata.quantization.as_deref(), Some("Q4_K_M"));
        assert_eq!(metadata.context_length, Some(32_768));
        assert_eq!(metadata.parameter_count, Some(14_800_000_000));
        assert_eq!(metadata.layers, Some(48));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn missing_block_count_leaves_layers_none() {
        let d = unique_dir("no-block-count");
        fs::create_dir_all(&d).unwrap();
        let path = d.join("model.gguf");
        write_minimal_gguf(&path);
        assert_eq!(inspect(&path).unwrap().layers, None);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn inspect_rejects_truncated_header() {
        let d = unique_dir("truncated");
        fs::create_dir_all(&d).unwrap();
        let path = d.join("model.gguf");
        fs::write(&path, b"GGUF\x03").unwrap();
        assert_eq!(
            inspect(&path).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn minimal_fixture_is_well_formed() {
        let d = unique_dir("minimal");
        fs::create_dir_all(&d).unwrap();
        let path = d.join("model.gguf");
        write_minimal_gguf(&path);
        assert_eq!(inspect(&path).unwrap().tensor_count, 0);
        let _ = fs::remove_dir_all(d);
    }
}
