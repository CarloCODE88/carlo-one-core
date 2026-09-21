// src/evidence/types.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single inference run. NEVER contains raw prompts, keys, or tool outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PromptRun {
    pub run_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub prompt_hmac: String, // SHA-256 HMAC of the prompt
    pub model_id: String,
    pub model_digest: String, // SHA-256 of the GGUF/model file
    pub knowledge_area: String,
    pub task_type: String,
    pub tools: Vec<String>,
    pub context_quality: ContextQuality,
    pub context_tokens: u32,
    pub requested_quality: QualityLevel,
    pub latency_budget_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextQuality {
    RepositoryGrounded,
    FileLocal,
    Global,
    Empty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityLevel {
    High,
    Medium,
    Low,
    Draft,
}

/// Tracks memory/PCIe transfers during chunk loading or KV cache moves.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TransferSample {
    pub run_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub kind: TransferKind,
    pub bytes: u64,
    pub elapsed_us: u64,
    pub queued_us: u64,
    pub stall_us: u64,
    pub source_region: MemoryRegion,
    pub destination_region: MemoryRegion,
    pub chunk_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    HostToDevice,
    DeviceToHost,
    SsdToRam,
    RamToPinned,
    PinnedToVram,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRegion {
    Ssd,
    Ram,
    PinnedRam,
    Vram,
    Evicted,
}

/// Snapshot of where model layers/experts reside at a given moment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResidencySample {
    pub run_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub layer_id: u32,
    pub expert_id: Option<u32>,
    pub state: MemoryRegion,
    pub hit: bool,
    pub reuse_distance: u32,
    pub vram_mb: f64,
    pub pinned_ram_mb: f64,
    pub ram_mb: f64,
    pub ssd_bytes: u64,
}

/// Final metrics of a completed inference run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Outcome {
    pub run_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_per_second: f64,
    pub first_token_ms: u32,
    pub p95_latency_ms: u32,
    pub tool_calls: u32,
    pub retry_count: u32,
    pub quality_score: f64,
    pub user_correction: bool,
    pub error_code: Option<String>,
}

/// Self-optimization policy proposed by the analyst or promoted to active.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Policy {
    pub policy_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub model_id: String,
    pub flags: Vec<String>,
    pub prefetch_depth: u32,
    pub cache_budget_mb: u32,
    pub cpu_threads: u32,
    pub confidence: f64,
    pub parent_policy_id: Option<Uuid>,
}
