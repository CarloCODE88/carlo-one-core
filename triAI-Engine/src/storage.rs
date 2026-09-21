//! Dry-run storage planning for model chunks.
//!
//! This module deliberately does not rewrite model files. It describes a
//! reversible layout that keeps the original GGUF active until a benchmark
//! proves that indexed, tensor-boundary-aware chunks are equivalent.

use serde::{Deserialize, Serialize};

pub const DEFAULT_CHUNK_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionPlan {
    OriginalGguf,
    ZstdPerChunk { level: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkDescriptor {
    pub index: u32,
    pub offset: u64,
    pub length: u64,
    pub source_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoragePlan {
    pub schema_version: u32,
    pub source_size_bytes: u64,
    pub chunk_bytes: u64,
    pub compression: CompressionPlan,
    pub chunks: Vec<ChunkDescriptor>,
    pub original_path_remains_active: bool,
}

/// Erzeugt eine reversible Chunk-Aufteilung. Die Grenzen sind zunächst nur
/// Bytebereiche; ein späterer GGUF-Indexer muss sie vor Promotion an
/// Tensorgrenzen verschieben. Leere Dateien werden abgelehnt.
pub fn plan_chunks(
    source_size_bytes: u64,
    chunk_bytes: u64,
    compression: CompressionPlan,
) -> Result<StoragePlan, String> {
    if source_size_bytes == 0 {
        return Err("source_size_bytes darf nicht null sein".into());
    }
    if chunk_bytes == 0 {
        return Err("chunk_bytes darf nicht null sein".into());
    }
    let count = source_size_bytes.div_ceil(chunk_bytes);
    if count > u32::MAX as u64 {
        return Err("zu viele Chunks".into());
    }
    let mut chunks = Vec::with_capacity(count as usize);
    for index in 0..count {
        let offset = index * chunk_bytes;
        let length = (source_size_bytes - offset).min(chunk_bytes);
        chunks.push(ChunkDescriptor {
            index: index as u32,
            offset,
            length,
            // Wird erst beim tatsächlichen, explizit freigegebenen Build mit
            // dem Quellhash befüllt; niemals einen erfundenen Digest setzen.
            source_sha256: String::new(),
        });
    }
    Ok(StoragePlan {
        schema_version: 1,
        source_size_bytes,
        chunk_bytes,
        compression,
        chunks,
        original_path_remains_active: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_complete_non_overlapping_chunks() {
        let plan = plan_chunks(10, 4, CompressionPlan::OriginalGguf).unwrap();
        assert_eq!(plan.chunks.len(), 3);
        assert_eq!(plan.chunks[0].offset, 0);
        assert_eq!(plan.chunks[0].length, 4);
        assert_eq!(plan.chunks[2].offset, 8);
        assert_eq!(plan.chunks[2].length, 2);
        assert!(plan.original_path_remains_active);
    }

    #[test]
    fn rejects_invalid_storage_plans() {
        assert!(plan_chunks(0, 4, CompressionPlan::OriginalGguf).is_err());
        assert!(plan_chunks(4, 0, CompressionPlan::OriginalGguf).is_err());
    }
}
