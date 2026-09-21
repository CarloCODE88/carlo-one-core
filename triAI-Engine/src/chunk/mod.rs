//! Read-only foundations for tensor-aware GGUF chunking.
//!
//! D1 parses and indexes model headers only. It never rewrites GGUF files or
//! loads tensor payloads into memory.

pub mod classify;
pub mod gguf;
pub mod index;
pub mod loader;
pub mod packer;
pub mod rollback;
pub mod warmup;

pub use classify::{classify_tensor, TensorCategory};
pub use gguf::{GgufMetadata, GgufReader, GgufTensorInfo};
pub use index::{sha256_of_file, ChunkDescriptor, ChunkManifest, TensorIndex};
pub use loader::{CacheStats, ChunkLoader, LoadedChunk, LoaderConfig};
pub use crate::kernel_worker::{KernelWorker, MemoryPressure, KernelReport, GpuMemoryInfo};
pub use packer::{pack_gguf, verify_archive, PackResult, PackerConfig};
pub use rollback::{reconstruct_chunk, FallbackReader};
pub use warmup::{ChunkLoadMetric, WarmupConfig, WarmupReport, WarmupStrategy};
