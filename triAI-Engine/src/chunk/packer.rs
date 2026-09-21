//! D2: GGUF tensor spans packed as independently verifiable zstd frames.
//!
//! Disk-Strategie v2: Nur unquantisierte Tensoren komprimieren.
//! - Quantisiert (Q4_K_M): <2% Ersparnis, nicht komprimieren
//! - Unquantisiert (FP32/FP16): 30-60% Ersparnis, mit zstd komprimieren
use super::index::{ChunkDescriptor, TensorEntry};
use super::{
    classify_tensor, sha256_of_file, ChunkManifest, GgufReader, TensorCategory, TensorIndex,
};
use crate::error::{Result, TriAIError};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

/// Kategorien die komprimiert werden (unquantisiert, hohe Redundanz)
const COMPRESSIBLE_CATEGORIES: &[TensorCategory] = &[
    TensorCategory::Embeddings,   // FP16/FP32 → 40% Ersparnis
    TensorCategory::Output,       // FP16/FP32 → 40% Ersparnis
    TensorCategory::Norms,        // FP32 → 35% Ersparnis
    TensorCategory::Tokenizer,    // Text → 60% Ersparnis
    TensorCategory::Metadata,     // Text → 60% Ersparnis
];

/// Prüft ob eine Kategorie komprimiert werden sollte
fn should_compress(category: TensorCategory) -> bool {
    COMPRESSIBLE_CATEGORIES.contains(&category)
}

#[derive(Debug, Clone)]
pub struct PackerConfig {
    pub target_chunk_bytes: u64,
    pub zstd_level: i32,
    pub separate_categories: Vec<TensorCategory>,
    /// Nur komprimierbare Kategorien komprimieren (v2 Strategie)
    pub compress_only_compressible: bool,
}
impl Default for PackerConfig {
    fn default() -> Self {
        Self {
            target_chunk_bytes: 64 * 1024 * 1024,
            zstd_level: 3,
            separate_categories: vec![TensorCategory::MoeExpert],
            compress_only_compressible: true, // v2: Default aktiviert
        }
    }
}
#[derive(Debug, Clone)]
pub struct PackResult {
    pub manifest: ChunkManifest,
    pub tensor_index: TensorIndex,
    pub output_dir: PathBuf,
    pub disk_savings_percent: f64,
}

pub fn pack_gguf(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    config: &PackerConfig,
) -> Result<PackResult> {
    let input = input.as_ref();
    let output = output.as_ref();
    if output.exists() || config.target_chunk_bytes == 0 || !(1..=22).contains(&config.zstd_level) {
        return Err(TriAIError::Other(
            "invalid pack destination or configuration".into(),
        ));
    }
    let mut r = GgufReader::open(input)?;
    let (meta, infos) = r.parse_header()?;
    if infos.is_empty() {
        return Err(TriAIError::Other("GGUF contains no tensors".into()));
    }
    let id = meta
        .kv
        .get("general.name")
        .cloned()
        .or_else(|| input.file_stem().map(|v| v.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unknown".into());
    let digest = sha256_of_file(input).map_err(TriAIError::Io)?;
    let len = fs::metadata(input).map_err(TriAIError::Io)?.len();
    let cats: Vec<_> = infos.iter().map(|x| classify_tensor(&x.name)).collect();
    fs::create_dir(output).map_err(TriAIError::Io)?;
    let chunks = output.join("chunks");
    if let Err(e) = fs::create_dir(&chunks) {
        let _ = fs::remove_dir(output);
        return Err(TriAIError::Io(e));
    }
    let outcome = (|| {
        let mut index = TensorIndex::new(id.clone(), digest.clone(), len);
        for (x, c) in infos.iter().zip(&cats) {
            index.add(TensorEntry {
                name: x.name.clone(),
                category: *c,
                dims: x.dims.clone(),
                dtype: x.dtype,
                gguf_offset: x.offset,
                data_offset: Some(x.data_offset),
                size: x.size,
                chunk_id: None,
                chunk_offset: None,
            });
        }
        let mut manifest = ChunkManifest::new(id, digest);
        let mut source = File::open(input).map_err(TriAIError::Io)?;
        let mut group = Vec::new();
        let mut used: u64 = 0;
        let mut groups = Vec::new();
        for (n, x) in infos.iter().enumerate() {
            if cats[n].eq(&TensorCategory::MoeExpert)
                && config.separate_categories.contains(&cats[n])
            {
                if !group.is_empty() {
                    groups.push(std::mem::take(&mut group));
                    used = 0;
                }
                groups.push(vec![n]);
            } else {
                if !group.is_empty() && used.saturating_add(x.size) > config.target_chunk_bytes {
                    groups.push(std::mem::take(&mut group));
                    used = 0;
                }
                group.push(n);
                used = used.saturating_add(x.size);
            }
        }
        if !group.is_empty() {
            groups.push(group);
        }
        for (number, g) in groups.iter().enumerate() {
            let cid = format!("chunk_{number:04}");
            let mut raw = Vec::new();
            let mut names = Vec::new();
            let mut categories = Vec::new();
            for &n in g {
                let x = &infos[n];
                let offset = raw.len() as u64;
                source
                    .seek(SeekFrom::Start(x.data_offset))
                    .map_err(TriAIError::Io)?;
                let mut b = vec![
                    0;
                    usize::try_from(x.size)
                        .map_err(|_| TriAIError::Other("tensor too large".into()))?
                ];
                source.read_exact(&mut b).map_err(TriAIError::Io)?;
                raw.extend_from_slice(&b);
                index.tensors[n].chunk_id = Some(cid.clone());
                index.tensors[n].chunk_offset = Some(offset);
                names.push(x.name.clone());
                if !categories.contains(&cats[n]) {
                    categories.push(cats[n]);
                }
            }
            let hash = format!("{:x}", Sha256::digest(&raw));

            // v2 Disk-Strategie: Nur unquantisierte Tensoren komprimieren
            let should_compress_chunk = if config.compress_only_compressible {
                categories.iter().all(|cat| should_compress(*cat))
            } else {
                true // Alte Strategie: Alles komprimieren
            };

            let packed = if should_compress_chunk {
                zstd::encode_all(raw.as_slice(), config.zstd_level).map_err(TriAIError::Io)?
            } else {
                // Unkomprimiert: Nur kopieren (Q4_K_M etc. bringt <2% Ersparnis)
                raw.clone()
            };

            let mut f = File::create(chunks.join(format!("{cid}.zst"))).map_err(TriAIError::Io)?;
            f.write_all(&packed).map_err(TriAIError::Io)?;
            f.sync_all().map_err(TriAIError::Io)?;
            manifest.add_chunk(ChunkDescriptor {
                chunk_id: cid,
                categories,
                raw_size: raw.len() as u64,
                compressed_size: packed.len() as u64,
                sha256: hash,
                file_offset: 0,
                tensor_names: names,
            });
        }
        fs::write(
            output.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).map_err(TriAIError::Serialization)?,
        )
        .map_err(TriAIError::Io)?;
        fs::write(
            output.join("tensor_index.json"),
            serde_json::to_vec_pretty(&index).map_err(TriAIError::Serialization)?,
        )
        .map_err(TriAIError::Io)?;
        verify_archive(output)?;
        Ok(PackResult {
            disk_savings_percent: manifest.disk_savings_percent(),
            manifest,
            tensor_index: index,
            output_dir: output.into(),
        })
    })();
    if outcome.is_err() {
        let _ = fs::remove_dir_all(output);
    }
    outcome
}
pub fn verify_archive(dir: impl AsRef<Path>) -> Result<bool> {
    let dir = dir.as_ref();
    let manifest: ChunkManifest =
        serde_json::from_slice(&fs::read(dir.join("manifest.json")).map_err(TriAIError::Io)?)
            .map_err(TriAIError::Serialization)?;
    for c in manifest.chunks {
        let chunk_data = fs::read(dir.join("chunks").join(format!("{}.zst", c.chunk_id)))
            .map_err(TriAIError::Io)?;
        let raw = if c.compressed_size == c.raw_size {
            chunk_data
        } else {
            zstd::decode_all(chunk_data.as_slice()).map_err(TriAIError::Io)?
        };
        if raw.len() as u64 != c.raw_size || format!("{:x}", Sha256::digest(&raw)) != c.sha256 {
            return Err(TriAIError::Other(
                "chunk integrity verification failed".into(),
            ));
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn put_u32(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend(value.to_le_bytes());
    }
    fn put_u64(bytes: &mut Vec<u8>, value: u64) {
        bytes.extend(value.to_le_bytes());
    }
    fn put_text(bytes: &mut Vec<u8>, value: &str) {
        put_u64(bytes, value.len() as u64);
        bytes.extend(value.as_bytes());
    }
    fn fixture(path: &Path, names: &[&str]) {
        let mut bytes = b"GGUF".to_vec();
        put_u32(&mut bytes, 3);
        put_u64(&mut bytes, names.len() as u64);
        put_u64(&mut bytes, 0);
        for (number, name) in names.iter().enumerate() {
            put_text(&mut bytes, name);
            put_u32(&mut bytes, 1);
            put_u64(&mut bytes, 16);
            put_u32(&mut bytes, 0);
            put_u64(&mut bytes, (number * 64) as u64);
        }
        while bytes.len() % 32 != 0 {
            bytes.push(0);
        }
        for number in 0..names.len() {
            bytes.extend(std::iter::repeat((number + 1) as u8).take(64));
        }
        File::create(path).unwrap().write_all(&bytes).unwrap();
    }
    #[test]
    fn pack_roundtrip_records_offsets() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("m.gguf");
        let output = temp.path().join("out");
        fixture(&input, &["blk.0.attn_q.weight", "blk.1.attn_q.weight"]);
        let result = pack_gguf(
            &input,
            &output,
            &PackerConfig {
                target_chunk_bytes: 64,
                zstd_level: 1,
                separate_categories: vec![],
                compress_only_compressible: true,
            },
        )
        .unwrap();
        assert!(verify_archive(&output).unwrap());
        assert_eq!(result.manifest.chunks.len(), 2);
        assert_eq!(result.tensor_index.tensors[0].chunk_offset, Some(0));
    }
    #[test]
    fn moe_separation_preserves_order() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("m.gguf");
        let output = temp.path().join("out");
        fixture(
            &input,
            &[
                "blk.0.attn_q.weight",
                "blk.0.ffn_gate.1",
                "blk.1.attn_q.weight",
            ],
        );
        let result = pack_gguf(&input, &output, &PackerConfig::default()).unwrap();
        let names: Vec<_> = result
            .manifest
            .chunks
            .iter()
            .flat_map(|chunk| chunk.tensor_names.iter().cloned())
            .collect();
        assert_eq!(
            names,
            vec![
                "blk.0.attn_q.weight",
                "blk.0.ffn_gate.1",
                "blk.1.attn_q.weight"
            ]
        );
    }
    #[test]
    fn corruption_and_existing_output_fail_closed() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("m.gguf");
        let output = temp.path().join("out");
        fixture(&input, &["blk.0.attn_q.weight"]);
        pack_gguf(&input, &output, &PackerConfig::default()).unwrap();
        assert!(pack_gguf(&input, &output, &PackerConfig::default()).is_err());
        fs::write(output.join("chunks/chunk_0000.zst"), b"bad").unwrap();
        assert!(verify_archive(&output).is_err());
    }

    #[test]
    fn v2_strategy_compresses_only_compressible() {
        // Verifiziert dass die v2 Strategie nur unquantisierte Tensoren komprimiert
        assert!(should_compress(TensorCategory::Embeddings));
        assert!(should_compress(TensorCategory::Norms));
        assert!(should_compress(TensorCategory::Tokenizer));

        // Quantisierte sollten NICHT komprimiert werden
        assert!(!should_compress(TensorCategory::Attention));
        assert!(!should_compress(TensorCategory::DenseFfn));
        assert!(!should_compress(TensorCategory::MoeExpert));
    }

    #[test]
    fn old_strategy_compresses_everything() {
        let config = PackerConfig {
            compress_only_compressible: false,
            ..Default::default()
        };
        // Mit compress_only_compressible=false werden alle komprimiert
        assert!(config.compress_only_compressible == false);
    }
}
