//! Bounded, read-only GGUF v2/v3 header parser.
//!
//! The parser deliberately does not infer quantized tensor byte sizes. Tensor
//! spans are derived from validated offsets, which keeps the index faithful to
//! the source file and safe for later chunking.

use crate::error::{Result, TriAIError};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufReader, Read, Seek},
    path::Path,
};

const MAGIC: &[u8; 4] = b"GGUF";
const MAX_ENTRIES: u64 = 1_000_000;
const MAX_STRING_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DIMS: u32 = 8;
const ALIGNMENT: u64 = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GgufTensorInfo {
    pub name: String,
    pub dims: Vec<u64>,
    pub dtype: u32,
    /// Offset from the aligned GGUF data section.
    pub offset: u64,
    /// Exact span up to the next tensor or end of file; may include padding.
    pub size: u64,
    pub data_offset: u64,
}

#[derive(Debug, Clone)]
pub struct GgufMetadata {
    pub version: u32,
    pub tensor_count: u64,
    pub metadata_kv_count: u64,
    /// Scalar values helpful for identification; complex values are recorded as
    /// their type marker after being safely skipped.
    pub kv: BTreeMap<String, String>,
}

pub struct GgufReader {
    file: BufReader<File>,
    file_len: u64,
    data_section_start: u64,
}

impl GgufReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path).map_err(TriAIError::Io)?;
        let file_len = file.metadata().map_err(TriAIError::Io)?.len();
        Ok(Self {
            file: BufReader::new(file),
            file_len,
            data_section_start: 0,
        })
    }

    pub fn parse_header(&mut self) -> Result<(GgufMetadata, Vec<GgufTensorInfo>)> {
        self.file.rewind().map_err(TriAIError::Io)?;
        let mut magic = [0; 4];
        self.file.read_exact(&mut magic).map_err(TriAIError::Io)?;
        if &magic != MAGIC {
            return Err(invalid("invalid GGUF magic"));
        }
        let version = self.u32()?;
        if !(2..=3).contains(&version) {
            return Err(invalid("unsupported GGUF version"));
        }
        let tensor_count = self.u64()?;
        let metadata_kv_count = self.u64()?;
        if tensor_count > MAX_ENTRIES || metadata_kv_count > MAX_ENTRIES {
            return Err(invalid("GGUF entry count exceeds limit"));
        }

        let mut kv = BTreeMap::new();
        for _ in 0..metadata_kv_count {
            let key = self.string()?;
            let value_type = self.u32()?;
            let value = self.value(value_type, 0)?;
            kv.insert(key, value);
        }

        let mut tensors = Vec::with_capacity(
            usize::try_from(tensor_count)
                .map_err(|_| invalid("tensor count overflows platform"))?,
        );
        for _ in 0..tensor_count {
            let name = self.string()?;
            let dimensions = self.u32()?;
            if dimensions > MAX_DIMS {
                return Err(invalid("tensor dimension count exceeds limit"));
            }
            let mut dims = Vec::with_capacity(dimensions as usize);
            for _ in 0..dimensions {
                dims.push(self.u64()?);
            }
            let dtype = self.u32()?;
            let offset = self.u64()?;
            tensors.push(GgufTensorInfo {
                name,
                dims,
                dtype,
                offset,
                size: 0,
                data_offset: 0,
            });
        }
        let header_end = self.file.stream_position().map_err(TriAIError::Io)?;
        self.data_section_start = align_up(header_end, ALIGNMENT)
            .ok_or_else(|| invalid("GGUF header alignment overflow"))?;
        if self.data_section_start > self.file_len {
            return Err(invalid("GGUF data section lies beyond file"));
        }

        // GGUF offsets must be strictly increasing: this permits exact bounded
        // spans without guessing quantization block layouts.
        let mut previous = None;
        for tensor in &tensors {
            if let Some(last) = previous {
                if tensor.offset <= last {
                    return Err(invalid("tensor offsets are not strictly increasing"));
                }
            }
            previous = Some(tensor.offset);
            let absolute = self
                .data_section_start
                .checked_add(tensor.offset)
                .ok_or_else(|| invalid("tensor offset overflow"))?;
            if absolute > self.file_len {
                return Err(invalid("tensor offset lies beyond file"));
            }
        }
        for index in 0..tensors.len() {
            let end_relative = tensors
                .get(index + 1)
                .map(|next| next.offset)
                .unwrap_or(self.file_len - self.data_section_start);
            let size = end_relative
                .checked_sub(tensors[index].offset)
                .ok_or_else(|| invalid("invalid tensor span"))?;
            tensors[index].data_offset = self.data_section_start + tensors[index].offset;
            tensors[index].size = size;
        }
        Ok((
            GgufMetadata {
                version,
                tensor_count,
                metadata_kv_count,
                kv,
            },
            tensors,
        ))
    }

    pub fn data_section_start(&self) -> u64 {
        self.data_section_start
    }

    fn u32(&mut self) -> Result<u32> {
        let mut b = [0; 4];
        self.file.read_exact(&mut b).map_err(TriAIError::Io)?;
        Ok(u32::from_le_bytes(b))
    }
    fn u64(&mut self) -> Result<u64> {
        let mut b = [0; 8];
        self.file.read_exact(&mut b).map_err(TriAIError::Io)?;
        Ok(u64::from_le_bytes(b))
    }
    fn string(&mut self) -> Result<String> {
        let len = self.u64()?;
        if len > MAX_STRING_BYTES {
            return Err(invalid("GGUF string exceeds limit"));
        }
        let mut bytes = vec![0; len as usize];
        self.file.read_exact(&mut bytes).map_err(TriAIError::Io)?;
        String::from_utf8(bytes).map_err(|_| invalid("GGUF string is not UTF-8"))
    }
    fn discard(&mut self, bytes: u64) -> Result<()> {
        let position = self.file.stream_position().map_err(TriAIError::Io)?;
        let end = position
            .checked_add(bytes)
            .ok_or_else(|| invalid("GGUF value overflow"))?;
        if end > self.file_len {
            return Err(invalid("GGUF value exceeds file"));
        }
        self.file
            .seek_relative(i64::try_from(bytes).map_err(|_| invalid("GGUF value too large"))?)
            .map_err(TriAIError::Io)?;
        Ok(())
    }
    fn value(&mut self, ty: u32, depth: u8) -> Result<String> {
        if depth > 4 {
            return Err(invalid("GGUF metadata nesting exceeds limit"));
        }
        match ty {
            0 => {
                self.discard(1)?;
                Ok("uint8".into())
            }
            1 => {
                self.discard(1)?;
                Ok("int8".into())
            }
            2 => {
                self.discard(2)?;
                Ok("uint16".into())
            }
            3 => {
                self.discard(2)?;
                Ok("int16".into())
            }
            4 => Ok(self.u32()?.to_string()),
            5 => {
                self.discard(4)?;
                Ok("int32".into())
            }
            6 => {
                self.discard(4)?;
                Ok("float32".into())
            }
            7 => {
                self.discard(1)?;
                Ok("bool".into())
            }
            8 => self.string(),
            9 => {
                let element_type = self.u32()?;
                let count = self.u64()?;
                if count > MAX_ENTRIES || element_type == 9 {
                    return Err(invalid("invalid GGUF array"));
                }
                for _ in 0..count {
                    self.value(element_type, depth + 1)?;
                }
                Ok(format!("array({count})"))
            }
            10 => Ok(self.u64()?.to_string()),
            11 | 12 => {
                self.discard(8)?;
                Ok("scalar64".into())
            }
            _ => Err(invalid("unknown GGUF metadata type")),
        }
    }
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|v| v / alignment * alignment)
}
fn invalid(message: impl Into<String>) -> TriAIError {
    TriAIError::Other(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;
    fn u32(out: &mut Vec<u8>, value: u32) {
        out.extend(value.to_le_bytes());
    }
    fn u64(out: &mut Vec<u8>, value: u64) {
        out.extend(value.to_le_bytes());
    }
    fn string(out: &mut Vec<u8>, value: &str) {
        u64(out, value.len() as u64);
        out.extend(value.as_bytes());
    }
    fn fixture(offsets: &[u64]) -> NamedTempFile {
        let mut bytes = b"GGUF".to_vec();
        u32(&mut bytes, 3);
        u64(&mut bytes, offsets.len() as u64);
        u64(&mut bytes, 1);
        string(&mut bytes, "general.name");
        u32(&mut bytes, 8);
        string(&mut bytes, "fixture");
        for (index, offset) in offsets.iter().enumerate() {
            string(&mut bytes, &format!("blk.{index}.attn_q.weight"));
            u32(&mut bytes, 1);
            u64(&mut bytes, 4);
            u32(&mut bytes, 0);
            u64(&mut bytes, *offset);
        }
        bytes.resize(align_up(bytes.len() as u64, 32).unwrap() as usize, 0);
        bytes.resize(bytes.len() + 128, 0);
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&bytes).unwrap();
        file.flush().unwrap();
        file
    }
    #[test]
    fn parses_header_and_exact_offset_spans() {
        let file = fixture(&[0, 64]);
        let mut reader = GgufReader::open(file.path()).unwrap();
        let (metadata, tensors) = reader.parse_header().unwrap();
        assert_eq!(metadata.kv["general.name"], "fixture");
        assert_eq!(tensors.len(), 2);
        assert_eq!(tensors[0].size, 64);
        assert_eq!(tensors[1].size, 64);
    }
    #[test]
    fn rejects_invalid_magic_and_offset_order() {
        let mut bad = NamedTempFile::new().unwrap();
        bad.write_all(b"NOPE").unwrap();
        assert!(GgufReader::open(bad.path())
            .unwrap()
            .parse_header()
            .is_err());
        let file = fixture(&[64, 0]);
        assert!(GgufReader::open(file.path())
            .unwrap()
            .parse_header()
            .is_err());
    }
    #[test]
    fn file_hash_is_stable() {
        let file = fixture(&[0]);
        let first = crate::chunk::index::sha256_of_file(file.path()).unwrap();
        let second = crate::chunk::index::sha256_of_file(file.path()).unwrap();
        assert_eq!(first, second);
    }
}
