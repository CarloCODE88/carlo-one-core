use crate::error::{Result, TriAIError};
use crate::supervisor::expert_tracker::ExpertEvent;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};
use super::{reconstruct_chunk, ChunkDescriptor, ChunkManifest, FallbackReader, TensorIndex};

#[derive(Debug, Clone)]
pub struct LoadedChunk {
    pub chunk_id: String,
    pub data: Vec<u8>,
    pub verified: bool,
    last_accessed: Instant,
}
impl LoadedChunk {
    pub fn memory_bytes(&self) -> u64 {
        self.data.len() as u64
    }
}
#[derive(Debug, Clone)]
pub struct LoaderConfig {
    pub cache_budget_bytes: u64,
    pub verify_on_load: bool,
    pub fallback_gguf_path: Option<PathBuf>,
}
impl Default for LoaderConfig {
    fn default() -> Self {
        Self {
            cache_budget_bytes: 512 * 1024 * 1024,
            verify_on_load: true,
            fallback_gguf_path: None,
        }
    }
}
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub loaded_bytes: u64,
    pub cached_chunks: u32,
}
struct Cache {
    entries: HashMap<String, LoadedChunk>,
    bytes: u64,
    stats: CacheStats,
    budget: u64,
}
impl Cache {
    fn get(&mut self, id: &str) -> Option<LoadedChunk> {
        match self.entries.get_mut(id) {
            Some(v) => {
                v.last_accessed = Instant::now();
                self.stats.hits += 1;
                Some(v.clone())
            }
            None => {
                self.stats.misses += 1;
                None
            }
        }
    }
    fn insert(&mut self, c: LoadedChunk) {
        let need = c.memory_bytes();
        while self.bytes.saturating_add(need) > self.budget && !self.entries.is_empty() {
            let key = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.last_accessed)
                .map(|(k, _)| k.clone())
                .unwrap();
            let old = self.entries.remove(&key).unwrap();
            self.bytes = self.bytes.saturating_sub(old.memory_bytes());
            self.stats.evictions += 1;
        }
        if need <= self.budget {
            self.bytes += need;
            self.stats.loaded_bytes += need;
            self.entries.insert(c.chunk_id.clone(), c);
            self.stats.cached_chunks = self.entries.len() as u32;
        }
    }
}
pub struct ChunkLoader {
    archive: PathBuf,
    manifest: ChunkManifest,
    index: TensorIndex,
    config: LoaderConfig,
    cache: Arc<Mutex<Cache>>,
    fallback: Option<FallbackReader>,
}
impl ChunkLoader {
    pub fn open(dir: impl AsRef<Path>, config: LoaderConfig) -> Result<Self> {
        let archive = dir.as_ref().to_path_buf();
        let manifest: ChunkManifest = read_json(archive.join("manifest.json"))?;
        let index: TensorIndex = read_json(archive.join("tensor_index.json"))?;
        let fallback = config
            .fallback_gguf_path
            .as_ref()
            .map(|path| FallbackReader::open(path, &index.gguf_sha256))
            .transpose()?;
        Ok(Self {
            archive,
            manifest,
            index,
            config: config.clone(),
            cache: Arc::new(Mutex::new(Cache {
                entries: HashMap::new(),
                bytes: 0,
                stats: CacheStats::default(),
                budget: config.cache_budget_bytes,
            })),
            fallback,
        })
    }
    pub fn load_chunk(&self, id: &str) -> Result<LoadedChunk> {
        if let Some(chunk) = self
            .cache
            .lock()
            .map_err(|_| TriAIError::Other("chunk cache poisoned".into()))?
            .get(id)
        {
            return Ok(chunk);
        }
        let desc = self
            .manifest
            .chunks
            .iter()
            .find(|c| c.chunk_id == id)
            .ok_or_else(|| TriAIError::Other("unknown chunk".into()))?;
        let (data, verified) = match self.load_primary(desc) {
            Ok(data) => (data, self.config.verify_on_load),
            Err(primary_error) => {
                let fallback = self.fallback.as_ref().ok_or(primary_error)?;
                let data = reconstruct_chunk(fallback, &self.index, &desc.tensor_names)?;
                if data.len() as u64 != desc.raw_size
                    || format!("{:x}", Sha256::digest(&data)) != desc.sha256
                {
                    return Err(TriAIError::Other(
                        "fallback chunk integrity verification failed".into(),
                    ));
                }
                (data, true)
            }
        };
        let chunk = LoadedChunk {
            chunk_id: id.into(),
            data,
            verified,
            last_accessed: Instant::now(),
        };
        self.cache
            .lock()
            .map_err(|_| TriAIError::Other("chunk cache poisoned".into()))?
            .insert(chunk.clone());
        Ok(chunk)
    }
    pub fn load_tensor_chunk(&self, name: &str) -> Result<LoadedChunk> {
        let entry = self
            .index
            .tensors
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| TriAIError::Other("unknown tensor".into()))?;
        self.load_chunk(
            entry
                .chunk_id
                .as_deref()
                .ok_or_else(|| TriAIError::Other("tensor is not packed".into()))?,
        )
    }
    pub fn load_eager(&self) -> Result<Vec<LoadedChunk>> {
        let mut ids: Vec<_> = self
            .index
            .eager_tensors()
            .iter()
            .filter_map(|t| t.chunk_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids.par_iter()
            .map(|id| self.load_chunk(id))
            .collect()
    }
    pub fn prefetch_async(loader: Arc<Self>, chunk_id: &str) -> Result<()> {
        let id = chunk_id.to_string();
        thread::spawn(move || {
            let _ = loader.load_chunk(&id);
        });
        Ok(())
    }
    pub fn prefetch_for_experts(
        self: &Arc<Self>,
        events: &[ExpertEvent],
    ) -> Result<()> {
        for event in events {
            for expert_id in &event.expert_ids {
                let tensor_name =
                    format!("blk.{}.ffn_gate.{}", event.layer_id, expert_id);
                if let Some(entry) = self.index.tensors.iter().find(|t| t.name == tensor_name) {
                    if let Some(ref chunk_id) = entry.chunk_id {
                        let _ = Self::prefetch_async(Arc::clone(self), chunk_id);
                    }
                }
            }
        }
        Ok(())
    }
    pub fn prefetch_eager_tensors(self: &Arc<Self>) -> Result<()> {
        let eager_ids: Vec<_> = self
            .index
            .eager_tensors()
            .iter()
            .filter_map(|t| t.chunk_id.clone())
            .collect();
        for id in eager_ids {
            let _ = Self::prefetch_async(Arc::clone(self), &id);
        }
        Ok(())
    }
    pub fn cache_stats(&self) -> CacheStats {
        self.cache
            .lock()
            .map(|c| c.stats.clone())
            .unwrap_or_default()
    }
    pub fn clear_cache(&self) {
        if let Ok(mut c) = self.cache.lock() {
            c.entries.clear();
            c.bytes = 0;
            c.stats.cached_chunks = 0;
        }
    }
    pub fn tensor_index(&self) -> &TensorIndex {
        &self.index
    }
    pub fn manifest(&self) -> &ChunkManifest {
        &self.manifest
    }
    pub fn has_fallback(&self) -> bool {
        self.fallback.is_some()
    }
    pub fn archive_path(&self) -> &PathBuf {
        &self.archive
    }
    fn load_primary(&self, desc: &ChunkDescriptor) -> Result<Vec<u8>> {
        let packed = fs::read(
            self.archive
                .join("chunks")
                .join(format!("{}.zst", desc.chunk_id)),
        )
        .map_err(TriAIError::Io)?;
        let data = if desc.compressed_size == desc.raw_size {
            packed
        } else {
            zstd::decode_all(packed.as_slice()).map_err(TriAIError::Io)?
        };
        if self.config.verify_on_load
            && (data.len() as u64 != desc.raw_size
                || format!("{:x}", Sha256::digest(&data)) != desc.sha256)
        {
            return Err(TriAIError::Other(
                "chunk integrity verification failed".into(),
            ));
        }
        Ok(data)
    }
}
fn read_json<T: serde::de::DeserializeOwned>(path: PathBuf) -> Result<T> {
    serde_json::from_slice(&fs::read(path).map_err(TriAIError::Io)?)
        .map_err(TriAIError::Serialization)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use crate::chunk::index::{ChunkDescriptor, TensorEntry};
    use crate::chunk::{pack_gguf, ChunkManifest, PackerConfig, TensorCategory};
    use crate::kernel_worker::KernelWorker;
    use tempfile::TempDir;

    fn archive() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("archive");
        std::fs::create_dir_all(root.join("chunks")).unwrap();
        let raw = b"chunk-data".to_vec();
        let hash = format!("{:x}", Sha256::digest(&raw));
        std::fs::File::create(root.join("chunks/chunk_0000.zst"))
            .unwrap()
            .write_all(&zstd::encode_all(raw.as_slice(), 1).unwrap())
            .unwrap();
        let mut manifest = ChunkManifest::new("m", "d");
        manifest.add_chunk(ChunkDescriptor {
            chunk_id: "chunk_0000".into(),
            categories: vec![TensorCategory::Tokenizer],
            raw_size: raw.len() as u64,
            compressed_size: 0,
            sha256: hash,
            file_offset: 0,
            tensor_names: vec!["tokenizer.x".into()],
        });
        let mut index = TensorIndex::new("m", "d", 0);
        index.add(TensorEntry {
            name: "tokenizer.x".into(),
            category: TensorCategory::Tokenizer,
            dims: vec![1],
            dtype: 0,
            gguf_offset: 0,
            data_offset: None,
            size: raw.len() as u64,
            chunk_id: Some("chunk_0000".into()),
            chunk_offset: Some(0),
        });
        std::fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("tensor_index.json"),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        (temp, root)
    }
    #[test]
    fn roundtrip_tensor_lookup_and_cache_hit() {
        let (_t, root) = archive();
        let loader = ChunkLoader::open(&root, LoaderConfig::default()).unwrap();
        assert!(loader.load_tensor_chunk("tokenizer.x").unwrap().verified);
        loader.load_chunk("chunk_0000").unwrap();
        assert_eq!(loader.cache_stats().hits, 1);
    }
    #[test]
    fn corrupted_frame_fails_closed() {
        let (_t, root) = archive();
        std::fs::write(root.join("chunks/chunk_0000.zst"), b"bad").unwrap();
        assert!(ChunkLoader::open(&root, LoaderConfig::default())
            .unwrap()
            .load_chunk("chunk_0000")
            .is_err());
    }
    #[test]
    fn corrupt_chunk_recovers_from_verified_original_gguf() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.gguf");
        let archive_dir = temp.path().join("archive");
        let mut bytes = b"GGUF".to_vec();
        bytes.extend(3_u32.to_le_bytes());
        bytes.extend(1_u64.to_le_bytes());
        bytes.extend(0_u64.to_le_bytes());
        let name = "tokenizer.x";
        bytes.extend((name.len() as u64).to_le_bytes());
        bytes.extend(name.as_bytes());
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend(16_u64.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(0_u64.to_le_bytes());
        while bytes.len() % 32 != 0 {
            bytes.push(0);
        }
        bytes.extend([7_u8; 64]);
        std::fs::write(&source, bytes).unwrap();
        pack_gguf(&source, &archive_dir, &PackerConfig::default()).unwrap();
        std::fs::write(archive_dir.join("chunks/chunk_0000.zst"), b"corrupt").unwrap();

        let loader = ChunkLoader::open(
            &archive_dir,
            LoaderConfig {
                fallback_gguf_path: Some(source),
                ..LoaderConfig::default()
            },
        )
        .unwrap();
        let chunk = loader.load_chunk("chunk_0000").unwrap();
        assert!(loader.has_fallback());
        assert!(chunk.verified);
        assert_eq!(chunk.data, vec![7_u8; 64]);
    }
    #[test]
    fn fallback_path_must_match_archive_digest() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.gguf");
        let different = temp.path().join("different.gguf");
        let archive_dir = temp.path().join("archive");
        let mut bytes = b"GGUF".to_vec();
        bytes.extend(3_u32.to_le_bytes());
        bytes.extend(1_u64.to_le_bytes());
        bytes.extend(0_u64.to_le_bytes());
        let name = "tokenizer.x";
        bytes.extend((name.len() as u64).to_le_bytes());
        bytes.extend(name.as_bytes());
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend(16_u64.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(0_u64.to_le_bytes());
        while bytes.len() % 32 != 0 {
            bytes.push(0);
        }
        bytes.extend([7_u8; 64]);
        std::fs::write(&source, bytes).unwrap();
        std::fs::write(&different, b"GGUFdifferent").unwrap();
        pack_gguf(&source, &archive_dir, &PackerConfig::default()).unwrap();
        assert!(ChunkLoader::open(
            &archive_dir,
            LoaderConfig {
                fallback_gguf_path: Some(different),
                ..LoaderConfig::default()
            },
        )
        .is_err());
    }

    #[test]
    fn kernel_worker_hybrid_memory() {
        let temp = TempDir::new().unwrap();
        let archive_dir = temp.path().join("archive");
        let worker = KernelWorker::new(&archive_dir, 100);
        worker.load_to_vram("chunk_0000", "tensor_a", 1024).unwrap();
        worker.load_to_vram("chunk_0001", "tensor_b", 1024).unwrap();
        assert_eq!(worker.get_report().vram_pages, 2);
        worker.evict_to_ram("chunk_0000").unwrap();
        assert_eq!(worker.get_report().vram_pages, 1);
        assert_eq!(worker.get_report().ram_pages, 1);
        worker.pin_chunk("chunk_0001").unwrap();
        worker.evict_to_ram("chunk_0001").unwrap();
        assert_eq!(worker.get_report().vram_pages, 1); // Pinned, not evicted
    }
}
