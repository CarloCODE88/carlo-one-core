//! Deterministischer, backend-unabhängiger Modellkatalog.
//!
//! Scanner liefern diesem Modul ausschließlich bereits ermittelte Metadaten.
//! Fundorte bleiben intern; die serialisierbaren API-Typen enthalten weder
//! absolute noch relative Dateisystempfade.

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

/// Herkunft eines Katalogfunds, ohne den internen Fundort offenzulegen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    DirectGguf,
    Ollama,
    LmStudio,
    Jan,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    #[default]
    LlamaCpp,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LlamaCpp => formatter.write_str("llama_cpp"),
        }
    }
}

/// Von einem Scanner gelieferte, pfadfreie Metadaten.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogMetadata {
    pub digest: String,
    pub alias: Option<String>,
    pub size_bytes: Option<u64>,
    pub backend: BackendKind,
    pub family: Option<String>,
    pub family_verified: bool,
    pub format: Option<String>,
    pub parameters: Option<String>,
    pub quantization: Option<String>,
    pub context_tokens: Option<u64>,
    pub capabilities: Vec<String>,
    pub unavailable_reason: Option<String>,
}

/// Vollständiger öffentlicher Katalogeintrag. Fundorte sind absichtlich nicht
/// Bestandteil dieses Typs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub digest: String,
    pub display_name: String,
    pub aliases: Vec<String>,
    pub sources: Vec<SourceKind>,
    pub size_bytes: Option<u64>,
    pub backend: Option<BackendKind>,
    pub family: Option<String>,
    pub format: Option<String>,
    pub parameters: Option<String>,
    pub quantization: Option<String>,
    pub context_tokens: Option<u64>,
    pub capabilities: Vec<String>,
    pub startable: bool,
    pub unavailable_reason: Option<String>,
    pub paged_supported: bool,
}

/// Kompakte, ebenfalls pfadfreie Ansicht für Modelllisten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSummary {
    pub id: String,
    pub display_name: String,
    pub digest: String,
    pub aliases: Vec<String>,
    pub sources: Vec<SourceKind>,
    pub size_bytes: Option<u64>,
    pub backend: Option<BackendKind>,
    pub family: Option<String>,
    pub format: Option<String>,
    pub parameters: Option<String>,
    pub quantization: Option<String>,
    pub context_tokens: Option<u64>,
    pub capabilities: Vec<String>,
    pub startable: bool,
    pub unavailable_reason: Option<String>,
    pub paged_supported: bool,
}

impl From<&CatalogEntry> for ModelSummary {
    fn from(entry: &CatalogEntry) -> Self {
        Self {
            id: entry.id.clone(),
            display_name: entry.display_name.clone(),
            digest: entry.digest.clone(),
            aliases: entry.aliases.clone(),
            sources: entry.sources.clone(),
            size_bytes: entry.size_bytes,
            backend: entry.backend,
            family: entry.family.clone(),
            format: entry.format.clone(),
            parameters: entry.parameters.clone(),
            quantization: entry.quantization.clone(),
            context_tokens: entry.context_tokens,
            capabilities: entry.capabilities.clone(),
            startable: entry.startable,
            unavailable_reason: entry.unavailable_reason.clone(),
            paged_supported: entry.paged_supported,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    InvalidDigest(String),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDigest(digest) => write!(
                formatter,
                "ungültiger SHA-256-Digest '{digest}': erwartet werden genau 64 Hex-Zeichen"
            ),
        }
    }
}

impl Error for CatalogError {}

/// Sammelstruktur für bereits gescannte Modelle. Die `add_*`-Methoden führen
/// keine Dateisystem- oder Netzwerkzugriffe aus.
#[derive(Debug, Default)]
pub struct ModelCatalog {
    entries: BTreeMap<String, EntryAccumulator>,
}

impl ModelCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_direct_gguf(
        &mut self,
        path: impl Into<PathBuf>,
        metadata: CatalogMetadata,
    ) -> Result<(), CatalogError> {
        self.add(SourceKind::DirectGguf, path.into(), None, metadata)
    }

    pub fn add_ollama(
        &mut self,
        path: impl Into<PathBuf>,
        ollama_name: impl Into<String>,
        metadata: CatalogMetadata,
    ) -> Result<(), CatalogError> {
        self.add(
            SourceKind::Ollama,
            path.into(),
            cleaned(ollama_name.into()),
            metadata,
        )
    }

    pub fn add_lm_studio(
        &mut self,
        path: impl Into<PathBuf>,
        metadata: CatalogMetadata,
    ) -> Result<(), CatalogError> {
        self.add(SourceKind::LmStudio, path.into(), None, metadata)
    }

    pub fn add_jan(
        &mut self,
        path: impl Into<PathBuf>,
        metadata: CatalogMetadata,
    ) -> Result<(), CatalogError> {
        self.add(SourceKind::Jan, path.into(), None, metadata)
    }

    /// Liefert nach stabiler ID sortierte Katalogeinträge.
    pub fn entries(&self) -> Vec<CatalogEntry> {
        self.entries
            .values()
            .map(EntryAccumulator::to_entry)
            .collect()
    }

    /// Liefert nach stabiler ID sortierte, kompakte Modellansichten.
    pub fn summaries(&self) -> Vec<ModelSummary> {
        self.entries().iter().map(ModelSummary::from).collect()
    }

    /// Interne Startauflösung. Der Pfad ist absichtlich in keinem
    /// serialisierbaren Typ enthalten.
    pub(crate) fn start_location(&self, id: &str) -> Option<(SourceKind, PathBuf)> {
        let entry = self.entries.get(id)?;
        entry
            .to_entry()
            .startable
            .then(|| entry.locations.iter().next())
            .flatten()
            .map(|location| (location.source, location.path.clone()))
    }

    fn add(
        &mut self,
        source: SourceKind,
        path: PathBuf,
        ollama_name: Option<String>,
        metadata: CatalogMetadata,
    ) -> Result<(), CatalogError> {
        let digest = normalize_sha256(&metadata.digest)?;
        let location = CatalogLocation { source, path };
        self.entries
            .entry(digest.clone())
            .or_insert_with(|| EntryAccumulator::new(digest))
            .merge(location, ollama_name, metadata);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CatalogLocation {
    source: SourceKind,
    path: PathBuf,
}

#[derive(Debug, Default)]
struct EntryAccumulator {
    digest: String,
    names: BTreeSet<(u8, String)>,
    aliases: BTreeSet<String>,
    locations: BTreeSet<CatalogLocation>,
    sizes: BTreeSet<u64>,
    backends: BTreeSet<BackendKind>,
    families: BTreeSet<String>,
    verified_families: BTreeSet<String>,
    formats: BTreeSet<String>,
    parameters: BTreeSet<String>,
    quantizations: BTreeSet<String>,
    context_lengths: BTreeSet<u64>,
    capabilities: BTreeSet<String>,
    unavailable_reasons: BTreeSet<String>,
}

impl EntryAccumulator {
    fn new(digest: String) -> Self {
        Self {
            digest,
            ..Self::default()
        }
    }

    fn merge(
        &mut self,
        location: CatalogLocation,
        ollama_name: Option<String>,
        metadata: CatalogMetadata,
    ) {
        if let Some(alias) = metadata.alias.and_then(cleaned) {
            self.names.insert((0, alias.clone()));
            self.aliases.insert(alias);
        }
        if let Some(name) = ollama_name {
            self.names.insert((1, name));
        }
        if let Some(file_name) = relative_file_name(&location.path) {
            self.names.insert((2, file_name));
        }
        self.locations.insert(location);

        if let Some(size) = metadata.size_bytes {
            self.sizes.insert(size);
        }
        self.backends.insert(metadata.backend);
        if let Some(family) = metadata.family.and_then(canonical_token) {
            if metadata.family_verified {
                self.verified_families.insert(family.clone());
            }
            self.families.insert(family);
        }
        if let Some(format) = metadata.format.and_then(canonical_token) {
            self.formats.insert(format);
        }
        if let Some(parameters) = metadata.parameters.and_then(cleaned) {
            self.parameters.insert(parameters);
        }
        if let Some(quantization) = metadata.quantization.and_then(canonical_token) {
            self.quantizations.insert(quantization);
        }
        if let Some(context_tokens) = metadata.context_tokens {
            self.context_lengths.insert(context_tokens);
        }
        self.capabilities.extend(
            metadata
                .capabilities
                .into_iter()
                .filter_map(canonical_token),
        );
        if let Some(reason) = metadata.unavailable_reason.and_then(cleaned) {
            self.unavailable_reasons.insert(reason);
        }
    }

    fn to_entry(&self) -> CatalogEntry {
        let mut conflicts = Vec::new();
        record_conflict("size_bytes", &self.sizes, &mut conflicts);
        record_conflict("backend", &self.backends, &mut conflicts);
        record_conflict("family", &self.families, &mut conflicts);
        record_conflict("format", &self.formats, &mut conflicts);
        record_conflict("parameters", &self.parameters, &mut conflicts);
        record_conflict("quantization", &self.quantizations, &mut conflicts);
        record_conflict("context_tokens", &self.context_lengths, &mut conflicts);

        let family = single_value(&self.families);
        let family_verified = family
            .as_ref()
            .is_some_and(|value| self.verified_families.contains(value));
        let paged_supported = conflicts.is_empty()
            && self.unavailable_reasons.is_empty()
            && family_verified
            && family.as_deref() == Some("qwen2");
        let mut reasons: Vec<String> = self.unavailable_reasons.iter().cloned().collect();
        if !conflicts.is_empty() {
            reasons.push(format!(
                "widersprüchliche Metadaten für denselben Digest: {}",
                conflicts.join("; ")
            ));
        }
        let unavailable_reason = (!reasons.is_empty()).then(|| reasons.join("; "));
        let display_name = self
            .names
            .iter()
            .next()
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| self.digest.clone());
        let aliases = self
            .names
            .iter()
            .map(|(_, name)| name)
            .filter(|name| *name != &display_name)
            .cloned()
            .chain(
                self.aliases
                    .iter()
                    .filter(|name| *name != &display_name)
                    .cloned(),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        CatalogEntry {
            id: self.digest.clone(),
            digest: self.digest.clone(),
            display_name,
            aliases,
            sources: self
                .locations
                .iter()
                .map(|location| location.source)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            size_bytes: single_value(&self.sizes),
            backend: single_value(&self.backends),
            family,
            format: single_value(&self.formats),
            parameters: single_value(&self.parameters),
            quantization: single_value(&self.quantizations),
            context_tokens: single_value(&self.context_lengths),
            capabilities: self.capabilities.iter().cloned().collect(),
            startable: unavailable_reason.is_none(),
            unavailable_reason,
            paged_supported,
        }
    }
}

fn normalize_sha256(value: &str) -> Result<String, CatalogError> {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    let hexadecimal = lower
        .strip_prefix("sha256:")
        .or_else(|| lower.strip_prefix("sha256-"))
        .unwrap_or(&lower);
    if hexadecimal.len() != 64 || !hexadecimal.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CatalogError::InvalidDigest(trimmed.to_owned()));
    }
    Ok(format!("sha256:{hexadecimal}"))
}

fn cleaned(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn canonical_token(value: String) -> Option<String> {
    cleaned(value).map(|value| value.to_ascii_lowercase())
}

fn relative_file_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn single_value<T: Clone + Ord>(values: &BTreeSet<T>) -> Option<T> {
    (values.len() == 1).then(|| {
        values
            .iter()
            .next()
            .expect("set contains one value")
            .clone()
    })
}

fn record_conflict<T: fmt::Display + Ord>(
    field: &str,
    values: &BTreeSet<T>,
    conflicts: &mut Vec<String>,
) {
    if values.len() > 1 {
        conflicts.push(format!(
            "{field}=[{}]",
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn metadata(digest: &str) -> CatalogMetadata {
        CatalogMetadata {
            digest: digest.into(),
            size_bytes: Some(42),
            family: Some("qwen2".into()),
            family_verified: true,
            format: Some("GGUF".into()),
            capabilities: vec!["Chat".into()],
            ..CatalogMetadata::default()
        }
    }

    #[test]
    fn digest_is_normalized_and_used_as_stable_id() {
        let mut catalog = ModelCatalog::new();
        catalog
            .add_direct_gguf(
                "model.gguf",
                metadata(&format!(" SHA256:{} ", A.to_uppercase())),
            )
            .unwrap();
        let entry = &catalog.entries()[0];
        assert_eq!(entry.digest, format!("sha256:{A}"));
        assert_eq!(entry.id, entry.digest);
    }

    #[test]
    fn sha256_dash_prefix_is_accepted() {
        let mut catalog = ModelCatalog::new();
        catalog
            .add_direct_gguf("model.gguf", metadata(&format!("sha256-{A}")))
            .unwrap();
        assert_eq!(catalog.entries()[0].id, format!("sha256:{A}"));
    }

    #[test]
    fn malformed_digest_is_rejected_without_entry() {
        let mut catalog = ModelCatalog::new();
        let error = catalog
            .add_direct_gguf("model.gguf", metadata("abc123"))
            .unwrap_err();
        assert!(matches!(error, CatalogError::InvalidDigest(_)));
        assert!(catalog.entries().is_empty());
    }

    #[test]
    fn same_digest_from_multiple_sources_is_merged_once() {
        let mut catalog = ModelCatalog::new();
        catalog.add_direct_gguf("first.gguf", metadata(A)).unwrap();
        catalog
            .add_jan("second.gguf", metadata(&format!("sha256:{A}")))
            .unwrap();
        let entries = catalog.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].sources,
            vec![SourceKind::DirectGguf, SourceKind::Jan]
        );
    }

    #[test]
    fn same_name_with_different_digests_stays_separate() {
        let mut catalog = ModelCatalog::new();
        catalog.add_direct_gguf("same.gguf", metadata(A)).unwrap();
        catalog.add_direct_gguf("same.gguf", metadata(B)).unwrap();
        assert_eq!(catalog.entries().len(), 2);
    }

    #[test]
    fn explicit_alias_has_highest_display_name_priority() {
        let mut catalog = ModelCatalog::new();
        catalog
            .add_ollama("blob", "ollama-name:latest", metadata(A))
            .unwrap();
        let mut aliased = metadata(A);
        aliased.alias = Some("INGRIED".into());
        catalog.add_jan("pretty-file.gguf", aliased).unwrap();
        assert_eq!(catalog.entries()[0].display_name, "INGRIED");
    }

    #[test]
    fn ollama_name_beats_relative_file_name() {
        let mut catalog = ModelCatalog::new();
        catalog.add_direct_gguf("a-file.gguf", metadata(A)).unwrap();
        catalog
            .add_ollama("sha256-blob", "z-model:14b", metadata(A))
            .unwrap();
        assert_eq!(catalog.entries()[0].display_name, "z-model:14b");
    }

    #[test]
    fn same_priority_names_are_chosen_lexicographically() {
        let mut catalog = ModelCatalog::new();
        catalog.add_jan("zeta.gguf", metadata(A)).unwrap();
        catalog.add_lm_studio("alpha.gguf", metadata(A)).unwrap();
        assert_eq!(catalog.entries()[0].display_name, "alpha.gguf");
    }

    #[test]
    fn aliases_sources_and_capabilities_are_sorted_and_deduplicated() {
        let mut catalog = ModelCatalog::new();
        let mut first = metadata(A);
        first.alias = Some("Zulu".into());
        first.capabilities = vec!["Tools".into(), "chat".into(), "tools".into()];
        catalog.add_jan("one.gguf", first).unwrap();
        let mut second = metadata(A);
        second.alias = Some("Alpha".into());
        second.capabilities = vec!["EMBEDDINGS".into(), "chat".into()];
        catalog.add_direct_gguf("two.gguf", second).unwrap();
        catalog.add_jan("one.gguf", metadata(A)).unwrap();

        let entry = &catalog.entries()[0];
        assert_eq!(entry.display_name, "Alpha");
        assert_eq!(entry.aliases, vec!["Zulu", "one.gguf", "two.gguf"]);
        assert_eq!(entry.sources, vec![SourceKind::DirectGguf, SourceKind::Jan]);
        assert_eq!(entry.capabilities, vec!["chat", "embeddings", "tools"]);
        assert_eq!(
            catalog.summaries()[0].aliases,
            vec!["Zulu", "one.gguf", "two.gguf"]
        );
    }

    #[test]
    fn conflicting_size_makes_entry_unstartable() {
        let mut catalog = ModelCatalog::new();
        catalog.add_jan("one.gguf", metadata(A)).unwrap();
        let mut conflicting = metadata(A);
        conflicting.size_bytes = Some(43);
        catalog.add_jan("two.gguf", conflicting).unwrap();
        let entry = &catalog.entries()[0];
        assert!(!entry.startable);
        assert_eq!(entry.size_bytes, None);
        assert!(entry
            .unavailable_reason
            .as_deref()
            .unwrap()
            .contains("size_bytes"));
    }

    #[test]
    fn conflicting_family_makes_entry_unstartable() {
        let mut catalog = ModelCatalog::new();
        catalog.add_jan("one.gguf", metadata(A)).unwrap();
        let mut conflicting = metadata(A);
        conflicting.family = Some("llama".into());
        catalog.add_jan("two.gguf", conflicting).unwrap();
        let entry = &catalog.entries()[0];
        assert!(!entry.startable);
        assert_eq!(entry.family, None);
        assert!(!entry.paged_supported);
    }

    #[test]
    fn conflicting_format_makes_entry_unstartable() {
        let mut catalog = ModelCatalog::new();
        catalog.add_jan("one.gguf", metadata(A)).unwrap();
        let mut conflicting = metadata(A);
        conflicting.format = Some("safetensors".into());
        catalog.add_jan("two.gguf", conflicting).unwrap();
        let entry = &catalog.entries()[0];
        assert!(!entry.startable);
        assert_eq!(entry.format, None);
    }

    #[test]
    fn verified_qwen2_supports_paging() {
        let mut catalog = ModelCatalog::new();
        catalog.add_direct_gguf("qwen.gguf", metadata(A)).unwrap();
        assert!(catalog.entries()[0].paged_supported);
    }

    #[test]
    fn unverified_qwen2_does_not_support_paging() {
        let mut catalog = ModelCatalog::new();
        let mut unverified = metadata(A);
        unverified.family_verified = false;
        catalog.add_direct_gguf("qwen.gguf", unverified).unwrap();
        assert!(!catalog.entries()[0].paged_supported);
    }

    #[test]
    fn qwen3moe_never_supports_paging() {
        let mut catalog = ModelCatalog::new();
        let mut qwen3moe = metadata(A);
        qwen3moe.family = Some("qwen3moe".into());
        catalog.add_direct_gguf("qwen.gguf", qwen3moe).unwrap();
        assert!(!catalog.entries()[0].paged_supported);
    }

    #[test]
    fn serialized_public_types_never_contain_internal_paths() {
        let secret_path = "/private/models/secret/model.gguf";
        let mut catalog = ModelCatalog::new();
        catalog.add_direct_gguf(secret_path, metadata(A)).unwrap();
        let entry_json = serde_json::to_string(&catalog.entries()[0]).unwrap();
        let summary_json = serde_json::to_string(&catalog.summaries()[0]).unwrap();
        assert!(!entry_json.contains("/private/"));
        assert!(!summary_json.contains("/private/"));
        assert!(!entry_json.contains("path"));
        assert!(!summary_json.contains("path"));
    }

    #[test]
    fn result_order_is_deterministic_by_stable_id() {
        let mut catalog = ModelCatalog::new();
        catalog.add_jan("b.gguf", metadata(B)).unwrap();
        catalog.add_jan("a.gguf", metadata(A)).unwrap();
        let ids: Vec<_> = catalog
            .entries()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(ids, vec![format!("sha256:{A}"), format!("sha256:{B}")]);
    }

    #[test]
    fn unavailable_source_has_no_start_location() {
        let mut catalog = ModelCatalog::new();
        let mut unavailable = metadata(A);
        unavailable.unavailable_reason = Some("Blob fehlt".into());
        catalog
            .add_ollama("/private/missing", "model:latest", unavailable)
            .unwrap();
        assert!(!catalog.entries()[0].startable);
        assert_eq!(catalog.start_location(&format!("sha256:{A}")), None);
    }

    #[test]
    fn internal_start_location_is_resolved_only_by_stable_id() {
        let mut catalog = ModelCatalog::new();
        catalog
            .add_direct_gguf("/private/model.gguf", metadata(A))
            .unwrap();
        assert_eq!(
            catalog.start_location(&format!("sha256:{A}")),
            Some((SourceKind::DirectGguf, PathBuf::from("/private/model.gguf")))
        );
        assert_eq!(catalog.start_location("model.gguf"), None);
    }

    #[test]
    fn extended_metadata_is_exposed_without_paths() {
        let mut catalog = ModelCatalog::new();
        let mut detailed = metadata(A);
        detailed.parameters = Some("14.8B".into());
        detailed.quantization = Some("Q4_K_M".into());
        detailed.context_tokens = Some(32_768);
        catalog.add_jan("model.gguf", detailed).unwrap();
        let summary = &catalog.summaries()[0];
        assert_eq!(summary.backend, Some(BackendKind::LlamaCpp));
        assert_eq!(summary.parameters.as_deref(), Some("14.8B"));
        assert_eq!(summary.quantization.as_deref(), Some("q4_k_m"));
        assert_eq!(summary.context_tokens, Some(32_768));
    }
}
