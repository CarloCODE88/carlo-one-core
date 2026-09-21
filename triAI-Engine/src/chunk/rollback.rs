//! Read-only original-GGUF fallback for corrupted packed chunks.
use super::{sha256_of_file, TensorIndex};
use crate::error::{Result, TriAIError};
use memmap2::Mmap;
use std::{
    fs::File,
    path::{Path, PathBuf},
};

pub struct FallbackReader {
    map: Mmap,
    path: PathBuf,
}
impl FallbackReader {
    pub fn open(path: impl AsRef<Path>, expected_digest: &str) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).map_err(TriAIError::Io)?;
        let map = unsafe { Mmap::map(&file).map_err(TriAIError::Io)? };
        if map.len() < 4 || &map[..4] != b"GGUF" {
            return Err(TriAIError::Other("fallback is not a GGUF".into()));
        }
        if sha256_of_file(&path).map_err(TriAIError::Io)? != expected_digest {
            return Err(TriAIError::Other("fallback GGUF digest mismatch".into()));
        }
        Ok(Self { map, path })
    }
    pub fn read_span(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        let start = usize::try_from(offset)
            .map_err(|_| TriAIError::Other("fallback offset overflow".into()))?;
        let size = usize::try_from(size)
            .map_err(|_| TriAIError::Other("fallback size overflow".into()))?;
        let end = start
            .checked_add(size)
            .ok_or_else(|| TriAIError::Other("fallback span overflow".into()))?;
        if end > self.map.len() {
            return Err(TriAIError::Other(format!(
                "fallback span exceeds {}",
                self.path.display()
            )));
        }
        Ok(self.map[start..end].to_vec())
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
pub fn reconstruct_chunk(
    reader: &FallbackReader,
    index: &TensorIndex,
    names: &[String],
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for name in names {
        let tensor = index
            .tensors
            .iter()
            .find(|t| &t.name == name)
            .ok_or_else(|| TriAIError::Other("fallback tensor missing from index".into()))?;
        let offset = tensor
            .data_offset
            .ok_or_else(|| TriAIError::Other("legacy index lacks absolute data offset".into()))?;
        out.extend(reader.read_span(offset, tensor.size)?);
    }
    Ok(out)
}
