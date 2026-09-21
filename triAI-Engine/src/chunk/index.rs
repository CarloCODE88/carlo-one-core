//! Serializable tensor index and manifest types for future D2/D3 stages.

use super::classify::TensorCategory;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TensorEntry {
    pub name: String,
    pub category: TensorCategory,
    pub dims: Vec<u64>,
    pub dtype: u32,
    pub gguf_offset: u64,
    #[serde(default)]
    pub data_offset: Option<u64>,
    pub size: u64,
    pub chunk_id: Option<String>,
    pub chunk_offset: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TensorIndex {
    pub model_id: String,
    pub gguf_sha256: String,
    pub gguf_size: u64,
    pub tensors: Vec<TensorEntry>,
    pub total_bytes: u64,
}

impl TensorIndex {
    pub fn new(
        model_id: impl Into<String>,
        gguf_sha256: impl Into<String>,
        gguf_size: u64,
    ) -> Self {
        Self {
            model_id: model_id.into(),
            gguf_sha256: gguf_sha256.into(),
            gguf_size,
            tensors: Vec::new(),
            total_bytes: 0,
        }
    }
    pub fn add(&mut self, entry: TensorEntry) {
        self.total_bytes = self.total_bytes.saturating_add(entry.size);
        self.tensors.push(entry);
    }
    pub fn by_category(&self, category: TensorCategory) -> Vec<&TensorEntry> {
        self.tensors
            .iter()
            .filter(|entry| entry.category == category)
            .collect()
    }
    pub fn eager_tensors(&self) -> Vec<&TensorEntry> {
        self.tensors
            .iter()
            .filter(|entry| entry.category.is_eager())
            .collect()
    }
    pub fn validate_order(&self, reference_names: &[String]) -> bool {
        self.tensors
            .iter()
            .map(|entry| &entry.name)
            .eq(reference_names.iter())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkDescriptor {
    pub chunk_id: String,
    pub categories: Vec<TensorCategory>,
    pub raw_size: u64,
    pub compressed_size: u64,
    pub sha256: String,
    pub file_offset: u64,
    pub tensor_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkManifest {
    pub model_id: String,
    pub gguf_sha256: String,
    pub version: u32,
    pub chunks: Vec<ChunkDescriptor>,
    pub total_raw_bytes: u64,
    pub total_compressed_bytes: u64,
}

impl ChunkManifest {
    pub fn new(model_id: impl Into<String>, gguf_sha256: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            gguf_sha256: gguf_sha256.into(),
            version: 1,
            chunks: Vec::new(),
            total_raw_bytes: 0,
            total_compressed_bytes: 0,
        }
    }
    pub fn add_chunk(&mut self, chunk: ChunkDescriptor) {
        self.total_raw_bytes = self.total_raw_bytes.saturating_add(chunk.raw_size);
        self.total_compressed_bytes = self
            .total_compressed_bytes
            .saturating_add(chunk.compressed_size);
        self.chunks.push(chunk);
    }
    pub fn chunk_for_tensor(&self, name: &str) -> Option<&ChunkDescriptor> {
        self.chunks
            .iter()
            .find(|chunk| chunk.tensor_names.iter().any(|tensor| tensor == name))
    }
    pub fn disk_savings_percent(&self) -> f64 {
        if self.total_raw_bytes == 0 {
            0.0
        } else {
            100.0
                * (self
                    .total_raw_bytes
                    .saturating_sub(self.total_compressed_bytes) as f64)
                / self.total_raw_bytes as f64
        }
    }
}

pub fn sha256_of_file(path: impl AsRef<Path>) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn index_preserves_order_and_categories() {
        let mut index = TensorIndex::new("m", "digest", 10);
        index.add(TensorEntry {
            name: "a".into(),
            category: TensorCategory::Tokenizer,
            dims: vec![1],
            dtype: 0,
            gguf_offset: 0,
            data_offset: None,
            size: 4,
            chunk_id: None,
            chunk_offset: None,
        });
        index.add(TensorEntry {
            name: "b".into(),
            category: TensorCategory::Attention,
            dims: vec![1],
            dtype: 0,
            gguf_offset: 4,
            data_offset: None,
            size: 4,
            chunk_id: None,
            chunk_offset: None,
        });
        assert!(index.validate_order(&["a".into(), "b".into()]));
        assert_eq!(index.eager_tensors().len(), 1);
    }
}
