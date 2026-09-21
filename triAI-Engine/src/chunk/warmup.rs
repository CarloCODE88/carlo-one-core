//! D4 deterministic eager-chunk warmup and startup timing metrics.
use super::{ChunkLoader, TensorCategory};
use crate::error::Result;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct ChunkLoadMetric {
    pub chunk_id: String,
    pub categories: Vec<TensorCategory>,
    pub duration: Duration,
    pub bytes: u64,
    pub was_cache_hit: bool,
}
#[derive(Debug, Clone)]
pub struct WarmupReport {
    pub total_duration: Duration,
    pub chunks_loaded: u32,
    pub tensors_covered: u32,
    pub per_chunk: Vec<ChunkLoadMetric>,
    pub cache_hits: u64,
}
impl WarmupReport {
    pub fn total_bytes(&self) -> u64 {
        self.per_chunk.iter().map(|m| m.bytes).sum()
    }
    pub fn avg_chunk_duration(&self) -> Duration {
        if self.per_chunk.is_empty() {
            Duration::ZERO
        } else {
            self.per_chunk.iter().map(|m| m.duration).sum::<Duration>()
                / self.per_chunk.len() as u32
        }
    }
    pub fn summary(&self) -> String {
        format!(
            "warmup: {} chunks, {} tensors",
            self.chunks_loaded, self.tensors_covered
        )
    }
}
#[derive(Debug, Clone)]
pub struct WarmupConfig {
    pub max_chunks: usize,
    pub strict_order: bool,
    pub critical_only: bool,
}
impl Default for WarmupConfig {
    fn default() -> Self {
        Self {
            max_chunks: 0,
            strict_order: true,
            critical_only: false,
        }
    }
}
impl WarmupConfig {
    pub fn critical_only() -> Self {
        Self {
            max_chunks: 3,
            strict_order: true,
            critical_only: true,
        }
    }
    pub fn eager_all() -> Self {
        Self {
            max_chunks: 0,
            strict_order: false,
            critical_only: false,
        }
    }
}
pub struct WarmupStrategy {
    config: WarmupConfig,
    kernel_worker: Option<crate::chunk::KernelWorker>,
}
impl WarmupStrategy {
    pub fn new(config: WarmupConfig) -> Self {
        Self {
            kernel_worker: None,
            config,
        }
    }
    pub fn with_kernel(mut self, worker: crate::chunk::KernelWorker) -> Self {
        self.kernel_worker = Some(worker);
        self
    }
    pub fn execute(&self, loader: &ChunkLoader) -> Result<WarmupReport> {
        let start = Instant::now();
        let eager = loader.tensor_index().eager_tensors();
        let mut planned: BTreeMap<String, (u8, Vec<TensorCategory>)> = BTreeMap::new();
        for tensor in &eager {
            if let Some(id) = &tensor.chunk_id {
                let entry = planned
                    .entry(id.clone())
                    .or_insert((tensor.category.warmup_priority(), Vec::new()));
                entry.0 = entry.0.min(tensor.category.warmup_priority());
                if !entry.1.contains(&tensor.category) {
                    entry.1.push(tensor.category);
                }
            }
        }
        let mut work: Vec<_> = planned.into_iter().collect();
        work.sort_by(|a, b| a.1 .0.cmp(&b.1 .0).then_with(|| a.0.cmp(&b.0)));
        let _chunks_to_load = if self.config.critical_only {
            work.truncate(3);
            3
        } else if self.config.max_chunks > 0 {
            work.truncate(self.config.max_chunks);
            self.config.max_chunks
        } else {
            work.len()
        };
        if self.config.strict_order {
            work.sort_by(|a, b| a.0.cmp(&b.0));
        }
        if let Some(worker) = &self.kernel_worker {
            worker.record_access("eager");
        }
        let before = loader.cache_stats().hits;
        let mut per_chunk = Vec::new();
        for (id, (_, categories)) in work {
            let stamp = Instant::now();
            let pre = loader.cache_stats().hits;
            let chunk = loader.load_chunk(&id)?;
            let hit = loader.cache_stats().hits > pre;
            per_chunk.push(ChunkLoadMetric {
                chunk_id: id,
                categories,
                duration: stamp.elapsed(),
                bytes: chunk.memory_bytes(),
                was_cache_hit: hit,
            });
        }
        let chunks_loaded = per_chunk.len() as u32;
        Ok(WarmupReport {
            total_duration: start.elapsed(),
            chunks_loaded,
            tensors_covered: eager.len() as u32,
            per_chunk,
            cache_hits: loader.cache_stats().hits - before,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_helpers_handle_empty() {
        let r = WarmupReport {
            total_duration: Duration::ZERO,
            chunks_loaded: 0,
            tensors_covered: 0,
            per_chunk: vec![],
            cache_hits: 0,
        };
        assert_eq!(r.avg_chunk_duration(), Duration::ZERO);
        assert_eq!(r.total_bytes(), 0);
        assert!(r.summary().contains("warmup:"));
    }
}
